use std::hint::black_box;
use std::time::{Duration, Instant};

use everusthing::FileRecord;
use everusthing::index::IndexRecord;
use everusthing::query::{Query, QueryOptions};

fn main() {
    let count = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(500_000);
    let records: Vec<_> = (0..count)
        .map(|index| FileRecord {
            path: format!(
                r"C:\workspace\project-{}\{}-{:07}.txt",
                index % 1_000,
                if index % 10 == 0 {
                    "Report"
                } else {
                    "document"
                },
                index
            ),
            ..FileRecord::default()
        })
        .collect();
    let query = Query::parse("report", QueryOptions::default()).unwrap();

    let (optimized, optimized_hits) = best_of_three(|| {
        records
            .iter()
            .filter(|record| query.matches(black_box(record)))
            .count()
    });
    let (allocating, allocating_hits) = best_of_three(|| {
        records
            .iter()
            .filter(|record| {
                record
                    .file_name()
                    .to_lowercase()
                    .contains(&black_box("report").to_lowercase())
            })
            .count()
    });

    assert_eq!(optimized_hits, allocating_hits);
    println!("records: {count}, hits: {optimized_hits}");
    println!(
        "optimized: {:>8.2} M records/s ({optimized:?})",
        throughput(count, optimized)
    );
    println!(
        "old allocating path: {:>8.2} M records/s ({allocating:?})",
        throughput(count, allocating)
    );
    println!(
        "speedup: {:.2}x",
        allocating.as_secs_f64() / optimized.as_secs_f64()
    );
    println!(
        "GUI result index memory: {:.2} MiB per million results (was {:.2})",
        index_memory_mib::<u32>(),
        index_memory_mib::<usize>()
    );
    println!(
        "FileRecord memory: {:.2} MiB per million records (was {:.2})",
        index_memory_mib::<FileRecord>(),
        144.0 * 1_000_000.0 / (1024.0 * 1024.0)
    );
    println!(
        "IndexRecord memory: {:.2} MiB per million records (was {:.2})",
        index_memory_mib::<IndexRecord>(),
        104.0 * 1_000_000.0 / (1024.0 * 1024.0)
    );
}

fn best_of_three(mut run: impl FnMut() -> usize) -> (Duration, usize) {
    let mut best = Duration::MAX;
    let mut result = 0;
    for _ in 0..3 {
        let start = Instant::now();
        result = black_box(run());
        best = best.min(start.elapsed());
    }
    (best, result)
}

fn throughput(count: usize, elapsed: Duration) -> f64 {
    count as f64 / elapsed.as_secs_f64() / 1_000_000.0
}

fn index_memory_mib<T>() -> f64 {
    (std::mem::size_of::<T>() * 1_000_000) as f64 / (1024 * 1024) as f64
}
