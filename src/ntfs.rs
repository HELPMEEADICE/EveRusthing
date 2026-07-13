use std::fmt::{self, Display, Formatter};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetLogicalDriveStringsW, GetVolumeInformationW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, NTFS_VOLUME_DATA_BUFFER, READ_USN_JOURNAL_DATA_V0,
    USN_JOURNAL_DATA_V0, USN_REASON_FILE_DELETE, USN_REASON_RENAME_OLD_NAME,
};

use crate::index::{FileId, IndexRecord, SharedIndex};

const ENUM_BUFFER_SIZE: usize = 1024 * 1024;
const USN_RECORD_V2_MIN_SIZE: usize = 60;
const ALL_USN_REASONS: u32 = u32::MAX;
const MFT_RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const NTFS_ROOT_RECORD_NUMBER: u64 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NtfsError {
    pub operation: &'static str,
    pub code: u32,
    pub detail: Option<String>,
}

impl NtfsError {
    fn windows(operation: &'static str) -> Self {
        Self {
            operation,
            code: unsafe { GetLastError() },
            detail: None,
        }
    }

    fn malformed(detail: impl Into<String>) -> Self {
        Self {
            operation: "parse USN record",
            code: 0,
            detail: Some(detail.into()),
        }
    }

    pub fn is_access_denied(&self) -> bool {
        self.code == ERROR_ACCESS_DENIED
    }
}

impl Display for NtfsError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if let Some(detail) = &self.detail {
            return write!(formatter, "{}: {detail}", self.operation);
        }
        if self.is_access_denied() {
            write!(
                formatter,
                "{} failed: access denied; run as administrator or install the EveRusthing service",
                self.operation
            )
        } else {
            write!(
                formatter,
                "{} failed with Windows error {}",
                self.operation, self.code
            )
        }
    }
}

