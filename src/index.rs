use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::model::FileRecord;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileId {
    pub volume_serial: u64,
    pub file_reference: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRecord {
    pub id: FileId,
    pub parent_reference: u64,
    pub name: String,
    pub size: Option<u64>,
    pub date_modified: Option<u64>,
    pub date_created: Option<u64>,
    pub attributes: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VolumeRoot {
    path: String,
    file_reference: u64,
}

#[derive(Debug, Default)]
pub struct Index {
    records: HashMap<FileId, IndexRecord>,
    volumes: HashMap<u64, VolumeRoot>,
}

#[derive(Clone, Debug, Default)]
pub struct SharedIndex(Arc<RwLock<Index>>);

impl Index {
    pub fn register_volume(
        &mut self,
        volume_serial: u64,
        path: impl Into<String>,
        root_file_reference: u64,
    ) {
        self.volumes.insert(
            volume_serial,
            VolumeRoot {
                path: path.into(),
                file_reference: root_file_reference,
            },
        );
    }

    pub fn upsert(&mut self, record: IndexRecord) {
        self.records.insert(record.id, record);
    }

    pub fn extend(&mut self, records: impl IntoIterator<Item = IndexRecord>) {
        self.records
            .extend(records.into_iter().map(|record| (record.id, record)));
    }

    pub fn remove(&mut self, id: FileId) -> Option<IndexRecord> {
        self.records.remove(&id)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn snapshot(&self) -> Vec<FileRecord> {
        let mut output = Vec::with_capacity(self.records.len());
        let mut names = Vec::new();
        let mut visited = Vec::new();
        for record in self.records.values() {
            let Some(path) = self.resolve_path(record.id, &mut names, &mut visited) else {
                continue;
            };
            output.push(FileRecord {
                path,
                volume_serial: Some(record.id.volume_serial),
                file_reference: Some(record.id.file_reference),
                parent_reference: Some(record.parent_reference),
                size: record.size,
                date_modified: record.date_modified,
                date_created: record.date_created,
                attributes: Some(record.attributes),
                file_list_filename: None,
            });
        }
        output.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        output
    }

    fn resolve_path<'a>(
        &'a self,
        id: FileId,
        names: &mut Vec<&'a str>,
        visited: &mut Vec<u64>,
    ) -> Option<String> {
        let volume = self.volumes.get(&id.volume_serial)?;
        if id.file_reference == volume.file_reference {
            return Some(format!("{}\\", volume.path.trim_end_matches(['\\', '/'])));
        }

        names.clear();
        visited.clear();
        let mut current = id;
        loop {
            if visited.contains(&current.file_reference) {
                return None;
            }
            visited.push(current.file_reference);
            if current.file_reference == volume.file_reference {
                break;
            }
            let record = self.records.get(&current)?;
            names.push(record.name.as_str());
            current = FileId {
                volume_serial: id.volume_serial,
                file_reference: record.parent_reference,
            };
        }

        names.reverse();
        let root = volume.path.trim_end_matches(['\\', '/']);
        let path_len = root.len() + names.iter().map(|name| name.len() + 1).sum::<usize>();
        let mut path = String::with_capacity(path_len);
        path.push_str(root);
        for name in names {
            path.push('\\');
            path.push_str(name);
        }
        Some(path)
    }
}

impl SharedIndex {
    pub fn restore(
        records: &[FileRecord],
        volumes: impl IntoIterator<Item = (u64, String, u64)>,
    ) -> Result<Self, &'static str> {
        let index = Self::default();
        for (volume_serial, root, root_file_reference) in volumes {
            index.register_volume(volume_serial, root, root_file_reference);
        }
        let mut restored = Vec::with_capacity(records.len());
        for record in records {
            let (Some(volume_serial), Some(file_reference), Some(parent_reference)) = (
                record.volume_serial,
                record.file_reference,
                record.parent_reference,
            ) else {
                return Err("database record is missing NTFS identity metadata");
            };
            restored.push(IndexRecord {
                id: FileId {
                    volume_serial,
                    file_reference,
                },
                parent_reference,
                name: record.file_name().to_owned(),
                size: record.size,
                date_modified: record.date_modified,
                date_created: record.date_created,
                attributes: record.attributes.unwrap_or_default(),
            });
        }
        index.extend(restored);
        Ok(index)
    }

    pub fn register_volume(&self, volume_serial: u64, path: String, root_file_reference: u64) {
        self.0
            .write()
            .expect("index lock poisoned")
            .register_volume(volume_serial, path, root_file_reference);
    }

    pub fn upsert(&self, record: IndexRecord) {
        self.0.write().expect("index lock poisoned").upsert(record);
    }

    pub fn extend(&self, records: impl IntoIterator<Item = IndexRecord>) {
        self.0.write().expect("index lock poisoned").extend(records);
    }

    pub fn remove(&self, id: FileId) -> Option<IndexRecord> {
        self.0.write().expect("index lock poisoned").remove(id)
    }

    pub fn snapshot(&self) -> Vec<FileRecord> {
        self.0.read().expect("index lock poisoned").snapshot()
    }

    pub fn len(&self) -> usize {
        self.0.read().expect("index lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.read().expect("index lock poisoned").is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FILE_ATTRIBUTE_DIRECTORY;

    fn record(frn: u64, parent: u64, name: &str, attributes: u32) -> IndexRecord {
        IndexRecord {
            id: FileId {
                volume_serial: 42,
                file_reference: frn,
            },
            parent_reference: parent,
            name: name.into(),
            size: None,
            date_modified: None,
            date_created: None,
            attributes,
        }
    }

    #[test]
    fn resolves_children_inserted_before_their_parents() {
        let mut index = Index::default();
        index.register_volume(42, "C:", 5);
        index.upsert(record(20, 10, "main.rs", 0x20));
        index.upsert(record(10, 5, "src", FILE_ATTRIBUTE_DIRECTORY));
        index.upsert(record(5, 5, ".", FILE_ATTRIBUTE_DIRECTORY));

        let paths: Vec<_> = index
            .snapshot()
            .into_iter()
            .map(|record| record.path)
            .collect();
        assert_eq!(paths, ["C:\\", "C:\\src", "C:\\src\\main.rs"]);
    }

    #[test]
    fn rename_updates_descendant_paths_without_walking_children() {
        let mut index = Index::default();
        index.register_volume(42, "D:", 5);
        index.upsert(record(10, 5, "old", FILE_ATTRIBUTE_DIRECTORY));
        index.upsert(record(20, 10, "file.txt", 0x20));
        index.upsert(record(10, 5, "new", FILE_ATTRIBUTE_DIRECTORY));

        assert!(
            index
                .snapshot()
                .iter()
                .any(|record| record.path == "D:\\new\\file.txt")
        );
    }

    #[test]
    fn omits_orphans_and_parent_cycles() {
        let mut index = Index::default();
        index.register_volume(42, "E:", 5);
        index.upsert(record(10, 999, "orphan", 0x20));
        index.upsert(record(20, 21, "a", FILE_ATTRIBUTE_DIRECTORY));
        index.upsert(record(21, 20, "b", FILE_ATTRIBUTE_DIRECTORY));

        assert!(index.snapshot().is_empty());
    }

    #[test]
    fn restores_parent_links_from_a_database_snapshot() {
        let mut source = Index::default();
        source.register_volume(42, "C:", 5);
        source.upsert(record(5, 5, ".", FILE_ATTRIBUTE_DIRECTORY));
        source.upsert(record(10, 5, "folder", FILE_ATTRIBUTE_DIRECTORY));
        source.upsert(record(20, 10, "file.txt", 0x20));

        let restored = SharedIndex::restore(&source.snapshot(), [(42, "C:\\".into(), 5)]).unwrap();
        let paths: Vec<_> = restored
            .snapshot()
            .into_iter()
            .map(|record| record.path)
            .collect();
        assert_eq!(paths, ["C:\\", "C:\\folder", "C:\\folder\\file.txt"]);
    }
}
