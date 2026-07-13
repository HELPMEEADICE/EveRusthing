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

    pub(crate) fn install(&mut self, column: usize, order: Vec<u32>) {
        self.cached_column = Some(column);
        self.cached_order = order;
    }

    pub(crate) fn build_order(records: &[FileRecord], column: usize) -> Vec<u32> {
        Self::build_order_reusing(records, column, Vec::new())
    }

    pub(crate) fn take_order_storage(&mut self) -> Vec<u32> {
        self.cached_column = None;
        std::mem::take(&mut self.cached_order)
    }

    pub(crate) fn build_order_reusing(
        records: &[FileRecord],
        column: usize,
        mut order: Vec<u32>,
    ) -> Vec<u32> {
        order.clear();
        order.extend(0..records.len() as u32);
        match column {
            1 => sort_parent_paths(records, &mut order),
            _ => order.sort_unstable_by(|left, right| {
                compare_records(&records[*left as usize], &records[*right as usize], column)
            }),
        }
        order
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
            self.install(column, Self::build_order(records, column));
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

fn sort_parent_paths(records: &[FileRecord], order: &mut [u32]) {
    let parent_lengths: Vec<u32> = records
        .iter()
        .map(|record| record.parent_path().len() as u32)
        .collect();
    order.sort_unstable_by(|left, right| {
        let left_record = &records[*left as usize];
        let right_record = &records[*right as usize];
        let left_parent = &left_record.path[..parent_lengths[*left as usize] as usize];
        let right_parent = &right_record.path[..parent_lengths[*right as usize] as usize];
        crate::database::compare_ascii_case_insensitive(left_parent, right_parent)
            .then_with(|| left_record.path.cmp(&right_record.path))
    });
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

    #[test]
    fn optimized_builders_match_comparison_sort_for_every_column() {
        let mut records = Vec::new();
        for index in 0..2_000_u64 {
            records.push(FileRecord {
                path: format!(
                    r"C:\group-{}\item-{:04}.dat",
                    index.wrapping_mul(47) % 31,
                    2_000 - index
                ),
                size: (index % 7 != 0)
                    .then_some(index.wrapping_mul(97) % 53)
                    .into(),
                date_modified: (index % 11 != 0)
                    .then_some(index.wrapping_mul(193) % 101)
                    .into(),
                ..FileRecord::default()
            });
        }

        for column in 1..=3 {
            let mut expected: Vec<_> = (0..records.len() as u32).collect();
            expected.sort_unstable_by(|left, right| {
                compare_records_legacy(&records[*left as usize], &records[*right as usize], column)
            });
            assert_eq!(ResultSorter::build_order(&records, column), expected);
        }
    }

    #[test]
    #[ignore = "release-only performance probe"]
    fn benchmark_full_sort_builders() {
        use std::time::Instant;

        let records: Vec<_> = (0..500_000_u64)
            .map(|index| FileRecord {
                path: format!(
                    r"C:\group-{:05}\item-{:07}.dat",
                    index.wrapping_mul(47_123) % 100_003,
                    index
                ),
                size: (index % 13 != 0)
                    .then_some(index.wrapping_mul(97_531) % 10_000_019)
                    .into(),
                date_modified: (index % 17 != 0)
                    .then_some(index.wrapping_mul(1_000_003) % 1_000_000_007)
                    .into(),
                ..FileRecord::default()
            })
            .collect();

        let mut storage = Vec::new();
        for (column, name) in [(1, "Path"), (2, "Size"), (3, "Date Modified")] {
            let comparison = (column == 1).then(|| {
                let mut expected: Vec<_> = (0..records.len() as u32).collect();
                let start = Instant::now();
                expected.sort_unstable_by(|left, right| {
                    compare_records_legacy(
                        &records[*left as usize],
                        &records[*right as usize],
                        column,
                    )
                });
                (start.elapsed(), expected)
            });

            let start = Instant::now();
            storage = ResultSorter::build_order_reusing(&records, column, storage);
            let optimized_elapsed = start.elapsed();
            if let Some((comparison_elapsed, expected)) = comparison {
                assert_eq!(storage, expected);
                println!(
                    "{name}: comparison={comparison_elapsed:?}, optimized={optimized_elapsed:?}, speedup={:.2}x",
                    comparison_elapsed.as_secs_f64() / optimized_elapsed.as_secs_f64()
                );
            } else {
                println!("{name}: background build={optimized_elapsed:?}");
            }
        }
    }

    fn compare_records_legacy(left: &FileRecord, right: &FileRecord, column: usize) -> Ordering {
        match column {
            1 => crate::database::compare_ascii_case_insensitive(
                left.parent_path(),
                right.parent_path(),
            ),
            2 => left.size.get().cmp(&right.size.get()),
            3 => left.date_modified.get().cmp(&right.date_modified.get()),
            _ => Ordering::Equal,
        }
        .then_with(|| left.path.cmp(&right.path))
    }
}
