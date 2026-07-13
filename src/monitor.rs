use std::collections::HashMap;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
};

use crate::database::{self, DatabaseSnapshot, VolumeCheckpoint};
use crate::filesystem_monitor::{self, ChangeAction, FileChange};
use crate::index::{FileId, IndexRecord, SharedIndex};
use crate::ntfs::NtfsVolume;
use crate::service;
use crate::service_protocol::VolumeReply;

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DATABASE_WRITE_INTERVAL: Duration = Duration::from_secs(30);

pub enum MonitorEvent {
    Updated(Vec<crate::FileRecord>),
    Error(String),
}

pub struct Monitor {
    stop: Arc<AtomicBool>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

pub fn start(
    index: SharedIndex,
    checkpoints: Vec<VolumeCheckpoint>,
    pipe_name: String,
    database_path: PathBuf,
    use_database: bool,
    notify: impl Fn(MonitorEvent) + Send + 'static,
) -> Monitor {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    thread::Builder::new()
        .name("ntfs-monitor".into())
        .spawn(move || {
            if let Err(error) = monitor_loop(
                &index,
                checkpoints,
                &pipe_name,
                &database_path,
                use_database,
                &worker_stop,
                &notify,
            ) && !worker_stop.load(Ordering::Acquire)
            {
                notify(MonitorEvent::Error(error));
            }
        })
        .expect("spawn NTFS monitor thread");
    Monitor { stop }
}

fn monitor_loop(
    index: &SharedIndex,
    checkpoints: Vec<VolumeCheckpoint>,
    pipe_name: &str,
    database_path: &std::path::Path,
    use_database: bool,
    stop: &Arc<AtomicBool>,
    notify: &impl Fn(MonitorEvent),
) -> Result<(), String> {
    match open_direct_volumes(checkpoints.clone()) {
        Ok(volumes) => monitor_direct(index, volumes, database_path, use_database, stop, notify),
        Err(error) => {
            if error.is_access_denied() && service::ping(pipe_name).is_ok() {
                monitor_service(
                    index,
                    checkpoints,
                    pipe_name,
                    database_path,
                    use_database,
                    stop,
                    notify,
                )
            } else {
                monitor_filesystem(
                    index,
                    checkpoints,
                    database_path,
                    use_database,
                    stop,
                    notify,
                )
            }
        }
    }
}

fn open_direct_volumes(
    checkpoints: Vec<VolumeCheckpoint>,
) -> Result<Vec<(NtfsVolume, VolumeCheckpoint)>, crate::ntfs::NtfsError> {
    checkpoints
        .into_iter()
        .map(|checkpoint| {
            let volume = NtfsVolume::open(&checkpoint.root)?;
            if volume.info().volume_serial != checkpoint.volume_serial {
                return Err(monitor_error("NTFS volume serial changed"));
            }
            let (journal_id, next_usn) = volume.journal_state()?;
            if journal_id != checkpoint.journal_id || checkpoint.next_usn > next_usn {
                return Err(monitor_error("USN journal checkpoint is no longer valid"));
            }
            Ok((volume, checkpoint))
        })
        .collect()
}

fn monitor_direct(
    index: &SharedIndex,
    mut volumes: Vec<(NtfsVolume, VolumeCheckpoint)>,
    database_path: &std::path::Path,
    use_database: bool,
    stop: &AtomicBool,
    notify: &impl Fn(MonitorEvent),
) -> Result<(), String> {
    let mut last_database_write = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let mut changed = false;
        for (volume, checkpoint) in &mut volumes {
            loop {
                let batch = volume
                    .read_changes(checkpoint.next_usn, checkpoint.journal_id)
                    .map_err(|error| error.to_string())?;
                let done = batch.records.is_empty() || batch.next_usn == checkpoint.next_usn;
                changed |= !batch.records.is_empty();
                checkpoint.next_usn = batch.next_usn;
                volume.apply_batch(index, &batch);
                if done || stop.load(Ordering::Acquire) {
                    break;
                }
            }
        }
        publish_if_changed(
            index,
            &volumes
                .iter()
                .map(|(_, checkpoint)| checkpoint.clone())
                .collect::<Vec<_>>(),
            changed,
            database_path,
            use_database,
            &mut last_database_write,
            notify,
        );
        interruptible_sleep(stop);
    }
    Ok(())
}

fn monitor_service(
    index: &SharedIndex,
    mut checkpoints: Vec<VolumeCheckpoint>,
    pipe_name: &str,
    database_path: &std::path::Path,
    use_database: bool,
    stop: &AtomicBool,
    notify: &impl Fn(MonitorEvent),
) -> Result<(), String> {
    let mut last_database_write = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let mut changed = false;
        for checkpoint in &mut checkpoints {
            let previous_usn = checkpoint.next_usn;
            let reply = service::catch_up_volume(pipe_name, index, &checkpoint_reply(checkpoint))
                .map_err(|error| error.to_string())?;
            checkpoint.next_usn = reply.next_usn;
            changed |= checkpoint.next_usn != previous_usn;
        }
        publish_if_changed(
            index,
            &checkpoints,
            changed,
            database_path,
            use_database,
            &mut last_database_write,
            notify,
        );
        interruptible_sleep(stop);
    }
    Ok(())
}