impl std::error::Error for NtfsError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NtfsVolumeInfo {
    pub root: String,
    pub volume_serial: u64,
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_file_record: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsnRecord {
    pub file_reference: u64,
    pub parent_reference: u64,
    pub usn: i64,
    pub timestamp: i64,
    pub reason: u32,
    pub attributes: u32,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsnBatch {
    pub next_usn: i64,
    pub records: Vec<UsnRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanResult {
    pub volume: NtfsVolumeInfo,
    pub root_file_reference: u64,
    pub journal_id: u64,
    pub next_usn: i64,
    pub record_count: usize,
}

#[derive(Debug)]
pub struct NtfsVolume {
    handle: HANDLE,
    info: NtfsVolumeInfo,
}

impl Drop for NtfsVolume {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl NtfsVolume {
    pub fn open(root: &str) -> Result<Self, NtfsError> {
        let root = normalize_root(root)?;
        let device = format!(r"\\.\{}", root.trim_end_matches('\\'));
        let device = wide_null(&device);
        let handle = unsafe {
            CreateFileW(
                device.as_ptr(),
                0x8000_0000,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(NtfsError::windows("open NTFS volume"));
        }

        let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { zeroed() };
        let mut returned = 0;
        let success = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                null(),
                0,
                (&mut data as *mut NTFS_VOLUME_DATA_BUFFER).cast(),
                size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if success == 0 {
            let error = NtfsError::windows("query NTFS volume data");
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }

        Ok(Self {
            handle,
            info: NtfsVolumeInfo {
                root,
                volume_serial: data.VolumeSerialNumber as u64,
                bytes_per_sector: data.BytesPerSector,
                bytes_per_cluster: data.BytesPerCluster,
                bytes_per_file_record: data.BytesPerFileRecordSegment,
            },
        })
    }

    pub fn info(&self) -> &NtfsVolumeInfo {
        &self.info
    }

    pub fn journal_state(&self) -> Result<(u64, i64), NtfsError> {
        let journal = self.query_journal()?;
        Ok((journal.UsnJournalID, journal.NextUsn))
    }

    pub fn scan_into(&self, index: &SharedIndex) -> Result<ScanResult, NtfsError> {
        let journal = self.query_journal()?;
        let mut input = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: journal.NextUsn,
        };
        let mut buffer = vec![0u8; ENUM_BUFFER_SIZE];
        let mut root_record_reference = None;
        let mut root_parent_reference = None;
        let mut record_count = 0;

        loop {
            let mut returned = 0;
            let success = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_ENUM_USN_DATA,
                    (&input as *const MFT_ENUM_DATA_V0).cast(),
                    size_of::<MFT_ENUM_DATA_V0>() as u32,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            if success == 0 {
                let error = unsafe { GetLastError() };
                if error == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(NtfsError {
                    operation: "enumerate NTFS MFT",
                    code: error,
                    detail: None,
                });
            }
            if returned < 8 {
                return Err(NtfsError::malformed(
                    "MFT enumeration buffer is shorter than 8 bytes",
                ));
            }

            input.StartFileReferenceNumber = read_u64(&buffer, 0)?;
            let records = parse_usn_records(&buffer[8..returned as usize])?;
            record_count += records.len();
            index.extend(records.into_iter().map(|record| {
                observe_root_reference(
                    &record,
                    &mut root_record_reference,
                    &mut root_parent_reference,
                );
                to_index_record(self.info.volume_serial, record)
            }));
        }

        let root_file_reference = root_record_reference
            .or(root_parent_reference)
            .ok_or_else(|| NtfsError::malformed("the MFT enumeration did not identify record 5"))?;
        index.register_volume(
            self.info.volume_serial,
            self.info.root.clone(),
            root_file_reference,
        );
        Ok(ScanResult {
            volume: self.info.clone(),
            root_file_reference,
            journal_id: journal.UsnJournalID,
            next_usn: journal.NextUsn,
            record_count,
        })
    }

    pub fn read_changes(&self, start_usn: i64, journal_id: u64) -> Result<UsnBatch, NtfsError> {
        let input = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: start_usn,
            ReasonMask: ALL_USN_REASONS,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };
        let mut buffer = vec![0u8; ENUM_BUFFER_SIZE];
        let mut returned = 0;
        let success = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_READ_USN_JOURNAL,
                (&input as *const READ_USN_JOURNAL_DATA_V0).cast(),
                size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if success == 0 {
            return Err(NtfsError::windows("read USN journal"));
        }
        if returned < 8 {
            return Err(NtfsError::malformed(
                "USN journal buffer is shorter than 8 bytes",
            ));
        }
        Ok(UsnBatch {
            next_usn: read_i64(&buffer, 0)?,
            records: parse_usn_records(&buffer[8..returned as usize])?,
        })
    }

    pub fn apply_batch(&self, index: &SharedIndex, batch: &UsnBatch) {
        apply_usn_batch(index, self.info.volume_serial, batch);
    }

    pub fn catch_up(
        &self,
        index: &SharedIndex,
        mut next_usn: i64,
        journal_id: u64,
    ) -> Result<i64, NtfsError> {
        loop {
            let batch = self.read_changes(next_usn, journal_id)?;
            let done = batch.records.is_empty() || batch.next_usn == next_usn;
            next_usn = batch.next_usn;
            self.apply_batch(index, &batch);
            if done {
                return Ok(next_usn);
            }
        }
    }

    fn query_journal(&self) -> Result<USN_JOURNAL_DATA_V0, NtfsError> {
        let mut journal: USN_JOURNAL_DATA_V0 = unsafe { zeroed() };
        let mut returned = 0;
        let success = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_QUERY_USN_JOURNAL,
                null(),
                0,
                (&mut journal as *mut USN_JOURNAL_DATA_V0).cast(),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if success == 0 {
            Err(NtfsError::windows("query USN journal"))
        } else {
            Ok(journal)
        }
    }
}

fn apply_usn_batch(index: &SharedIndex, volume_serial: u64, batch: &UsnBatch) {
    for record in &batch.records {
        let id = FileId {
            volume_serial,
            file_reference: record.file_reference,
        };
        if record.reason & USN_REASON_FILE_DELETE != 0 {
            index.remove(id);
        } else if record.reason & USN_REASON_RENAME_OLD_NAME == 0 {
            index.upsert(to_index_record(volume_serial, record.clone()));
        }
    }
}

pub fn apply_usn_changes(index: &SharedIndex, volume_serial: u64, batch: &UsnBatch) {
    apply_usn_batch(index, volume_serial, batch);
}

pub fn discover_ntfs_volumes() -> Result<Vec<String>, NtfsError> {
    let required = unsafe { GetLogicalDriveStringsW(0, null_mut()) };
    if required == 0 {
        return Err(NtfsError::windows("list logical drives"));
    }
    let mut buffer = vec![0u16; required as usize + 1];
    if unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) } == 0 {
        return Err(NtfsError::windows("list logical drives"));
    }

    let mut volumes = Vec::new();
    for root in split_wide_strings(&buffer) {
        let root_wide = wide_null(&root);
        let mut filesystem = [0u16; 32];
        let success = unsafe {
            GetVolumeInformationW(
                root_wide.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        };
        if success != 0 {
            let end = filesystem
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(filesystem.len());
            if String::from_utf16_lossy(&filesystem[..end]).eq_ignore_ascii_case("NTFS") {
                volumes.push(root);
            }
        }
    }
    Ok(volumes)
}

fn parse_usn_records(bytes: &[u8]) -> Result<Vec<UsnRecord>, NtfsError> {
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            return Err(NtfsError::malformed("truncated USN record header"));
        }
        let length = read_u32(bytes, offset)? as usize;
        if length < USN_RECORD_V2_MIN_SIZE || offset + length > bytes.len() {
            return Err(NtfsError::malformed(format!(
                "invalid USN record length {length}"
            )));
        }
        let major = read_u16(bytes, offset + 4)?;
        if major != 2 {
            return Err(NtfsError::malformed(format!(
                "unsupported USN record version {major}"
            )));
        }
        let name_length = read_u16(bytes, offset + 56)? as usize;
        let name_offset = read_u16(bytes, offset + 58)? as usize;
        if !name_length.is_multiple_of(2)
            || name_offset < USN_RECORD_V2_MIN_SIZE
            || name_offset + name_length > length
        {
            return Err(NtfsError::malformed("invalid USN filename range"));
        }
        let name_bytes = &bytes[offset + name_offset..offset + name_offset + name_length];
        records.push(UsnRecord {
            file_reference: read_u64(bytes, offset + 8)?,
            parent_reference: read_u64(bytes, offset + 16)?,
            usn: read_i64(bytes, offset + 24)?,
            timestamp: read_i64(bytes, offset + 32)?,
            reason: read_u32(bytes, offset + 40)?,
            attributes: read_u32(bytes, offset + 52)?,
            name: decode_utf16_lossy(name_bytes),
        });
        offset += length;
    }
    Ok(records)
}

