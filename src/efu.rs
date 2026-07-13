use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use crate::model::FileRecord;

const WINDOWS_TO_UNIX_EPOCH_SECONDS: i64 = 11_644_473_600;
const FILETIME_TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Debug, Eq, PartialEq)]
pub enum EfuError {
    Io(String),
    InvalidUtf8,
    UnterminatedQuote,
    MissingFilenameColumn,
    InvalidNumber { column: String, value: String },
    InvalidDate(String),
}

impl Display for EfuError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "{message}"),
            Self::InvalidUtf8 => formatter.write_str("EFU file is not valid UTF-8"),
            Self::UnterminatedQuote => formatter.write_str("unterminated quoted EFU field"),
            Self::MissingFilenameColumn => formatter.write_str("EFU header has no Filename column"),
            Self::InvalidNumber { column, value } => {
                write!(formatter, "invalid {column} value: {value}")
            }
            Self::InvalidDate(value) => write!(formatter, "invalid EFU date: {value}"),
        }
    }
}

impl Error for EfuError {}

pub fn read_file(path: &Path) -> Result<Vec<FileRecord>, EfuError> {
    let bytes = fs::read(path).map_err(|error| EfuError::Io(error.to_string()))?;
    let mut records = parse(&bytes)?;
    let file_list_filename: Arc<str> = path.to_string_lossy().into_owned().into();
    for record in &mut records {
        record.file_list_filename = Some(Arc::clone(&file_list_filename));
        record.resolve_relative_to(path);
    }
    Ok(records)
}

pub fn write_file(path: &Path, records: &[FileRecord]) -> Result<(), EfuError> {
    let file = File::create(path).map_err(|error| EfuError::Io(error.to_string()))?;
    let mut file = BufWriter::new(file);
    write_records(&mut file, records).map_err(|error| EfuError::Io(error.to_string()))?;
    file.flush()
        .map_err(|error| EfuError::Io(error.to_string()))
}

pub fn parse(bytes: &[u8]) -> Result<Vec<FileRecord>, EfuError> {
    let text = std::str::from_utf8(bytes).map_err(|_| EfuError::InvalidUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut columns = None;
    let mut records = Vec::new();
    parse_csv(text, |mut row| {
        let Some(columns) = columns else {
            let column = |name: &str| {
                row.iter()
                    .position(|value| value.eq_ignore_ascii_case(name))
            };
            columns = Some(Columns {
                filename: column("Filename").ok_or(EfuError::MissingFilenameColumn)?,
                size: column("Size"),
                modified: column("Date Modified"),
                created: column("Date Created"),
                attributes: column("Attributes"),
            });
            return Ok(());
        };
        if !row.iter().any(|field| !field.is_empty()) {
            return Ok(());
        }
        let get = |index: Option<usize>| index.and_then(|index| row.get(index)).map(String::as_str);
        let size = parse_optional_u64(get(columns.size), "Size")?;
        let date_modified = parse_optional_date(get(columns.modified))?;
        let date_created = parse_optional_date(get(columns.created))?;
        let attributes = parse_optional_u32(get(columns.attributes), "Attributes")?;
        let path = row
            .get_mut(columns.filename)
            .map(std::mem::take)
            .unwrap_or_default();
        records.push(FileRecord {
            path,
            volume_serial: None.into(),
            file_reference: None.into(),
            parent_reference: None.into(),
            size: size.into(),
            date_modified: date_modified.into(),
            date_created: date_created.into(),
            attributes: attributes.into(),
            file_list_filename: None,
        });
        Ok(())
    })?;
    columns.ok_or(EfuError::MissingFilenameColumn)?;
    Ok(records)
}

pub fn write(records: &[FileRecord]) -> Vec<u8> {
    let mut output = Vec::new();
    write_records(&mut output, records).expect("writing to a Vec cannot fail");
    output
}

#[derive(Clone, Copy)]
struct Columns {
    filename: usize,
    size: Option<usize>,
    modified: Option<usize>,
    created: Option<usize>,
    attributes: Option<usize>,
}

fn parse_csv(
    text: &str,
    mut visit: impl FnMut(Vec<String>) -> Result<(), EfuError>,
) -> Result<(), EfuError> {
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut field_start = true;

    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
                field_start = false;
            }
            '"' if quoted => quoted = false,
            '"' if field_start => quoted = true,
            ',' if !quoted => {
                row.push(std::mem::take(&mut field));
                field_start = true;
            }
            '\r' | '\n' if !quoted => {
                if character == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                visit(std::mem::take(&mut row))?;
                field_start = true;
            }
            _ => {
                field.push(character);
                field_start = false;
            }
        }
    }

    if quoted {
        return Err(EfuError::UnterminatedQuote);
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        visit(row)?;
    }
    Ok(())
}

fn parse_optional_u64(value: Option<&str>, column: &str) -> Result<Option<u64>, EfuError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    value
        .parse()
        .map(Some)
        .map_err(|_| EfuError::InvalidNumber {
            column: column.into(),
            value: value.into(),
        })
}

fn parse_optional_u32(value: Option<&str>, column: &str) -> Result<Option<u32>, EfuError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let parsed = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(|| value.parse(), |hex| u32::from_str_radix(hex, 16));
    parsed.map(Some).map_err(|_| EfuError::InvalidNumber {
        column: column.into(),
        value: value.into(),
    })
}