fn monitor_filesystem(
    index: &SharedIndex,
    checkpoints: Vec<VolumeCheckpoint>,
    database_path: &std::path::Path,
    use_database: bool,
    stop: &Arc<AtomicBool>,
    notify: &impl Fn(MonitorEvent),
) -> Result<(), String> {
    let mut paths = HashMap::new();
    for record in index.snapshot_unsorted() {
        if let (Some(volume_serial), Some(file_reference)) =
            (record.volume_serial.get(), record.file_reference.get())
        {
            paths.insert(
                path_key(std::path::Path::new(&record.path)),
                FileId {
                    volume_serial,
                    file_reference,
                },
            );
        }
    }
    let (sender, receiver) = mpsc::channel();
    filesystem_monitor::spawn(
        checkpoints.iter().map(|volume| volume.root.clone()),
        Arc::clone(stop),
        sender,
    );
    let mut last_database_write = Instant::now();
    let mut pending_rename = None;
    while !stop.load(Ordering::Acquire) {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(changes)) => {
                let changed =
                    apply_filesystem_changes(index, &mut paths, &mut pending_rename, changes);
                publish_if_changed(
                    index,
                    &checkpoints,
                    changed,
                    database_path,
                    use_database,
                    &mut last_database_write,
                    notify,
                );
            }
            Ok(Err(error)) => notify(MonitorEvent::Error(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("all directory monitors stopped".into());
            }
        }
    }
    Ok(())
}

fn apply_filesystem_changes(
    index: &SharedIndex,
    paths: &mut HashMap<String, FileId>,
    pending_rename: &mut Option<(String, FileId)>,
    changes: Vec<FileChange>,
) -> bool {
    let mut changed = false;
    for change in changes {
        let key = path_key(&change.path);
        match change.action {
            ChangeAction::RenamedOld => {
                if let Some(id) = paths.get(&key).copied() {
                    *pending_rename = Some((key, id));
                }
            }
            ChangeAction::Removed => {
                changed |= remove_path_and_descendants(index, paths, &key);
            }
            ChangeAction::Added | ChangeAction::Modified | ChangeAction::RenamedNew => {
                let renamed = if change.action == ChangeAction::RenamedNew {
                    pending_rename.take()
                } else {
                    None
                };
                if let Some(mut record) = record_from_path(&change.path, paths) {
                    if let Some((old_key, old_id)) = renamed {
                        record.id = old_id;
                        rename_path_and_descendants(paths, &old_key, &key);
                    }
                    paths.insert(key, record.id);
                    index.upsert(record);
                    changed = true;
                }
            }
        }
    }
    changed
}

