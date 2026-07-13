use std::path::Path;
use std::sync::Arc;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

#[repr(transparent)]
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct OptionalU64(u64);

#[repr(transparent)]
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct OptionalU32(u32);

impl OptionalU64 {
    pub const NONE: Self = Self(u64::MAX);

    pub const fn get(self) -> Option<u64> {
        if self.0 == u64::MAX {
            None
        } else {
            Some(self.0)
        }
    }

    pub fn is_some_and(self, predicate: impl FnOnce(u64) -> bool) -> bool {
        self.get().is_some_and(predicate)
    }

    pub const fn unwrap_or(self, default: u64) -> u64 {
        match self.get() {
            Some(value) => value,
            None => default,
        }
    }

    pub const fn or(self, fallback: Self) -> Self {
        if self.0 == u64::MAX { fallback } else { self }
    }

    pub fn map<T>(self, map: impl FnOnce(u64) -> T) -> Option<T> {
        self.get().map(map)
    }
}

impl OptionalU32 {
    pub const NONE: Self = Self(u32::MAX);

    pub const fn get(self) -> Option<u32> {
        if self.0 == u32::MAX {
            None
        } else {
            Some(self.0)
        }
    }

    pub fn is_some_and(self, predicate: impl FnOnce(u32) -> bool) -> bool {
        self.get().is_some_and(predicate)
    }

    pub const fn unwrap_or(self, default: u32) -> u32 {
        match self.get() {
            Some(value) => value,
            None => default,
        }
    }

    pub const fn unwrap_or_default(self) -> u32 {
        self.unwrap_or(0)
    }
}

impl Default for OptionalU64 {
    fn default() -> Self {
        Self::NONE
    }
}

impl Default for OptionalU32 {
    fn default() -> Self {
        Self::NONE
    }
}

impl From<Option<u64>> for OptionalU64 {
    fn from(value: Option<u64>) -> Self {
        value.map_or(Self::NONE, Self)
    }
}

impl From<Option<u32>> for OptionalU32 {
    fn from(value: Option<u32>) -> Self {
        value.map_or(Self::NONE, Self)
    }
}

impl From<OptionalU64> for Option<u64> {
    fn from(value: OptionalU64) -> Self {
        value.get()
    }
}

impl From<OptionalU32> for Option<u32> {
    fn from(value: OptionalU32) -> Self {
        value.get()
    }
}

impl std::fmt::Debug for OptionalU64 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(formatter)
    }
}

impl std::fmt::Debug for OptionalU32 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(formatter)
    }
}

impl PartialEq<Option<u64>> for OptionalU64 {
    fn eq(&self, other: &Option<u64>) -> bool {
        self.get() == *other
    }
}

impl PartialEq<Option<u32>> for OptionalU32 {
    fn eq(&self, other: &Option<u32>) -> bool {
        self.get() == *other
    }
}

impl Ord for OptionalU64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.get().cmp(&other.get())
    }
}

impl PartialOrd for OptionalU64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileRecord {
    pub path: String,
    pub volume_serial: OptionalU64,
    pub file_reference: OptionalU64,
    pub parent_reference: OptionalU64,
    pub size: OptionalU64,
    pub date_modified: OptionalU64,
    pub date_created: OptionalU64,
    pub attributes: OptionalU32,
    pub file_list_filename: Option<Arc<str>>,
}

impl FileRecord {
    pub fn file_name(&self) -> &str {
        self.path
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(self.path.as_str())
    }

    pub fn parent_path(&self) -> &str {
        let name_len = self.file_name().len();
        let end = self.path.len().saturating_sub(name_len);
        self.path[..end].trim_end_matches(['\\', '/'])
    }

    pub fn extension(&self) -> &str {
        let name = self.file_name();
        match name.rfind('.') {
            Some(index) if index + 1 < name.len() => &name[index + 1..],
            _ => "",
        }
    }

    pub fn is_directory(&self) -> bool {
        self.attributes
            .is_some_and(|attributes| attributes & FILE_ATTRIBUTE_DIRECTORY != 0)
    }

    pub fn resolve_relative_to(&mut self, file_list: &Path) {
        let path = Path::new(&self.path);
        if path.is_absolute() || self.path.starts_with("\\\\") {
            return;
        }

        if let Some(parent) = file_list.parent() {
            self.path = parent.join(path).to_string_lossy().into_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_windows_paths_on_non_windows_hosts_too() {
        let record = FileRecord {
            path: r"C:\work\archive.tar.gz".into(),
            ..FileRecord::default()
        };

        assert_eq!(record.file_name(), "archive.tar.gz");
        assert_eq!(record.parent_path(), r"C:\work");
        assert_eq!(record.extension(), "gz");
    }

    #[test]
    fn compact_record_layout_avoids_option_discriminants() {
        assert_eq!(std::mem::size_of::<OptionalU64>(), 8);
        assert_eq!(std::mem::size_of::<OptionalU32>(), 4);
        assert_eq!(std::mem::size_of::<FileRecord>(), 96);
        assert_eq!(OptionalU64::from(Some(0)).get(), Some(0));
        assert_eq!(OptionalU64::from(None).get(), None);
    }
}
