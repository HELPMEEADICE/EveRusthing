use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::model::FileRecord;

const MAGIC: &[u8; 8] = b"EVRSTDB\0";
const VERSION: u32 = 2;
const HEADER_SIZE: usize = 28;
const MAX_DATABASE_SIZE: u64 = 16 * 1024 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000_000;
const MAX_STRING_SIZE: usize = 1024 * 1024;
const NONE_U64: u64 = u64::MAX;
const NONE_U32: u32 = u32::MAX;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

#[derive(Debug)]
pub enum DatabaseError {
    Missing,
    Io(io::Error),
    Corrupt(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeCheckpoint {
    pub root: String,
    pub volume_serial: u64,
    pub root_file_reference: u64,
    pub journal_id: u64,
    pub next_usn: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DatabaseSnapshot {
    pub records: Vec<FileRecord>,
    pub volumes: Vec<VolumeCheckpoint>,
}

impl Display for DatabaseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("database does not exist"),
            Self::Io(error) => Display::fmt(error, formatter),
            Self::Corrupt(message) => write!(formatter, "database is corrupt: {message}"),
        }
    }
}

impl std::error::Error for DatabaseError {}

pub fn read(path: &Path) -> Result<DatabaseSnapshot, DatabaseError> {
    let mut file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            DatabaseError::Missing
        } else {
            DatabaseError::Io(error)
        }
    })?;
    let length = file.metadata().map_err(DatabaseError::Io)?.len();
    if length < HEADER_SIZE as u64 || length > MAX_DATABASE_SIZE {
        return Err(DatabaseError::Corrupt("invalid file size"));
    }

    let mut header = [0u8; HEADER_SIZE];
    file.read_exact(&mut header).map_err(DatabaseError::Io)?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(DatabaseError::Corrupt("invalid magic"));
    }
    if u32::from_le_bytes(header[8..12].try_into().unwrap()) != VERSION {
        return Err(DatabaseError::Corrupt("unsupported version"));
    }
    let payload_size = usize::try_from(u64::from_le_bytes(header[12..20].try_into().unwrap()))
        .map_err(|_| DatabaseError::Corrupt("payload is too large"))?;
    let expected_checksum = u64::from_le_bytes(header[20..28].try_into().unwrap());
    if payload_size as u64 != length - HEADER_SIZE as u64 {
        return Err(DatabaseError::Corrupt("payload length mismatch"));
    }
    let mut decoder = Decoder::new(file, payload_size);
    let snapshot = decode_snapshot(&mut decoder)?;
    if decoder.remaining() != 0 {
        return Err(DatabaseError::Corrupt("trailing payload data"));
    }
    if decoder.checksum != expected_checksum {
        return Err(DatabaseError::Corrupt("checksum mismatch"));
    }
    Ok(snapshot)
}

