use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::model::FileRecord;

const MAGIC: &[u8; 8] = b"EVRSTDB\0";
const VERSION: u32 = 1;
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

pub fn read(path: &Path) -> Result<Vec<FileRecord>, DatabaseError> {
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

    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes).map_err(DatabaseError::Io)?;
    let mut decoder = Decoder::new(&bytes);
    if decoder.take(MAGIC.len())? != MAGIC {
        return Err(DatabaseError::Corrupt("invalid magic"));
    }
    if decoder.u32()? != VERSION {
        return Err(DatabaseError::Corrupt("unsupported version"));
    }
    let payload_size = usize::try_from(decoder.u64()?)
        .map_err(|_| DatabaseError::Corrupt("payload is too large"))?;
    let expected_checksum = decoder.u64()?;
    if payload_size != decoder.remaining() {
        return Err(DatabaseError::Corrupt("payload length mismatch"));
    }
    let payload = decoder.take(payload_size)?;
    if checksum(payload) != expected_checksum {
        return Err(DatabaseError::Corrupt("checksum mismatch"));
    }
    decode_records(payload)
}

pub fn write(path: &Path, records: &[FileRecord]) -> Result<(), DatabaseError> {
    let payload = encode_records(records)?;
    let payload_size =
        u64::try_from(payload.len()).map_err(|_| DatabaseError::Corrupt("payload is too large"))?;
    let mut header = Vec::with_capacity(HEADER_SIZE);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    header.extend_from_slice(&payload_size.to_le_bytes());
    header.extend_from_slice(&checksum(&payload).to_le_bytes());

    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = File::create(&temporary).map_err(DatabaseError::Io)?;
        file.write_all(&header).map_err(DatabaseError::Io)?;
        file.write_all(&payload).map_err(DatabaseError::Io)?;
        file.sync_all().map_err(DatabaseError::Io)?;
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
    build: impl FnOnce() -> Result<Vec<FileRecord>, String>,
) -> Result<Vec<FileRecord>, String> {
    if use_database
        && !force_reindex
        && let Ok(records) = read(path)
    {
        return Ok(records);
    }

    let mut records = build()?;
    sort_records(&mut records);
    if use_database {
        write(path, &records).map_err(|error| format!("save database failed: {error}"))?;
    }
    Ok(records)
}

pub fn sort_records(records: &mut [FileRecord]) {
    records.sort_unstable_by(|left, right| {
        compare_ascii_case_insensitive(left.file_name(), right.file_name())
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn encode_records(records: &[FileRecord]) -> Result<Vec<u8>, DatabaseError> {
    if records.len() > MAX_RECORDS {
        return Err(DatabaseError::Corrupt("too many records"));
    }
    let mut output = Vec::new();
    output.extend_from_slice(&(records.len() as u64).to_le_bytes());
    for record in records {
        output.extend_from_slice(&record.volume_serial.unwrap_or(NONE_U64).to_le_bytes());
        output.extend_from_slice(&record.file_reference.unwrap_or(NONE_U64).to_le_bytes());
        output.extend_from_slice(&record.size.unwrap_or(NONE_U64).to_le_bytes());
        output.extend_from_slice(&record.date_modified.unwrap_or(NONE_U64).to_le_bytes());
        output.extend_from_slice(&record.date_created.unwrap_or(NONE_U64).to_le_bytes());
        output.extend_from_slice(&record.attributes.unwrap_or(NONE_U32).to_le_bytes());
        push_string(&mut output, &record.path)?;
    }
    Ok(output)
}

fn decode_records(payload: &[u8]) -> Result<Vec<FileRecord>, DatabaseError> {
    let mut decoder = Decoder::new(payload);
    let count = usize::try_from(decoder.u64()?)
        .map_err(|_| DatabaseError::Corrupt("record count is too large"))?;
    if count > MAX_RECORDS || count > decoder.remaining() / 48 {
        return Err(DatabaseError::Corrupt("invalid record count"));
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        records.push(FileRecord {
            volume_serial: optional_u64(decoder.u64()?),
            file_reference: optional_u64(decoder.u64()?),
            size: optional_u64(decoder.u64()?),
            date_modified: optional_u64(decoder.u64()?),
            date_created: optional_u64(decoder.u64()?),
            attributes: optional_u32(decoder.u32()?),
            path: decoder.string()?,
            file_list_filename: None,
        });
    }
    if decoder.remaining() != 0 {
        return Err(DatabaseError::Corrupt("trailing payload data"));
    }
    Ok(records)
}

fn push_string(output: &mut Vec<u8>, value: &str) -> Result<(), DatabaseError> {
    if value.len() > MAX_STRING_SIZE {
        return Err(DatabaseError::Corrupt("path is too long"));
    }
    output.extend_from_slice(&(value.len() as u32).to_le_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

pub(crate) fn compare_ascii_case_insensitive(left: &str, right: &str) -> std::cmp::Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
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

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], DatabaseError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(DatabaseError::Corrupt("length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(DatabaseError::Corrupt("unexpected end of file"))?;
        self.offset = end;
        Ok(value)
    }

    fn u32(&mut self) -> Result<u32, DatabaseError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, DatabaseError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<String, DatabaseError> {
        let length = self.u32()? as usize;
        if length > MAX_STRING_SIZE {
            return Err(DatabaseError::Corrupt("path is too long"));
        }
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| DatabaseError::Corrupt("path is not UTF-8"))
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
            volume_serial: Some(12),
            file_reference: Some(34),
            size: Some(56),
            date_modified: Some(78),
            date_created: Some(90),
            attributes: Some(0x20),
            file_list_filename: None,
        }
    }

    #[test]
    fn database_round_trip_preserves_records() {
        let path = test_path("round-trip");
        let records = vec![record(r"C:\work\alpha.txt"), record(r"D:\beta.bin")];
        write(&path, &records).unwrap();
        assert_eq!(read(&path).unwrap(), records);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_truncated_and_modified_databases() {
        let path = test_path("corrupt");
        write(&path, &[record(r"C:\file.txt")]).unwrap();
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
        write(&path, &[record(r"C:\cached.txt")]).unwrap();

        let mut builds = 0;
        let cached = load_or_rebuild(&path, true, false, || {
            builds += 1;
            Ok(vec![record(r"C:\rebuilt.txt")])
        })
        .unwrap();
        assert_eq!(builds, 0);
        assert_eq!(cached[0].path, r"C:\cached.txt");

        let rebuilt = load_or_rebuild(&path, true, true, || {
            builds += 1;
            Ok(vec![record(r"C:\rebuilt.txt")])
        })
        .unwrap();
        assert_eq!(builds, 1);
        assert_eq!(rebuilt[0].path, r"C:\rebuilt.txt");
        assert_eq!(read(&path).unwrap(), rebuilt);
        fs::remove_file(path).unwrap();
    }
}
