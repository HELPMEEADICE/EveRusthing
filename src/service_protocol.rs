use std::fmt::{self, Display, Formatter};
use std::io::{self, Read, Write};

use crate::model::FileRecord;

pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;
pub const COMMAND_PING: u32 = 0x4556_0000;
pub const COMMAND_SCAN_ALL: u32 = 0x4556_0001;
pub const REPLY_PONG: u32 = 0x4556_8000;
pub const REPLY_VOLUME: u32 = 0x4556_8001;
pub const REPLY_RECORDS: u32 = 0x4556_8002;
pub const REPLY_DONE: u32 = 0x4556_8003;
pub const REPLY_ERROR: u32 = 5;

const NONE_U64: u64 = u64::MAX;
const NONE_U32: u32 = u32::MAX;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub code: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeReply {
    pub root: String,
    pub volume_serial: u64,
    pub root_file_reference: u64,
    pub journal_id: u64,
    pub next_usn: i64,
    pub record_count: u64,
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    InvalidFrame(&'static str),
    InvalidUtf8,
}

impl Display for ProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, formatter),
            Self::InvalidFrame(message) => write!(formatter, "invalid service frame: {message}"),
            Self::InvalidUtf8 => formatter.write_str("invalid UTF-8 in service frame"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn read_frame(reader: &mut impl Read) -> Result<Frame, ProtocolError> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if !(8..=MAX_FRAME_SIZE).contains(&length) {
        return Err(ProtocolError::InvalidFrame("length is outside 8..=8 MiB"));
    }
    let mut body = vec![0u8; length - 4];
    reader.read_exact(&mut body)?;
    let code = u32::from_le_bytes(body[..4].try_into().unwrap());
    Ok(Frame {
        code,
        payload: body[4..].to_vec(),
    })
}

pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> Result<(), ProtocolError> {
    let length = frame
        .payload
        .len()
        .checked_add(8)
        .ok_or(ProtocolError::InvalidFrame("length overflow"))?;
    if length > MAX_FRAME_SIZE {
        return Err(ProtocolError::InvalidFrame("frame exceeds 8 MiB"));
    }
    writer.write_all(&(length as u32).to_le_bytes())?;
    writer.write_all(&frame.code.to_le_bytes())?;
    writer.write_all(&frame.payload)?;
    writer.flush()?;
    Ok(())
}

pub fn encode_volume(volume: &VolumeReply) -> Vec<u8> {
    let mut output = Vec::with_capacity(48 + volume.root.len());
    push_u64(&mut output, volume.volume_serial);
    push_u64(&mut output, volume.root_file_reference);
    push_u64(&mut output, volume.journal_id);
    push_i64(&mut output, volume.next_usn);
    push_u64(&mut output, volume.record_count);
    push_string(&mut output, &volume.root);
    output
}

pub fn decode_volume(bytes: &[u8]) -> Result<VolumeReply, ProtocolError> {
    let mut decoder = Decoder::new(bytes);
    let volume = VolumeReply {
        volume_serial: decoder.u64()?,
        root_file_reference: decoder.u64()?,
        journal_id: decoder.u64()?,
        next_usn: decoder.i64()?,
        record_count: decoder.u64()?,
        root: decoder.string()?,
    };
    decoder.finish()?;
    Ok(volume)
}

pub fn encode_records(records: &[FileRecord]) -> Vec<u8> {
    let mut output = Vec::new();
    push_u32(&mut output, records.len() as u32);
    for record in records {
        push_u64(&mut output, record.volume_serial.unwrap_or(NONE_U64));
        push_u64(&mut output, record.file_reference.unwrap_or(NONE_U64));
        push_u64(&mut output, record.size.unwrap_or(NONE_U64));
        push_u64(&mut output, record.date_modified.unwrap_or(NONE_U64));
        push_u64(&mut output, record.date_created.unwrap_or(NONE_U64));
        push_u32(&mut output, record.attributes.unwrap_or(NONE_U32));
        push_string(&mut output, &record.path);
    }
    output
}

