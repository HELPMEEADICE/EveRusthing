use std::cmp::Ordering;

use crate::FileRecord;

const CACHE_SORT_MIN_RESULTS: usize = 16_384;

#[derive(Default)]
pub(crate) struct ResultSorter {
    cached_column: Option<usize>,
    cached_order: Vec<u32>,
}

impl ResultSorter {
    pub(crate) fn invalidate(&mut self) {
        self.cached_column = None;
        self.cached_order.clear();
    }

    pub(crate) fn ordered_indices(&self, column: usize) -> Option<&[u32]> {
        (self.cached_column == Some(column)).then_some(self.cached_order.as_slice())
    }

    pub(crate) fn sort(
        &mut self,
        records: &[FileRecord],
        visible: &mut Vec<u32>,
        column: usize,
        ascending: bool,
        already_default_sorted: bool,
    ) {
        if column == 0 {
            restore_default_order(visible, records.len(), already_default_sorted);
        } else if self.cached_column == Some(column) {
            apply_cached_order(visible, &self.cached_order, records.len());
        } else if should_cache_sort(visible.len(), records.len()) {
            self.cached_order.clear();
            self.cached_order.extend(0..records.len() as u32);
            self.cached_order.sort_unstable_by(|left, right| {
                compare_records(&records[*left as usize], &records[*right as usize], column)
            });
            self.cached_column = Some(column);
            apply_cached_order(visible, &self.cached_order, records.len());
        } else {
            visible.sort_unstable_by(|left, right| {
                compare_records(&records[*left as usize], &records[*right as usize], column)
            });
        }

        if !ascending {
            visible.reverse();
        }
    }
}

fn should_cache_sort(visible_count: usize, record_count: usize) -> bool {
    visible_count == record_count
        || visible_count >= CACHE_SORT_MIN_RESULTS && visible_count >= record_count.div_ceil(2)
}

fn restore_default_order(visible: &mut Vec<u32>, record_count: usize, already_sorted: bool) {
    if already_sorted {
        return;
    }
    if visible.len() == record_count {
        visible.clear();
        visible.extend(0..record_count as u32);
    } else {
        visible.sort_unstable();
    }
}

fn apply_cached_order(visible: &mut Vec<u32>, order: &[u32], record_count: usize) {
    if visible.is_empty() {
        return;
    }
    if visible.len() == record_count {
        visible.clear();
        visible.extend_from_slice(order);
        return;
    }

    let mut included = vec![false; record_count];
    for &index in visible.iter() {
        included[index as usize] = true;
    }
    visible.clear();
    visible.extend(
        order
            .iter()
            .copied()
            .filter(|index| included[*index as usize]),
    );
}

fn compare_records(left: &FileRecord, right: &FileRecord, column: usize) -> Ordering {
    match column {
        0 => crate::database::compare_ascii_case_insensitive(left.file_name(), right.file_name()),
        1 => {
            crate::database::compare_ascii_case_insensitive(left.parent_path(), right.parent_path())
        }
        2 => left.size.cmp(&right.size),
        3 => left.date_modified.cmp(&right.date_modified),
        _ => Ordering::Equal,
    }
    .then_with(|| left.path.cmp(&right.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, size: u64, modified: u64) -> FileRecord {
        FileRecord {
            path: path.into(),
            size: Some(size).into(),
            date_modified: Some(modified).into(),
            ..FileRecord::default()
        }
    }

    fn records() -> Vec<FileRecord> {
        let mut records = vec![
            record(r"C:\z\charlie.txt", 30, 1),
            record(r"C:\b\Alpha.txt", 20, 3),
            record(r"C:\a\alpha.txt", 10, 2),
            record(r"C:\c\bravo.txt", 40, 4),
        ];
        crate::database::sort_records(&mut records);
        records
    }

    fn paths<'a>(records: &'a [FileRecord], visible: &[u32]) -> Vec<&'a str> {
        visible
            .iter()
            .map(|index| records[*index as usize].path.as_str())
            .collect()
    }

    #[test]
    fn caches_full_non_default_sort_and_reorders_filtered_results() {
        let records = records();
        let mut sorter = ResultSorter::default();
        let mut visible: Vec<_> = (0..records.len() as u32).collect();
        sorter.sort(&records, &mut visible, 2, true, true);
        assert_eq!(
            paths(&records, &visible),
            [
                r"C:\a\alpha.txt",
                r"C:\b\Alpha.txt",
                r"C:\z\charlie.txt",
                r"C:\c\bravo.txt"
            ]
        );
        assert!(sorter.ordered_indices(2).is_some());

        visible.retain(|index| records[*index as usize].size.get().unwrap() >= 20);
        visible.reverse();
        sorter.sort(&records, &mut visible, 2, false, false);
        assert_eq!(
            paths(&records, &visible),
            [r"C:\c\bravo.txt", r"C:\z\charlie.txt", r"C:\b\Alpha.txt"]
        );
    }

    #[test]
    fn restores_default_order_without_string_comparisons() {
        let records = records();
        let mut sorter = ResultSorter::default();
        let mut visible = vec![3, 1, 2, 0];
        sorter.sort(&records, &mut visible, 0, true, false);
        assert_eq!(visible, [0, 1, 2, 3]);

        sorter.sort(&records, &mut visible, 0, false, true);
        assert_eq!(visible, [3, 2, 1, 0]);
    }

    #[test]
    fn invalidation_discards_cached_record_order() {
        let records = records();
        let mut sorter = ResultSorter::default();
        let mut visible: Vec<_> = (0..records.len() as u32).collect();
        sorter.sort(&records, &mut visible, 3, true, true);
        assert!(sorter.ordered_indices(3).is_some());
        sorter.invalidate();
        assert!(sorter.ordered_indices(3).is_none());
    }
}
