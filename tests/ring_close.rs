//! T005 deferred-close-through-the-ring pin, isolated from `tests/uring_wait.rs`
//! because it is COMPILE-RED on a `Driver::close`/`is_closed` wiring gap.
//!
//! The deferred-close STATE MACHINE landed (batch 5: `FileTable` Closing→Closed,
//! `reap_ring` retire past drain), but its public OBSERVATION surface is
//! unusable: `Driver::close(fd: FileHandle)` consumes the handle, while
//! `Driver::is_closed(fd: &FileHandle)` demands one back — and the real `Driver`
//! (unlike `MockDriver`) exposes no `duplicate_handle`, so after a close there is
//! no handle left to observe with. The mock already resolves this by taking a
//! `FileId` (`MockDriver::is_closed(FileId)`); this pin binds the same ergonomics
//! on the real driver, so it is RED until `Driver::is_closed` accepts the retained
//! `FileId` (or a `duplicate_handle` is added). Linux-only — the ring is there.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use dios::driver::{CompletionBatch, Driver};
use dios::testing::{DriverReadTestingExt, ReadFrameIdx};
use dios::DirectIo;

const FRAME_BYTES: u32 = 4096;
const DRAIN_DEADLINE: Duration = Duration::from_secs(5);
const DRAIN_IDLE_BACKOFF: Duration = Duration::from_micros(50);
const DRAIN_POLLS_MAX: u32 = 1_000_000;

static UNIQUE: AtomicU32 = AtomicU32::new(0);

fn seed_frame(tag: &str) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("ringclose-{tag}-{}-{n}", std::process::id()));
    std::fs::write(&path, vec![0x2Cu8; FRAME_BYTES as usize]).expect("seed a full frame");
    path
}

fn drain_one(drv: &Driver) -> Result<u32, i32> {
    let mut out = CompletionBatch::with_capacity(1);
    let deadline = Instant::now() + DRAIN_DEADLINE;
    for _ in 0..DRAIN_POLLS_MAX {
        if drv.poll(&mut out) > 0 {
            let completion = out.iter().next().expect("one drained completion");
            return match completion.result() {
                Ok(bytes) => Ok(bytes),
                Err(err) => Err(err.raw_os_error().unwrap_or(-1)),
            };
        }
        assert!(
            Instant::now() < deadline,
            "no completion reaped within the drain deadline"
        );
        std::thread::sleep(DRAIN_IDLE_BACKOFF);
    }
    panic!("no completion reaped within the drain poll iteration cap");
}

#[test]
fn deferred_close_through_the_ring_flips_is_closed_only_after_the_in_flight_read_drains() {
    let path = seed_frame("defer-close");
    let drv = Driver::builder()
        .queue_capacity(4)
        .frames(1)
        .frame_bytes(FRAME_BYTES)
        .retry_bound(3)
        .build();
    let fd = drv
        .open(&path, DirectIo::Disabled)
        .expect("open the seeded file");
    let id = fd.file_id();

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");
    drv.close(fd);

    assert!(
        !drv.is_closed(id),
        "close(2) must not issue through the ring while a read is in flight (INV-11)"
    );

    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "the read on the closing fd still completes through the ring"
    );
    assert!(
        drv.is_closed(id),
        "the deferred close(2) is observable only after the fd's in-flight ops drain"
    );
}
