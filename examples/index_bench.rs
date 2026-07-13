use std::hint::black_box;
use std::time::{Duration, Instant};

use everusthing::index::{FileId, Index, IndexRecord};
use everusthing::model::FILE_ATTRIBUTE_DIRECTORY;

fn main() {
    let count = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(500_000u64);
    let mut index = Index::default();
    index.register_volume(42, "C:", 5);
    index.upsert(record(5, 5, ".", FILE_ATTRIBUTE_DIRECTORY));
    for number in 0..count {
        let file_reference = number + 6;
        let parent_reference = if number < 32 { 5 } else { (number / 32) + 5 };
        index.upsert(record(
            file_reference,
            parent_reference,
            &format!("item-{number:07}"),
            if number % 8 == 0 {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                0x20
            },
        ));
    }

    let (elapsed, records) = best_of_three(|| index.snapshot());
    assert_eq!(records.len(), count as usize + 1);
    black_box(records);
    println!("records: {}", count + 1);
    println!(
        "snapshot: {:.2} M records/s ({elapsed:?})",
        (count + 1) as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );
    let (elapsed, records) = best_of_three(|| index.snapshot_unsorted());
    assert_eq!(records.len(), count as usize + 1);
    black_box(records);
    println!(
        "unsorted production snapshot: {:.2} M records/s ({elapsed:?})",
        (count + 1) as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );
}

fn record(frn: u64, parent: u64, name: &str, attributes: u32) -> IndexRecord {
    IndexRecord {
        id: FileId {
            volume_serial: 42,
            file_reference: frn,
        },
        parent_reference: parent,
        name: name.into(),
        size: None.into(),
        date_modified: None.into(),
        date_created: None.into(),
        attributes,
    }
}

fn best_of_three<T>(mut run: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut result = None;
    for _ in 0..3 {
        let start = Instant::now();
        let current = black_box(run());
        let elapsed = start.elapsed();
        if elapsed < best {
            best = elapsed;
            result = Some(current);
        }
    }
    (best, result.unwrap())
}