fn parse_optional_date(value: Option<&str>) -> Result<Option<u64>, EfuError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if let Ok(filetime) = value.parse() {
        return Ok(Some(filetime));
    }
    parse_iso_filetime(value).map(Some)
}

fn parse_iso_filetime(value: &str) -> Result<u64, EfuError> {
    let value = value.strip_suffix('Z').unwrap_or(value);
    if value.len() < 19 {
        return Err(EfuError::InvalidDate(value.into()));
    }
    let number = |range: std::ops::Range<usize>| {
        value
            .get(range)
            .and_then(|part| part.parse::<u32>().ok())
            .ok_or_else(|| EfuError::InvalidDate(value.into()))
    };
    let year = number(0..4)? as i32;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if !matches!(value.as_bytes().get(4), Some(b'-'))
        || !matches!(value.as_bytes().get(7), Some(b'-'))
        || !matches!(value.as_bytes().get(10), Some(b'T' | b' '))
        || !matches!(value.as_bytes().get(13), Some(b':'))
        || !matches!(value.as_bytes().get(16), Some(b':'))
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(EfuError::InvalidDate(value.into()));
    }

    let fraction = value.get(19..).unwrap_or_default();
    let fractional_ticks = if fraction.is_empty() {
        0
    } else {
        let digits = fraction
            .strip_prefix('.')
            .ok_or_else(|| EfuError::InvalidDate(value.into()))?;
        if digits.is_empty()
            || digits.len() > 7
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(EfuError::InvalidDate(value.into()));
        }
        let parsed: u64 = digits
            .parse()
            .map_err(|_| EfuError::InvalidDate(value.into()))?;
        parsed * 10_u64.pow(7 - digits.len() as u32)
    };

    let unix_days = days_from_civil(year, month, day);
    let unix_seconds =
        unix_days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    let windows_seconds = unix_seconds
        .checked_add(WINDOWS_TO_UNIX_EPOCH_SECONDS)
        .filter(|seconds| *seconds >= 0)
        .ok_or_else(|| EfuError::InvalidDate(value.into()))? as u64;
    Ok(windows_seconds * FILETIME_TICKS_PER_SECOND + fractional_ticks)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month as i32 + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day as i32 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

fn write_records(output: &mut impl Write, records: &[FileRecord]) -> std::io::Result<()> {
    output.write_all(b"Filename,Size,Date Modified,Date Created,Attributes\r\n")?;
    for record in records {
        write_csv_field(output, &record.path)?;
        output.write_all(b",")?;
        write_optional(output, record.size.get())?;
        output.write_all(b",")?;
        write_optional(output, record.date_modified.get())?;
        output.write_all(b",")?;
        write_optional(output, record.date_created.get())?;
        output.write_all(b",")?;
        write_optional(output, record.attributes.get())?;
        output.write_all(b"\r\n")?;
    }
    Ok(())
}

fn write_optional<T: Display>(output: &mut impl Write, value: Option<T>) -> std::io::Result<()> {
    if let Some(value) = value {
        write!(output, "{value}")?;
    }
    Ok(())
}

fn write_csv_field(output: &mut impl Write, value: &str) -> std::io::Result<()> {
    if value.contains([',', '"', '\r', '\n']) {
        output.write_all(b"\"")?;
        let mut rest = value.as_bytes();
        while let Some(index) = rest.iter().position(|byte| *byte == b'"') {
            output.write_all(&rest[..index])?;
            output.write_all(b"\"\"")?;
            rest = &rest[index + 1..];
        }
        output.write_all(rest)?;
        output.write_all(b"\"")?;
    } else {
        output.write_all(value.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bom_quotes_hex_attributes_and_iso_dates() {
        let input = concat!(
            "\u{feff}Filename,Size,Date Modified,Date Created,Attributes\r\n",
            "\"C:\\a,b\\say \"\"hello\"\".txt\",42,2020-01-02T03:04:05.5Z,,0x20\r\n",
            "C:\\folder,,,,16\r\n"
        );
        let records = parse(input.as_bytes()).unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "C:\\a,b\\say \"hello\".txt");
        assert_eq!(records[0].size, Some(42));
        assert_eq!(records[0].date_modified, Some(132_224_078_455_000_000));
        assert_eq!(records[0].attributes, Some(0x20));
        assert!(records[1].is_directory());
    }

    #[test]
    fn written_records_round_trip() {
        let records = vec![FileRecord {
            path: "C:\\comma,name\\quote\".txt".into(),
            volume_serial: None.into(),
            file_reference: None.into(),
            parent_reference: None.into(),
            size: Some(123).into(),
            date_modified: Some(456).into(),
            date_created: None.into(),
            attributes: Some(32).into(),
            file_list_filename: Some("ignored.efu".into()),
        }];

        assert_eq!(
            parse(&write(&records)).unwrap()[0],
            FileRecord {
                file_list_filename: None,
                ..records[0].clone()
            }
        );
    }

    #[test]
    fn rejects_unclosed_csv_quotes() {
        assert_eq!(
            parse(b"Filename\r\n\"broken"),
            Err(EfuError::UnterminatedQuote)
        );
    }
}