pub fn write(path: &Path, snapshot: &DatabaseSnapshot) -> Result<(), DatabaseError> {
    let temporary = temporary_path(path);
    let result = (|| {
        let file = File::create(&temporary).map_err(DatabaseError::Io)?;
        let mut file = BufWriter::new(file);
        file.write_all(&[0; HEADER_SIZE])
            .map_err(DatabaseError::Io)?;
        let (payload_size, payload_checksum) = {
            let mut encoder = Encoder::new(&mut file);
            encode_snapshot(&mut encoder, snapshot)?;
            (encoder.written, encoder.checksum)
        };
        if payload_size > MAX_DATABASE_SIZE - HEADER_SIZE as u64 {
            return Err(DatabaseError::Corrupt("payload is too large"));
        }
        let mut header = [0u8; HEADER_SIZE];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[12..20].copy_from_slice(&payload_size.to_le_bytes());
        header[20..28].copy_from_slice(&payload_checksum.to_le_bytes());
        file.seek(SeekFrom::Start(0)).map_err(DatabaseError::Io)?;
        file.write_all(&header).map_err(DatabaseError::Io)?;
        file.flush().map_err(DatabaseError::Io)?;
        file.get_ref().sync_all().map_err(DatabaseError::Io)?;
        drop(file);
        replace_file(&temporary, path).map_err(DatabaseError::Io)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn default_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name("EveRusthing.db"))
        .unwrap_or_else(|| PathBuf::from("EveRusthing.db"))
}

pub fn load_or_rebuild(
    path: &Path,
    use_database: bool,
    force_reindex: bool,
    refresh: impl FnOnce(DatabaseSnapshot) -> Result<DatabaseSnapshot, String>,
    build: impl FnOnce() -> Result<DatabaseSnapshot, String>,
) -> Result<Vec<FileRecord>, String> {
    load_snapshot_or_rebuild(path, use_database, force_reindex, refresh, build)
        .map(|snapshot| snapshot.records)
}

pub fn load_snapshot_or_rebuild(
    path: &Path,
    use_database: bool,
    force_reindex: bool,
    refresh: impl FnOnce(DatabaseSnapshot) -> Result<DatabaseSnapshot, String>,
    build: impl FnOnce() -> Result<DatabaseSnapshot, String>,
) -> Result<DatabaseSnapshot, String> {
    if use_database
        && !force_reindex
        && let Ok(snapshot) = read(path)
        && let Ok(mut snapshot) = refresh(snapshot)
    {
        sort_records(&mut snapshot.records);
        write(path, &snapshot).map_err(|error| format!("save database failed: {error}"))?;
        return Ok(snapshot);
    }

    let mut snapshot = build()?;
    sort_records(&mut snapshot.records);
    if use_database {
        write(path, &snapshot).map_err(|error| format!("save database failed: {error}"))?;
    }
    Ok(snapshot)
}

#[cfg(windows)]
pub fn load_local(
    path: &Path,
    pipe_name: &str,
    use_database: bool,
    force_reindex: bool,
) -> Result<Vec<FileRecord>, String> {
    load_local_snapshot(path, pipe_name, use_database, force_reindex)
        .map(|snapshot| snapshot.records)
}

#[cfg(windows)]
pub fn load_local_snapshot(
    path: &Path,
    pipe_name: &str,
    use_database: bool,
    force_reindex: bool,
) -> Result<DatabaseSnapshot, String> {
    load_snapshot_or_rebuild(
        path,
        use_database,
        force_reindex,
        |snapshot| match refresh_direct(snapshot.clone()) {
            Ok(snapshot) => Ok(snapshot),
            Err(error) if error.is_access_denied() => {
                refresh_through_service(snapshot.clone(), pipe_name).or(Ok(snapshot))
            }
            Err(error) => Err(error.to_string()),
        },
        || match build_direct() {
            Ok(snapshot) => Ok(snapshot),
            Err(error) if error.is_access_denied() => build_through_service(pipe_name),
            Err(error) => Err(error.to_string()),
        },
    )
}

#[cfg(windows)]
fn refresh_direct(
    mut snapshot: DatabaseSnapshot,
) -> Result<DatabaseSnapshot, crate::ntfs::NtfsError> {
    use crate::index::SharedIndex;
    use crate::ntfs::{NtfsVolume, discover_ntfs_volumes};

    let roots = discover_ntfs_volumes()?;
    ensure_same_roots(&roots, &snapshot.volumes)?;
    let index = SharedIndex::restore(
        &snapshot.records,
        snapshot.volumes.iter().map(|volume| {
            (
                volume.volume_serial,
                volume.root.clone(),
                volume.root_file_reference,
            )
        }),
    )
    .map_err(stale_database_error)?;

    for checkpoint in &mut snapshot.volumes {
        let volume = NtfsVolume::open(&checkpoint.root)?;
        if volume.info().volume_serial != checkpoint.volume_serial {
            return Err(stale_database_error("NTFS volume serial changed"));
        }
        let (journal_id, journal_next_usn) = volume.journal_state()?;
        if journal_id != checkpoint.journal_id || checkpoint.next_usn > journal_next_usn {
            return Err(stale_database_error(
                "USN journal checkpoint is no longer valid",
            ));
        }
        checkpoint.next_usn =
            volume.catch_up(&index, checkpoint.next_usn, checkpoint.journal_id)?;
    }
    snapshot.records = index.snapshot_unsorted();
    Ok(snapshot)
}

#[cfg(windows)]
fn stale_database_error(detail: impl Into<String>) -> crate::ntfs::NtfsError {
    crate::ntfs::NtfsError {
        operation: "refresh cached database",
        code: 0,
        detail: Some(detail.into()),
    }
}

#[cfg(windows)]
fn refresh_through_service(
    mut snapshot: DatabaseSnapshot,
    pipe_name: &str,
) -> Result<DatabaseSnapshot, String> {
    use crate::index::SharedIndex;
    use crate::ntfs::discover_ntfs_volumes;
    use crate::service;
    use crate::service_protocol::VolumeReply;

    let roots = discover_ntfs_volumes().map_err(|error| error.to_string())?;
    ensure_same_roots(&roots, &snapshot.volumes).map_err(|error| error.to_string())?;
    let index = SharedIndex::restore(
        &snapshot.records,
        snapshot.volumes.iter().map(|volume| {
            (
                volume.volume_serial,
                volume.root.clone(),
                volume.root_file_reference,
            )
        }),
    )
    .map_err(str::to_owned)?;

    for checkpoint in &mut snapshot.volumes {
        let reply = service::catch_up_volume(
            pipe_name,
            &index,
            &VolumeReply {
                root: checkpoint.root.clone(),
                volume_serial: checkpoint.volume_serial,
                root_file_reference: checkpoint.root_file_reference,
                journal_id: checkpoint.journal_id,
                next_usn: checkpoint.next_usn,
                record_count: 0,
            },
        )
        .map_err(|error| error.to_string())?;
        checkpoint.next_usn = reply.next_usn;
    }
    snapshot.records = index.snapshot_unsorted();
    Ok(snapshot)
}

#[cfg(windows)]
fn build_direct() -> Result<DatabaseSnapshot, crate::ntfs::NtfsError> {
    use crate::index::SharedIndex;
    use crate::ntfs::{NtfsVolume, discover_ntfs_volumes};

    let index = SharedIndex::default();
    let mut volumes = Vec::new();
    for root in discover_ntfs_volumes()? {
        let volume = NtfsVolume::open(&root)?;
        let scan = volume.scan_into(&index)?;
        let next_usn = volume.catch_up(&index, scan.next_usn, scan.journal_id)?;
        volumes.push(VolumeCheckpoint {
            root: scan.volume.root,
            volume_serial: scan.volume.volume_serial,
            root_file_reference: scan.root_file_reference,
            journal_id: scan.journal_id,
            next_usn,
        });
    }
    Ok(DatabaseSnapshot {
        records: index.snapshot_unsorted(),
        volumes,
    })
}

#[cfg(windows)]
fn build_through_service(pipe_name: &str) -> Result<DatabaseSnapshot, String> {
    let scan = crate::service::scan_local(pipe_name).map_err(|error| error.to_string())?;
    Ok(DatabaseSnapshot {
        records: scan.records,
        volumes: scan
            .volumes
            .into_iter()
            .map(|volume| VolumeCheckpoint {
                root: volume.root,
                volume_serial: volume.volume_serial,
                root_file_reference: volume.root_file_reference,
                journal_id: volume.journal_id,
                next_usn: volume.next_usn,
            })
            .collect(),
    })
}

#[cfg(windows)]
fn ensure_same_roots(
    roots: &[String],
    volumes: &[VolumeCheckpoint],
) -> Result<(), crate::ntfs::NtfsError> {
    let same = roots.len() == volumes.len()
        && roots.iter().all(|root| {
            volumes
                .iter()
                .any(|volume| volume.root.eq_ignore_ascii_case(root))
        });
    if same {
        Ok(())
    } else {
        Err(crate::ntfs::NtfsError {
            operation: "refresh cached database",
            code: 0,
            detail: Some("indexed NTFS volume set changed".into()),
        })
    }
}

pub fn sort_records(records: &mut [FileRecord]) {
    records.sort_unstable_by(|left, right| {
        compare_ascii_case_insensitive(left.file_name(), right.file_name())
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn encode_snapshot(
    output: &mut Encoder<impl Write>,
    snapshot: &DatabaseSnapshot,
) -> Result<(), DatabaseError> {
    if snapshot.records.len() > MAX_RECORDS {
        return Err(DatabaseError::Corrupt("too many records"));
    }
    if snapshot.volumes.len() > 26 {
        return Err(DatabaseError::Corrupt("too many volumes"));
    }
    output.u32(snapshot.volumes.len() as u32)?;
    for volume in &snapshot.volumes {
        output.u64(volume.volume_serial)?;
        output.u64(volume.root_file_reference)?;
        output.u64(volume.journal_id)?;
        output.i64(volume.next_usn)?;
        output.string(&volume.root)?;
    }
    output.u64(snapshot.records.len() as u64)?;
    for record in &snapshot.records {
        output.u64(record.volume_serial.unwrap_or(NONE_U64))?;
        output.u64(record.file_reference.unwrap_or(NONE_U64))?;
        output.u64(record.parent_reference.unwrap_or(NONE_U64))?;
        output.u64(record.size.unwrap_or(NONE_U64))?;
        output.u64(record.date_modified.unwrap_or(NONE_U64))?;
        output.u64(record.date_created.unwrap_or(NONE_U64))?;
        output.u32(record.attributes.unwrap_or(NONE_U32))?;
        output.string(&record.path)?;
    }
    Ok(())
}

fn decode_snapshot(decoder: &mut Decoder<impl Read>) -> Result<DatabaseSnapshot, DatabaseError> {
    let volume_count = decoder.u32()? as usize;
    if volume_count > 26 || volume_count > decoder.remaining() / 36 {
        return Err(DatabaseError::Corrupt("invalid volume count"));
    }
    let mut volumes = Vec::with_capacity(volume_count);
    for _ in 0..volume_count {
        volumes.push(VolumeCheckpoint {
            volume_serial: decoder.u64()?,
            root_file_reference: decoder.u64()?,
            journal_id: decoder.u64()?,
            next_usn: decoder.i64()?,
            root: decoder.string()?,
        });
    }
    let count = usize::try_from(decoder.u64()?)
        .map_err(|_| DatabaseError::Corrupt("record count is too large"))?;
    if count > MAX_RECORDS || count > decoder.remaining() / 60 {
        return Err(DatabaseError::Corrupt("invalid record count"));
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        records.push(FileRecord {
            volume_serial: optional_u64(decoder.u64()?).into(),
            file_reference: optional_u64(decoder.u64()?).into(),
            parent_reference: optional_u64(decoder.u64()?).into(),
            size: optional_u64(decoder.u64()?).into(),
            date_modified: optional_u64(decoder.u64()?).into(),
            date_created: optional_u64(decoder.u64()?).into(),
            attributes: optional_u32(decoder.u32()?).into(),
            path: decoder.string()?,
            file_list_filename: None,
        });
    }
    Ok(DatabaseSnapshot { records, volumes })
}

pub(crate) fn compare_ascii_case_insensitive(left: &str, right: &str) -> std::cmp::Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    for index in 0..left.len().min(right.len()) {
        let order = left[index]
            .to_ascii_lowercase()
            .cmp(&right[index].to_ascii_lowercase());
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".tmp");
    value.into()
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain([0]).collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

fn optional_u64(value: u64) -> Option<u64> {
    (value != NONE_U64).then_some(value)
}

fn optional_u32(value: u32) -> Option<u32> {
    (value != NONE_U32).then_some(value)
}

struct Encoder<W> {
    writer: W,
    written: u64,
    checksum: u64,
}

impl<W: Write> Encoder<W> {
    fn new(writer: W) -> Self {
        Self {
            writer,
            written: 0,
            checksum: FNV_OFFSET,
        }
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), DatabaseError> {
        self.writer.write_all(bytes).map_err(DatabaseError::Io)?;
        self.written = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or(DatabaseError::Corrupt("payload is too large"))?;
        update_checksum(&mut self.checksum, bytes);
        Ok(())
    }

    fn u32(&mut self, value: u32) -> Result<(), DatabaseError> {
        self.bytes(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), DatabaseError> {
        self.bytes(&value.to_le_bytes())
    }

    fn i64(&mut self, value: i64) -> Result<(), DatabaseError> {
        self.bytes(&value.to_le_bytes())
    }

    fn string(&mut self, value: &str) -> Result<(), DatabaseError> {
        if value.len() > MAX_STRING_SIZE {
            return Err(DatabaseError::Corrupt("path is too long"));
        }
        self.u32(value.len() as u32)?;
        self.bytes(value.as_bytes())
    }
}

struct Decoder<R> {
    reader: R,
    remaining: usize,
    checksum: u64,
}

impl<R: Read> Decoder<R> {
    fn new(reader: R, remaining: usize) -> Self {
        Self {
            reader,
            remaining,
            checksum: FNV_OFFSET,
        }
    }

    fn remaining(&self) -> usize {
        self.remaining
    }

    fn bytes(&mut self, output: &mut [u8]) -> Result<(), DatabaseError> {
        if output.len() > self.remaining {
            return Err(DatabaseError::Corrupt("unexpected end of file"));
        }
        self.reader.read_exact(output).map_err(DatabaseError::Io)?;
        self.remaining -= output.len();
        update_checksum(&mut self.checksum, output);
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, DatabaseError> {
        let mut bytes = [0; 4];
        self.bytes(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, DatabaseError> {
        let mut bytes = [0; 8];
        self.bytes(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn i64(&mut self) -> Result<i64, DatabaseError> {
        let mut bytes = [0; 8];
        self.bytes(&mut bytes)?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn string(&mut self) -> Result<String, DatabaseError> {
        let length = self.u32()? as usize;
        if length > MAX_STRING_SIZE {
            return Err(DatabaseError::Corrupt("path is too long"));
        }
        let mut bytes = vec![0; length];
        self.bytes(&mut bytes)?;
        String::from_utf8(bytes).map_err(|_| DatabaseError::Corrupt("path is not UTF-8"))
    }
}

fn update_checksum(checksum: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *checksum = (*checksum ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "everusthing-{name}-{}-{nonce}.db",
            std::process::id()
        ))
    }

    fn record(path: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            volume_serial: Some(12).into(),
            file_reference: Some(34).into(),
            parent_reference: Some(5).into(),
            size: Some(56).into(),
            date_modified: Some(78).into(),
            date_created: Some(90).into(),
            attributes: Some(0x20).into(),
            file_list_filename: None,
        }
    }

    #[test]
    fn database_round_trip_preserves_records() {
        let path = test_path("round-trip");
        let snapshot = DatabaseSnapshot {
            records: vec![record(r"C:\work\alpha.txt"), record(r"D:\beta.bin")],
            volumes: vec![VolumeCheckpoint {
                root: "C:\\".into(),
                volume_serial: 12,
                root_file_reference: 5,
                journal_id: 99,
                next_usn: 123,
            }],
        };
        write(&path, &snapshot).unwrap();
        assert_eq!(read(&path).unwrap(), snapshot);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_truncated_and_modified_databases() {
        let path = test_path("corrupt");
        write(
            &path,
            &DatabaseSnapshot {
                records: vec![record(r"C:\file.txt")],
                volumes: Vec::new(),
            },
        )
        .unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[HEADER_SIZE] ^= 0x80;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(read(&path), Err(DatabaseError::Corrupt(_))));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn default_sort_is_case_insensitive_name_then_path() {
        let mut records = vec![
            record(r"C:\z\beta.txt"),
            record(r"C:\b\Alpha.txt"),
            record(r"C:\a\alpha.txt"),
        ];
        sort_records(&mut records);
        let paths: Vec<_> = records.iter().map(|record| record.path.as_str()).collect();
        assert_eq!(
            paths,
            [r"C:\a\alpha.txt", r"C:\b\Alpha.txt", r"C:\z\beta.txt"]
        );
    }

    #[test]
    fn cached_database_skips_rebuild_unless_forced() {
        let path = test_path("load-or-rebuild");
        write(
            &path,
            &DatabaseSnapshot {
                records: vec![record(r"C:\cached.txt")],
                volumes: Vec::new(),
            },
        )
        .unwrap();

        let mut builds = 0;
        let cached = load_or_rebuild(&path, true, false, Ok, || {
            builds += 1;
            Ok(DatabaseSnapshot {
                records: vec![record(r"C:\rebuilt.txt")],
                volumes: Vec::new(),
            })
        })
        .unwrap();
        assert_eq!(builds, 0);
        assert_eq!(cached[0].path, r"C:\cached.txt");

        let rebuilt = load_or_rebuild(&path, true, true, Ok, || {
            builds += 1;
            Ok(DatabaseSnapshot {
                records: vec![record(r"C:\rebuilt.txt")],
                volumes: Vec::new(),
            })
        })
        .unwrap();
        assert_eq!(builds, 1);
        assert_eq!(rebuilt[0].path, r"C:\rebuilt.txt");
        assert_eq!(read(&path).unwrap().records, rebuilt);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_cache_refresh_rebuilds_the_database() {
        let path = test_path("stale-cache");
        write(
            &path,
            &DatabaseSnapshot {
                records: vec![record(r"C:\stale.txt")],
                volumes: Vec::new(),
            },
        )
        .unwrap();

        let records = load_or_rebuild(
            &path,
            true,
            false,
            |_| Err("journal changed".into()),
            || {
                Ok(DatabaseSnapshot {
                    records: vec![record(r"C:\fresh.txt")],
                    volumes: Vec::new(),
                })
            },
        )
        .unwrap();

        assert_eq!(records[0].path, r"C:\fresh.txt");
        fs::remove_file(path).unwrap();
    }
}
