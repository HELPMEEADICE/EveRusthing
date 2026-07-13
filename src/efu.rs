use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
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
    fs::write(path, write(records)).map_err(|error| EfuError::Io(error.to_string()))
}

pub fn parse(bytes: &[u8]) -> Result<Vec<FileRecord>, EfuError> {
    let text = std::str::from_utf8(bytes).map_err(|_| EfuError::InvalidUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rows = parse_csv(text)?;
    let Some(header) = rows.first() else {
        return Err(EfuError::MissingFilenameColumn);
    };

    let column = |name: &str| {
        header
            .iter()
            .position(|value| value.eq_ignore_ascii_case(name))
    };
    let filename = column("Filename").ok_or(EfuError::MissingFilenameColumn)?;
    let size = column("Size");
    let modified = column("Date Modified");
    let created = column("Date Created");
    let attributes = column("Attributes");

    rows.into_iter()
        .skip(1)
        .filter(|row| row.iter().any(|field| !field.is_empty()))
        .map(|row| {
            let get =
                |index: Option<usize>| index.and_then(|index| row.get(index)).map(String::as_str);
            Ok(FileRecord {
                path: row.get(filename).cloned().unwrap_or_default(),
                volume_serial: None,
                file_reference: None,
                parent_reference: None,
                size: parse_optional_u64(get(size), "Size")?,
                date_modified: parse_optional_date(get(modified))?,
                date_created: parse_optional_date(get(created))?,
                attributes: parse_optional_u32(get(attributes), "Attributes")?,
                file_list_filename: None,
            })
        })
        .collect()
}

pub fn write(records: &[FileRecord]) -> Vec<u8> {
    let mut output = String::from("Filename,Size,Date Modified,Date Created,Attributes\r\n");
    for record in records {
        push_csv_field(&mut output, &record.path);
        output.push(',');
        push_optional(&mut output, record.size);
        output.push(',');
        push_optional(&mut output, record.date_modified);
        output.push(',');
        push_optional(&mut output, record.date_created);
        output.push(',');
        push_optional(&mut output, record.attributes);
        output.push_str("\r\n");
    }
    output.into_bytes()
}

fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, EfuError> {
    let mut rows = Vec::new();
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
                rows.push(std::mem::take(&mut row));
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
        rows.push(row);
    }
    Ok(rows)
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

fn push_optional<T: Display>(output: &mut String, value: Option<T>) {
    if let Some(value) = value {
        output.push_str(&value.to_string());
    }
}

fn push_csv_field(output: &mut String, value: &str) {
    if value.contains([',', '"', '\r', '\n']) {
        output.push('"');
        output.push_str(&value.replace('"', "\"\""));
        output.push('"');
    } else {
        output.push_str(value);
    }
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
            volume_serial: None,
            file_reference: None,
            parent_reference: None,
            size: Some(123),
            date_modified: Some(456),
            date_created: None,
            attributes: Some(32),
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