pub fn decode_records(bytes: &[u8]) -> Result<Vec<FileRecord>, ProtocolError> {
    let mut decoder = Decoder::new(bytes);
    let count = decoder.u32()? as usize;
    if count > bytes.len() / 4 {
        return Err(ProtocolError::InvalidFrame("impossible record count"));
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let volume_serial = optional_u64(decoder.u64()?);
        let file_reference = optional_u64(decoder.u64()?);
        let size = optional_u64(decoder.u64()?);
        let date_modified = optional_u64(decoder.u64()?);
        let date_created = optional_u64(decoder.u64()?);
        let attributes = optional_u32(decoder.u32()?);
        let path = decoder.string()?;
        records.push(FileRecord {
            path,
            volume_serial,
            file_reference,
            size,
            date_modified,
            date_created,
            attributes,
            file_list_filename: None,
        });
    }
    decoder.finish()?;
    Ok(records)
}

pub fn records_in_pages(records: &[FileRecord], target_size: usize) -> Vec<&[FileRecord]> {
    let mut pages = Vec::new();
    let mut start = 0;
    let mut size = 4;
    for (index, record) in records.iter().enumerate() {
        let record_size = 48 + 4 + record.path.len();
        if index > start && size + record_size > target_size {
            pages.push(&records[start..index]);
            start = index;
            size = 4;
        }
        size += record_size;
    }
    if start < records.len() {
        pages.push(&records[start..]);
    }
    pages
}

fn optional_u64(value: u64) -> Option<u64> {
    (value != NONE_U64).then_some(value)
}

fn optional_u32(value: u32) -> Option<u32> {
    (value != NONE_U32).then_some(value)
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend(value.to_le_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend(value.to_le_bytes());
}

fn push_i64(output: &mut Vec<u8>, value: i64) {
    output.extend(value.to_le_bytes());
}

fn push_string(output: &mut Vec<u8>, value: &str) {
    push_u32(output, value.len() as u32);
    output.extend(value.as_bytes());
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64, ProtocolError> {
        let bytes = self.take(8)?;
        Ok(i64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let length = self.u32()? as usize;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(ProtocolError::InvalidFrame("field length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ProtocolError::InvalidFrame("truncated field"))?;
        self.offset = end;
        Ok(value)
    }

    fn finish(&self) -> Result<(), ProtocolError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ProtocolError::InvalidFrame("trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn frame_matches_everything_length_prefix_layout() {
        let frame = Frame {
            code: COMMAND_PING,
            payload: vec![1, 2, 3],
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).unwrap();

        assert_eq!(&bytes[..4], &11u32.to_le_bytes());
        assert_eq!(read_frame(&mut Cursor::new(bytes)).unwrap(), frame);
    }

    #[test]
    fn rejects_oversized_and_truncated_frames() {
        let mut oversized = Cursor::new(((MAX_FRAME_SIZE + 1) as u32).to_le_bytes());
        assert!(read_frame(&mut oversized).is_err());
        let mut truncated = Cursor::new([8, 0, 0, 0, 1, 0]);
        assert!(read_frame(&mut truncated).is_err());
    }

    #[test]
    fn volume_and_records_round_trip() {
        let volume = VolumeReply {
            root: "C:\\".into(),
            volume_serial: 42,
            root_file_reference: 5,
            journal_id: 9,
            next_usn: 100,
            record_count: 1,
        };
        assert_eq!(decode_volume(&encode_volume(&volume)).unwrap(), volume);

        let records = vec![FileRecord {
            path: "C:\\src\\main.rs".into(),
            volume_serial: Some(42),
            file_reference: Some(10),
            size: None,
            date_modified: Some(20),
            date_created: None,
            attributes: Some(32),
            file_list_filename: None,
        }];
        assert_eq!(decode_records(&encode_records(&records)).unwrap(), records);
    }

    #[test]
    fn record_pages_preserve_all_records() {
        let records: Vec<_> = (0..10)
            .map(|index| FileRecord {
                path: format!("C:\\file-{index}.txt"),
                ..FileRecord::default()
            })
            .collect();
        let pages = records_in_pages(&records, 150);
        assert!(pages.len() > 1);
        assert_eq!(pages.iter().map(|page| page.len()).sum::<usize>(), 10);
    }
}