fn record_from_path(
    path: &std::path::Path,
    paths: &HashMap<String, FileId>,
) -> Option<IndexRecord> {
    let parent = path
        .parent()
        .and_then(|parent| paths.get(&path_key(parent)))?;
    let info = file_information(path)?;
    Some(IndexRecord {
        id: file_id(&info),
        parent_reference: parent.file_reference,
        name: path.file_name()?.to_string_lossy().into_owned(),
        size: Some(((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64).into(),
        date_modified: Some(file_time_value(info.ftLastWriteTime)).into(),
        date_created: Some(file_time_value(info.ftCreationTime)).into(),
        attributes: info.dwFileAttributes,
    })
}

fn file_information(path: &std::path::Path) -> Option<BY_HANDLE_FILE_INFORMATION> {
    let path_wide = wide_null(&path.to_string_lossy());
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let success = unsafe { GetFileInformationByHandle(handle, &mut info) };
    unsafe { CloseHandle(handle) };
    if success == 0 {
        return None;
    }
    Some(info)
}

fn file_id(info: &BY_HANDLE_FILE_INFORMATION) -> FileId {
    FileId {
        volume_serial: info.dwVolumeSerialNumber as u64,
        file_reference: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    }
}

fn remove_path_and_descendants(
    index: &SharedIndex,
    paths: &mut HashMap<String, FileId>,
    key: &str,
) -> bool {
    let prefix = format!("{key}\\");
    let removed = paths
        .iter()
        .filter(|(path, _)| path.as_str() == key || path.starts_with(&prefix))
        .map(|(path, id)| (path.clone(), *id))
        .collect::<Vec<_>>();
    for (path, id) in &removed {
        paths.remove(path);
        index.remove(*id);
    }
    !removed.is_empty()
}

fn rename_path_and_descendants(paths: &mut HashMap<String, FileId>, old: &str, new: &str) {
    let prefix = format!("{old}\\");
    let renamed = paths
        .iter()
        .filter(|(path, _)| path.as_str() == old || path.starts_with(&prefix))
        .map(|(path, id)| (path.clone(), *id))
        .collect::<Vec<_>>();
    for (path, id) in renamed {
        paths.remove(&path);
        paths.insert(format!("{new}{}", &path[old.len()..]), id);
    }
}

fn path_key(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn file_time_value(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn publish_if_changed(
    index: &SharedIndex,
    checkpoints: &[VolumeCheckpoint],
    changed: bool,
    database_path: &std::path::Path,
    use_database: bool,
    last_database_write: &mut Instant,
    notify: &impl Fn(MonitorEvent),
) {
    if !changed {
        return;
    }
    let mut snapshot = DatabaseSnapshot {
        records: index.snapshot_unsorted(),
        volumes: checkpoints.to_vec(),
    };
    database::sort_records(&mut snapshot.records);
    let should_write = use_database && last_database_write.elapsed() >= DATABASE_WRITE_INTERVAL;
    if should_write {
        *last_database_write = Instant::now();
    }
    let write_error = should_write
        .then(|| database::write(database_path, &snapshot))
        .transpose()
        .err();
    notify(MonitorEvent::Updated(snapshot.records));
    if let Some(error) = write_error {
        notify(MonitorEvent::Error(format!(
            "save database failed: {error}"
        )));
    }
}

fn checkpoint_reply(checkpoint: &VolumeCheckpoint) -> VolumeReply {
    VolumeReply {
        root: checkpoint.root.clone(),
        volume_serial: checkpoint.volume_serial,
        root_file_reference: checkpoint.root_file_reference,
        journal_id: checkpoint.journal_id,
        next_usn: checkpoint.next_usn,
        record_count: 0,
    }
}

fn interruptible_sleep(stop: &AtomicBool) {
    let iterations = POLL_INTERVAL.as_millis() / STOP_POLL_INTERVAL.as_millis();
    for _ in 0..iterations {
        if stop.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
}

fn monitor_error(detail: impl Into<String>) -> crate::ntfs::NtfsError {
    crate::ntfs::NtfsError {
        operation: "monitor NTFS volume",
        code: 0,
        detail: Some(detail.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::SystemTime;

    #[test]
    fn filesystem_fallback_updates_live_index() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("everusthing-index-watch-{unique}"));
        fs::create_dir(&root).unwrap();
        let root_info = file_information(&root).unwrap();
        let root_id = file_id(&root_info);
        let root_text = root.to_string_lossy().into_owned();
        let index = SharedIndex::default();
        index.register_volume(
            root_id.volume_serial,
            root_text.clone(),
            root_id.file_reference,
        );
        index.upsert(IndexRecord {
            id: root_id,
            parent_reference: root_id.file_reference,
            name: ".".into(),
            size: Some(0).into(),
            date_modified: None.into(),
            date_created: None.into(),
            attributes: root_info.dwFileAttributes,
        });
        let checkpoint = VolumeCheckpoint {
            root: root_text,
            volume_serial: root_id.volume_serial,
            root_file_reference: root_id.file_reference,
            journal_id: 0,
            next_usn: 0,
        };
        let (sender, receiver) = mpsc::channel();
        let monitor = start(
            index,
            vec![checkpoint],
            "missing-test-pipe".into(),
            PathBuf::from("unused.db"),
            false,
            move |event| sender.send(event).unwrap(),
        );
        thread::sleep(Duration::from_millis(100));

        let old = root.join("old.txt");
        let new = root.join("new.txt");
        fs::write(&old, b"created").unwrap();
        wait_for_records(&receiver, |records| {
            records
                .iter()
                .any(|record| record.path.ends_with("old.txt"))
        });
        fs::rename(&old, &new).unwrap();
        wait_for_records(&receiver, |records| {
            records
                .iter()
                .any(|record| record.path.ends_with("new.txt"))
                && !records
                    .iter()
                    .any(|record| record.path.ends_with("old.txt"))
        });
        fs::remove_file(&new).unwrap();
        wait_for_records(&receiver, |records| {
            !records
                .iter()
                .any(|record| record.path.ends_with("new.txt"))
        });

        drop(monitor);
        fs::write(root.join("wake"), b"").unwrap();
        thread::sleep(Duration::from_millis(50));
        fs::remove_dir_all(root).unwrap();
    }

    fn wait_for_records(
        receiver: &mpsc::Receiver<MonitorEvent>,
        predicate: impl Fn(&[crate::FileRecord]) -> bool,
    ) {
        for _ in 0..10 {
            match receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
                MonitorEvent::Updated(records) if predicate(&records) => return,
                MonitorEvent::Updated(_) => {}
                MonitorEvent::Error(error) => panic!("monitor failed: {error}"),
            }
        }
        panic!("timed out waiting for live index update");
    }
}
