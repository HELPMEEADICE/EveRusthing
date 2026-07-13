use std::cmp::Ordering;
use std::hint::black_box;
use std::time::{Duration, Instant};

use everusthing::FileRecord;

fn main() {
    let count = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(500_000);
    assert!(count <= u32::MAX as usize);

    let records: Vec<_> = (0..count)
        .map(|index| FileRecord {
            path: format!(
                r"C:\workspace\project-{:05}\item-{:07}.dat",
                index.wrapping_mul(47_123) % 100_003,
                index
            ),
            ..FileRecord::default()
        })
        .collect();
    let mut cached_order: Vec<_> = (0..count as u32).collect();
    cached_order.sort_unstable_by(|left, right| {
        compare_parent(&records[*left as usize], &records[*right as usize])
    });

    let source: Vec<_> = (0..count as u32).filter(|index| index % 3 != 0).collect();
    let (comparison_sort, sorted) = best_of_five(|| {
        let mut visible = source.clone();
        visible.sort_unstable_by(|left, right| {
            compare_parent(&records[*left as usize], &records[*right as usize])
        });
        visible
    });
    let (cached_reorder, reordered) = best_of_five(|| {
        let mut included = vec![false; count];
        for &index in &source {
            included[index as usize] = true;
        }
        cached_order
            .iter()
            .copied()
            .filter(|index| included[*index as usize])
            .collect::<Vec<_>>()
    });
    assert_eq!(sorted, reordered);

    let (comparison_reverse, descending) = best_of_five(|| {
        let mut visible = sorted.clone();
        visible.sort_unstable_by(|left, right| {
            compare_parent(&records[*right as usize], &records[*left as usize])
        });
        visible
    });
    let (linear_reverse, reversed) = best_of_five(|| {
        let mut visible = sorted.clone();
        visible.reverse();
        visible
    });
    assert_eq!(descending, reversed);

    println!("records: {count}, visible: {}", source.len());
    println!("comparison reorder: {comparison_sort:?}");
    println!(
        "cached reorder:     {cached_reorder:?} ({:.2}x faster)",
        comparison_sort.as_secs_f64() / cached_reorder.as_secs_f64()
    );
    println!("comparison reverse: {comparison_reverse:?}");
    println!(
        "linear reverse:     {linear_reverse:?} ({:.2}x faster)",
        comparison_reverse.as_secs_f64() / linear_reverse.as_secs_f64()
    );
    println!(
        "single cached order: {:.2} MiB/million records",
        std::mem::size_of::<u32>() as f64 * 1_000_000.0 / (1024.0 * 1024.0)
    );
}

fn compare_parent(left: &FileRecord, right: &FileRecord) -> Ordering {
    compare_ascii_case_insensitive(left.parent_path(), right.parent_path())
        .then_with(|| left.path.cmp(&right.path))
}

fn compare_ascii_case_insensitive(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    for index in 0..left.len().min(right.len()) {
        let order = left[index]
            .to_ascii_lowercase()
            .cmp(&right[index].to_ascii_lowercase());
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn best_of_five(mut run: impl FnMut() -> Vec<u32>) -> (Duration, Vec<u32>) {
    let mut best = Duration::MAX;
    let mut result = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        result = black_box(run());
        best = best.min(start.elapsed());
    }
    (best, result)
}