fn decode_utf16_lossy(bytes: &[u8]) -> String {
    let code_units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    let mut output = String::with_capacity(bytes.len() / 2);
    for character in char::decode_utf16(code_units) {
        output.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    output
}

fn observe_root_reference(
    record: &UsnRecord,
    root_record_reference: &mut Option<u64>,
    root_parent_reference: &mut Option<u64>,
) {
    if record.file_reference & MFT_RECORD_NUMBER_MASK == NTFS_ROOT_RECORD_NUMBER {
        *root_record_reference = Some(record.file_reference);
    }
    if root_parent_reference.is_none()
        && record.parent_reference & MFT_RECORD_NUMBER_MASK == NTFS_ROOT_RECORD_NUMBER
    {
        *root_parent_reference = Some(record.parent_reference);
    }
}

fn to_index_record(volume_serial: u64, record: UsnRecord) -> IndexRecord {
    IndexRecord {
        id: FileId {
            volume_serial,
            file_reference: record.file_reference,
        },
        parent_reference: record.parent_reference,
        name: record.name,
        size: None,
        date_modified: None,
        date_created: None,
        attributes: record.attributes,
    }
}

fn normalize_root(root: &str) -> Result<String, NtfsError> {
    let root = root.trim();
    let bytes = root.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return Err(NtfsError {
            operation: "validate volume",
            code: 0,
            detail: Some(format!("expected a drive such as C:, got {root}")),
        });
    }
    Ok(format!("{}:\\", (bytes[0] as char).to_ascii_uppercase()))
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn split_wide_strings(buffer: &[u16]) -> Vec<String> {
    buffer
        .split(|value| *value == 0)
        .take_while(|value| !value.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, NtfsError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| NtfsError::malformed("truncated u16 field"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, NtfsError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| NtfsError::malformed("truncated u32 field"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, NtfsError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| NtfsError::malformed("truncated u64 field"))?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, NtfsError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| NtfsError::malformed("truncated i64 field"))?;
    Ok(i64::from_le_bytes(value.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usn_record(name: &str) -> Vec<u8> {
        let name: Vec<u16> = name.encode_utf16().collect();
        let length = (USN_RECORD_V2_MIN_SIZE + name.len() * 2 + 7) & !7;
        let mut bytes = vec![0u8; length];
        bytes[0..4].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        bytes[8..16].copy_from_slice(&123u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&5u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&77i64.to_le_bytes());
        bytes[32..40].copy_from_slice(&88i64.to_le_bytes());
        bytes[40..44].copy_from_slice(&0x100u32.to_le_bytes());
        bytes[52..56].copy_from_slice(&0x20u32.to_le_bytes());
        bytes[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&(USN_RECORD_V2_MIN_SIZE as u16).to_le_bytes());
        for (index, character) in name.into_iter().enumerate() {
            let start = USN_RECORD_V2_MIN_SIZE + index * 2;
            bytes[start..start + 2].copy_from_slice(&character.to_le_bytes());
        }
        bytes
    }

    fn parsed_record(file_reference: u64, parent_reference: u64) -> UsnRecord {
        UsnRecord {
            file_reference,
            parent_reference,
            usn: 1,
            timestamp: 2,
            reason: 0,
            attributes: 0x10,
            name: String::new(),
        }
    }

    #[test]
    fn parses_aligned_usn_v2_records() {
        let mut bytes = usn_record("alpha.txt");
        bytes.extend(usn_record("中文.rs"));
        let records = parse_usn_records(&bytes).unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].file_reference, 123);
        assert_eq!(records[0].parent_reference, 5);
        assert_eq!(records[0].name, "alpha.txt");
        assert_eq!(records[1].name, "中文.rs");
    }

    #[test]
    fn rejects_invalid_usn_ranges_and_versions() {
        let mut bytes = usn_record("file.txt");
        bytes[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert!(parse_usn_records(&bytes).is_err());

        let mut bytes = usn_record("file.txt");
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        assert!(parse_usn_records(&bytes).is_err());
    }

    #[test]
    fn identifies_ntfs_root_without_full_reference_self_parenting() {
        let root = 0x000A_0000_0000_0005;
        let stale_parent = 0x0009_0000_0000_0005;
        let mut root_record_reference = None;
        let mut root_parent_reference = None;

        observe_root_reference(
            &parsed_record(root, stale_parent),
            &mut root_record_reference,
            &mut root_parent_reference,
        );

        assert_eq!(root_record_reference.or(root_parent_reference), Some(root));
    }

    #[test]
    fn infers_ntfs_root_reference_when_record_five_is_not_enumerated() {
        let root = 0x000A_0000_0000_0005;
        let mut root_record_reference = None;
        let mut root_parent_reference = None;

        observe_root_reference(
            &parsed_record(0x0003_0000_0000_002A, root),
            &mut root_record_reference,
            &mut root_parent_reference,
        );

        assert_eq!(root_record_reference.or(root_parent_reference), Some(root));
    }

    #[test]
    fn normalizes_drive_roots() {
        assert_eq!(normalize_root("c:").unwrap(), "C:\\");
        assert_eq!(normalize_root("D:\\").unwrap(), "D:\\");
        assert!(normalize_root("folder").is_err());
    }

    #[test]
    fn splits_win32_multistrings() {
        let input = [
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0,
            b'D' as u16,
            b':' as u16,
            b'\\' as u16,
            0,
            0,
        ];
        assert_eq!(split_wide_strings(&input), ["C:\\", "D:\\"]);
    }

    #[test]
    fn applies_create_rename_and_delete_usn_reasons() {
        let index = SharedIndex::default();
        index.register_volume(42, "C:".into(), 5);
        let make = |name: &str, reason: u32| UsnRecord {
            file_reference: 10,
            parent_reference: 5,
            usn: 1,
            timestamp: 2,
            reason,
            attributes: 0x20,
            name: name.into(),
        };

        apply_usn_batch(
            &index,
            42,
            &UsnBatch {
                next_usn: 2,
                records: vec![make("old.txt", 0x100)],
            },
        );
        apply_usn_batch(
            &index,
            42,
            &UsnBatch {
                next_usn: 3,
                records: vec![
                    make("old.txt", USN_REASON_RENAME_OLD_NAME),
                    make("new.txt", 0x2000),
                ],
            },
        );
        assert_eq!(index.snapshot()[0].path, "C:\\new.txt");

        apply_usn_batch(
            &index,
            42,
            &UsnBatch {
                next_usn: 4,
                records: vec![make("new.txt", USN_REASON_FILE_DELETE)],
            },
        );
        assert!(index.is_empty());
    }

    #[test]
    fn cached_parent_links_keep_descendants_valid_after_directory_rename() {
        let source = SharedIndex::default();
        source.register_volume(42, "C:\\".into(), 5);
        source.extend([
            to_index_record(42, parsed_record(5, 5)),
            to_index_record(
                42,
                UsnRecord {
                    name: "old".into(),
                    ..parsed_record(10, 5)
                },
            ),
            to_index_record(
                42,
                UsnRecord {
                    name: "child.txt".into(),
                    attributes: 0x20,
                    ..parsed_record(20, 10)
                },
            ),
        ]);
        let cached = source.snapshot();
        let restored = SharedIndex::restore(&cached, [(42, "C:\\".into(), 5)]).unwrap();

        apply_usn_batch(
            &restored,
            42,
            &UsnBatch {
                next_usn: 2,
                records: vec![UsnRecord {
                    name: "new".into(),
                    reason: 0x2000,
                    ..parsed_record(10, 5)
                }],
            },
        );

        assert!(
            restored
                .snapshot()
                .iter()
                .any(|record| record.path == "C:\\new\\child.txt")
        );
    }
}
