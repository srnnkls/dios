//! Fresh-process retirement timing; compare interleaved binaries with `compare`.
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use dios::testing::{MockDriver, MockPoolTestingExt, PoolBuilderTestingExt, PoolTestingExt};
use dios::{DirectIo, FileId, PageId, Pool, RetireStatus};

const PAGES_PER_FILE: u32 = 64;

fn fixture(frames: u32, file_count: u32) -> (Pool<MockDriver>, Vec<FileId>) {
    assert!(frames >= file_count * PAGES_PER_FILE);
    assert!(file_count > 0);
    let driver = MockDriver::builder()
        .queue_capacity(4)
        .frames(frames)
        .frame_bytes(4096)
        .write_slots(2)
        .build();
    let pool = Pool::builder()
        .frame_count(frames)
        .granule(4096)
        .registered_file_capacity(file_count)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .write_slots(2)
        .max_inflight_product_ops(2)
        .build_on(driver)
        .expect("pool");
    let files: Vec<_> = (0..file_count)
        .map(|index| {
            pool.open(
                Path::new(&format!("retirement-{index}")),
                DirectIo::Disabled,
            )
            .expect("file")
        })
        .collect();
    for granule in 0..PAGES_PER_FILE {
        for file in &files {
            pool.insert_resident_frame(PageId::new(*file, granule), 0xA7);
        }
    }
    (pool, files)
}

fn main() -> ExitCode {
    let mut arguments = std::env::args()
        .skip(1)
        .filter(|argument| !argument.starts_with('-'));
    let (Some(frames), Some(file_count)) = (
        arguments
            .next()
            .and_then(|frames| frames.parse::<u32>().ok()),
        arguments.next().and_then(|files| files.parse::<u32>().ok()),
    ) else {
        println!("usage: file_retirement <frames> <files>");
        return ExitCode::SUCCESS;
    };
    let (pool, files) = fixture(frames, file_count);
    let started = Instant::now();
    for file in &files {
        assert_eq!(pool.retire_file(*file), RetireStatus::Retiring);
        for _ in 0..32 {
            if pool.retire_file(*file) == RetireStatus::Retired {
                break;
            }
            pool.poll();
        }
        assert_eq!(pool.retire_file(*file), RetireStatus::Retired);
    }
    let retirement_ns = started.elapsed().as_nanos();
    for file in files {
        assert!(pool.driver().is_closed(file));
    }
    println!("{retirement_ns}");
    ExitCode::SUCCESS
}
