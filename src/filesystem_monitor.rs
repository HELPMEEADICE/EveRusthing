use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};

const BUFFER_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeAction {
    Added,
    Removed,
    Modified,
    RenamedOld,
    RenamedNew,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    pub action: ChangeAction,
    pub path: PathBuf,
}

pub fn spawn(
    roots: impl IntoIterator<Item = String>,
    stop: Arc<AtomicBool>,
    sender: Sender<Result<Vec<FileChange>, String>>,
) {
    for root in roots {
        let stop = Arc::clone(&stop);
        let sender = sender.clone();
        thread::Builder::new()
            .name(format!("directory-monitor-{root}"))
            .spawn(move || watch_root(root, &stop, &sender))
            .expect("spawn directory monitor thread");
    }
}

fn watch_root(root: String, stop: &AtomicBool, sender: &Sender<Result<Vec<FileChange>, String>>) {
    let root_wide = wide_null(&root);
    let handle = unsafe {
        CreateFileW(
            root_wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let _ = sender.send(Err(format!(
            "watch {root} failed with Windows error {}",
            unsafe { GetLastError() }
        )));
        return;
    }

    let mut buffer = vec![0u8; BUFFER_SIZE];
    while !stop.load(Ordering::Acquire) {
        let mut returned = 0;
        let success = unsafe {
            ReadDirectoryChangesW(
                handle,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_CREATION,
                &mut returned,
                null_mut(),
                None,
            )
        };
        if success == 0 {
            let _ = sender.send(Err(format!(
                "watch {root} failed with Windows error {}",
                unsafe { GetLastError() }
            )));
            break;
        }
        if returned != 0 {
            match parse_changes(&root, &buffer[..returned as usize]) {
                Ok(changes) if !changes.is_empty() => {
                    if sender.send(Ok(changes)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    }
    unsafe { CloseHandle(handle) };
}

fn parse_changes(root: &str, bytes: &[u8]) -> Result<Vec<FileChange>, String> {
    let mut changes = Vec::new();
    let mut offset = 0;
    loop {
        if bytes.len().saturating_sub(offset) < 12 {
            return Err("directory notification is truncated".into());
        }
        let next = read_u32(bytes, offset)? as usize;
        let action = match read_u32(bytes, offset + 4)? {
            FILE_ACTION_ADDED => ChangeAction::Added,
            FILE_ACTION_REMOVED => ChangeAction::Removed,
            FILE_ACTION_MODIFIED => ChangeAction::Modified,
            FILE_ACTION_RENAMED_OLD_NAME => ChangeAction::RenamedOld,
            FILE_ACTION_RENAMED_NEW_NAME => ChangeAction::RenamedNew,
            _ => ChangeAction::Modified,
        };
        let name_bytes = read_u32(bytes, offset + 8)? as usize;
        if !name_bytes.is_multiple_of(2) || offset + 12 + name_bytes > bytes.len() {
            return Err("directory notification has an invalid filename".into());
        }
        let name = bytes[offset + 12..offset + 12 + name_bytes]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        let name = String::from_utf16_lossy(&name.collect::<Vec<_>>());
        changes.push(FileChange {
            action,
            path: PathBuf::from(root).join(name),
        });
        if next == 0 {
            return Ok(changes);
        }
        if next < 12 || offset + next >= bytes.len() {
            return Err("directory notification has an invalid next offset".into());
        }
        offset += next;
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let bytes = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "directory notification is truncated".to_owned())?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime};

    #[test]
    fn reports_real_create_rename_and_delete_events() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("everusthing-watch-{unique}"));
        fs::create_dir(&root).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        spawn(
            [root.to_string_lossy().into_owned()],
            Arc::clone(&stop),
            sender,
        );
        thread::sleep(Duration::from_millis(100));

        let old = root.join("old.txt");
        let new = root.join("new.txt");
        fs::write(&old, b"created").unwrap();
        fs::rename(&old, &new).unwrap();
        fs::remove_file(&new).unwrap();

        let mut actions = Vec::new();
        for _ in 0..10 {
            let changes = receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
            actions.extend(
                changes
                    .into_iter()
                    .map(|change| (change.action, change.path)),
            );
            if actions.iter().any(|(action, path)| {
                *action == ChangeAction::Removed && path.file_name().unwrap() == "new.txt"
            }) {
                break;
            }
        }
        assert!(actions.iter().any(|(action, path)| {
            *action == ChangeAction::Added && path.file_name().unwrap() == "old.txt"
        }));
        assert!(actions.iter().any(|(action, path)| {
            *action == ChangeAction::RenamedNew && path.file_name().unwrap() == "new.txt"
        }));
        assert!(actions.iter().any(|(action, path)| {
            *action == ChangeAction::Removed && path.file_name().unwrap() == "new.txt"
        }));

        stop.store(true, Ordering::Release);
        fs::write(root.join("wake"), b"").unwrap();
        thread::sleep(Duration::from_millis(50));
        fs::remove_dir_all(root).unwrap();
    }
}
