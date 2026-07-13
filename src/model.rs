use std::path::Path;
use std::sync::Arc;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileRecord {
    pub path: String,
    pub volume_serial: Option<u64>,
    pub file_reference: Option<u64>,
    pub parent_reference: Option<u64>,
    pub size: Option<u64>,
    pub date_modified: Option<u64>,
    pub date_created: Option<u64>,
    pub attributes: Option<u32>,
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
}
