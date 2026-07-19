//! Eager backend (T003, AD-7) real-file behaviour, against the scope API Contract:
//!
//!  * `Driver::builder()` is `MockDriver::builder()` minus the mock-only `seed`,
//!    and its `build()` is infallible — the eager slab is a plain preallocation,
//!    no ring to open.
//!  * `Driver::open` opens an existing file (no create mode in T003), so a
//!    missing path maps to `ENOENT`.
//!  * `DirectIo::Preferred` requests direct IO; `io_mode()` reports
//!    the probe outcome as `IoMode::{Direct(Alignment), Buffered}`, never a
//!    silent bool (scope Constraints).
//!  * `Driver::copy_frame` is the read-observation seam until T007's `FrameGuard`.
//!
//! On Linux `Driver` binds the uring backend (T004) and `src/backend/eager.rs`
//! does not compile there, so the eager submit path cannot be reached on Linux;
//! the Linux test covers only the backend-agnostic `src/open.rs` probe, and the
//! submit-path misalignment crash is pinned by the macOS pair. Linux submit-path
//! coverage lands with T004's uring test file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(not(target_os = "linux"))]
use std::time::Duration;

#[cfg(not(target_os = "linux"))]
use dios::driver::{CompletionBatch, Driver, FileHandle, IoMode, SubmitError, SyncMode};
use dios::testing::{DriverObservation, DriverReadTestingExt, ReadFrameIdx};
use dios::DirectIo;

const FRAME_BYTES: u32 = 4096;
#[cfg(not(target_os = "linux"))]
const ENOENT: i32 = 2;

static UNIQUE: AtomicU32 = AtomicU32::new(0);

/// A fresh, unique path under Cargo's per-suite temp dir (inside `target/`).
fn temp_path(tag: &str) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("eager-{tag}-{}-{n}", std::process::id()));
    path
}

fn driver(frames: u32, frame_bytes: u32, queue_capacity: u32) -> Driver {
    Driver::builder()
        .queue_capacity(queue_capacity)
        .frames(frames)
        .frame_bytes(frame_bytes)
        .write_slots(queue_capacity)
        .retry_bound(3)
        .build()
        .expect("the eager driver initializes")
}

fn open_existing(drv: &Driver, path: &Path, direct_io: DirectIo) -> FileHandle {
    drv.open(path, direct_io)
        .expect("open of a pre-created file succeeds")
}

#[cfg(not(target_os = "linux"))]
fn drain_one(drv: &Driver) -> Result<u32, i32> {
    let mut out = CompletionBatch::with_capacity(1);
    for _ in 0..64u32 {
        if drv.poll(&mut out) > 0 {
            let completion = out.iter().next().expect("one drained completion");
            return match completion.result() {
                Ok(bytes) => Ok(bytes),
                Err(err) => Err(err.raw_os_error().unwrap_or(-1)),
            };
        }
    }
    panic!("poll made no progress draining a completion");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn blocking_write_lands_bytes_on_disk_at_the_requested_offset() {
    let path = temp_path("blocking-write");
    std::fs::File::create(&path).expect("pre-create the target file");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    let payload = [0xABu8; 64];
    let offset = 100u64;
    let offset_bytes = usize::try_from(offset).expect("offset fits usize");
    drv.write_all_blocking(&fd, &payload, offset)
        .expect("a clean blocking write completes synchronously");
    drv.fsync_blocking(&fd, SyncMode::Full)
        .expect("a clean blocking fsync completes synchronously");

    let on_disk = std::fs::read(&path).expect("read the file back out of band");
    assert_eq!(
        on_disk.len(),
        offset_bytes + payload.len(),
        "pwrite at an offset extends the file to offset + len"
    );
    assert!(
        on_disk[..offset_bytes].iter().all(|&b| b == 0),
        "the gap before the offset is a zero hole, not clobbered"
    );
    assert_eq!(
        &on_disk[offset_bytes..],
        &payload,
        "the blocking write plumbs the offset through to a real pwrite"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn dropping_the_eager_driver_quiesces_an_admitted_write() {
    let path = temp_path("drop-quiesce-write");
    std::fs::File::create(&path).expect("pre-create the target file");
    let drv = driver(1, FRAME_BYTES, 1);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);
    let arena = drv.write_arena();
    let mut slot = arena.alloc().expect("one staging slot is available");
    slot.fill(0xD7);

    drv.submit_write(&fd, slot, 0)
        .expect("the write is admitted before teardown");
    drop(drv);

    let landed = std::fs::read(&path).expect("read the file after driver teardown");
    assert_eq!(
        landed,
        vec![0xD7; FRAME_BYTES as usize],
        "drop drains admitted eager work before the retained file and staging lease are released"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn exhausted_alloc_wait_pumps_a_write_and_preserves_its_completion() {
    let path = temp_path("write-alloc-wait");
    std::fs::File::create(&path).expect("pre-create the target file");
    let drv = driver(1, FRAME_BYTES, 1);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);
    let arena = drv.write_arena();
    let mut slot = arena.alloc().expect("one staging slot");
    slot.fill(0xA7);
    let token = drv
        .submit_write(&fd, slot, 0)
        .expect("the sole staging slot is admitted");

    assert!(arena.alloc().is_none(), "the admitted write owns the slot");
    let recycled = arena
        .alloc_wait(Duration::from_secs(1))
        .expect("the eager waiter pumps its own write to completion");
    assert_eq!(recycled.len(), FRAME_BYTES as usize);
    drop(recycled);

    let mut out = CompletionBatch::with_capacity(1);
    assert_eq!(
        drv.poll(&mut out),
        1,
        "the pumped completion remains visible"
    );
    assert_eq!(out.iter().next().expect("one completion").token(), token);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn alloc_wait_times_out_when_an_unsubmitted_slot_stays_held() {
    let drv = driver(1, FRAME_BYTES, 1);
    let arena = drv.write_arena();
    let held = arena.alloc().expect("one staging slot");
    let timeout = Duration::from_millis(20);
    let started = std::time::Instant::now();

    assert!(
        arena.alloc_wait(timeout).is_none(),
        "no admitted op can free the slot"
    );
    assert!(
        started.elapsed() >= timeout,
        "the bounded wait honors its deadline"
    );
    drop(held);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn completion_backlog_saturation_backpressures_then_recovers() {
    let path = temp_path("write-backlog");
    std::fs::File::create(&path).expect("pre-create the target file");
    let drv = driver(1, FRAME_BYTES, 2);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);
    let arena = drv.write_arena();

    let first = arena.alloc().expect("first slot");
    drv.submit_write(&fd, first, 0).expect("first write");
    let second = arena
        .alloc_wait(Duration::from_secs(1))
        .expect("first completion frees the staging slot");
    drv.submit_write(&fd, second, 0).expect("second write");
    let third = arena
        .alloc_wait(Duration::from_secs(1))
        .expect("second completion frees the staging slot");

    let Err((SubmitError::Full, third)) = drv.submit_write(&fd, third, 0) else {
        panic!("two preserved completions saturate the fixed admission bound");
    };
    let mut out = CompletionBatch::with_capacity(1);
    assert_eq!(
        drv.poll(&mut out),
        1,
        "public poll drains the oldest backlog entry"
    );
    drv.submit_write(&fd, third, 0)
        .expect("draining one completion restores one admission credit");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn a_write_slot_from_another_driver_is_rejected_before_consumption() {
    let path = temp_path("foreign-write-slot");
    std::fs::File::create(&path).expect("pre-create the target file");
    let owner = driver(1, FRAME_BYTES, 1);
    let foreign = driver(1, FRAME_BYTES, 1);
    let fd = open_existing(&foreign, &path, DirectIo::Disabled);
    let arena = owner.write_arena();
    let slot = arena.alloc().expect("owner slot");

    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = foreign.submit_write(&fd, slot, 0);
    }));
    assert!(
        rejected.is_err(),
        "cross-driver slots are programmer errors"
    );
    assert!(
        arena.alloc().is_some(),
        "unwinding returned the unconsumed owner slot"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn write_slots_are_aligned_to_the_configured_granule() {
    let granule = 16 * 1024;
    let drv = driver(1, granule, 1);
    let arena = drv.write_arena();
    let slot = arena.alloc().expect("one staging slot");

    assert_eq!(slot.len(), granule as usize);
    assert_eq!(slot.as_ptr().addr() % granule as usize, 0);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn submit_read_executes_at_poll_not_at_submit() {
    let path = temp_path("defer");
    std::fs::write(&path, [0x11u8; 100]).expect("seed a short file");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    std::fs::write(&path, [0x11u8; FRAME_BYTES as usize * 2])
        .expect("grow after submit, before poll");

    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "poll — not submit — ran the pread, so it read the grown file's full frame"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn a_single_threaded_submit_then_poll_completes_without_external_help() {
    let path = temp_path("inline");
    std::fs::write(&path, [0x7Eu8; FRAME_BYTES as usize]).expect("seed a full frame");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");

    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "poll on the sole thread drains the op — no background poller or waker is needed"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn a_read_lands_the_file_bytes_in_the_preallocated_frame() {
    let path = temp_path("roundtrip");
    let payload: Vec<u8> = (0..FRAME_BYTES).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &payload).expect("seed a full frame of known bytes");
    let drv = driver(2, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    drv.submit_read(&fd, ReadFrameIdx::new(1), 0)
        .expect("submit within capacity");
    assert_eq!(drain_one(&drv), Ok(FRAME_BYTES), "a full-frame read");

    let mut frame = vec![0u8; FRAME_BYTES as usize];
    let _ = drv.copy_frame(ReadFrameIdx::new(1), &mut frame);
    assert_eq!(
        frame, payload,
        "the eager pread landed the file's bytes into the configured frame slot"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn submit_read_honors_the_offset() {
    let path = temp_path("offset");
    let mut contents = vec![0xA1u8; FRAME_BYTES as usize];
    contents.extend(std::iter::repeat_n(0xB2u8, FRAME_BYTES as usize));
    std::fs::write(&path, &contents).expect("seed two frames of distinct bytes");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    drv.submit_read(&fd, ReadFrameIdx::new(0), u64::from(FRAME_BYTES))
        .expect("submit within capacity");
    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "a full-frame read at offset"
    );

    let mut frame = vec![0u8; FRAME_BYTES as usize];
    let _ = drv.copy_frame(ReadFrameIdx::new(0), &mut frame);
    assert!(
        frame.iter().all(|&b| b == 0xB2),
        "the read targeted the second frame's offset, not the file start"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn a_short_read_at_eof_reports_the_partial_count() {
    let path = temp_path("short");
    let short_len = 1500u32;
    std::fs::write(&path, vec![0x5Au8; short_len as usize]).expect("seed a sub-frame file");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");
    assert_eq!(
        drain_one(&drv),
        Ok(short_len),
        "the eager executor reports the true pread count at EOF, not a padded frame"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn open_of_a_missing_path_surfaces_enoent() {
    let path = temp_path("missing");
    let drv = driver(1, FRAME_BYTES, 4);

    let err = drv
        .open(&path, DirectIo::Disabled)
        .expect_err("opening a nonexistent path fails");
    assert_eq!(
        err.raw_os_error(),
        Some(ENOENT),
        "the eager open path maps the syscall errno into a typed IoError"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn fsync_full_barrier_succeeds_through_both_paths() {
    let path = temp_path("fsync");
    std::fs::write(&path, [0u8; 64]).expect("seed a file");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    drv.fsync_blocking(&fd, SyncMode::Full)
        .expect("the full-barrier fsync path runs a real fcntl and succeeds");

    drv.submit_fsync(&fd, SyncMode::Full)
        .expect("submit within capacity");
    assert_eq!(
        drain_one(&drv),
        Ok(0),
        "a clean fsync completes with a zero-byte transfer"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn direct_open_reports_nocache_direct_mode() {
    let path = temp_path("direct");
    std::fs::write(&path, [0u8; FRAME_BYTES as usize]).expect("seed a full frame");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Preferred);

    match fd.io_mode() {
        IoMode::Direct(alignment) => assert!(
            alignment.get().is_power_of_two() && alignment.get() >= 512,
            "the F_NOCACHE handle carries a self-imposed sector alignment: {alignment:?}"
        ),
        IoMode::Buffered => {
            panic!("F_NOCACHE is always available on darwin; a direct request must not fall back")
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
#[should_panic(expected = "is not aligned")]
fn a_misaligned_read_on_a_direct_handle_panics_before_an_op_issues() {
    let path = temp_path("misaligned");
    std::fs::write(&path, [0u8; FRAME_BYTES as usize]).expect("seed a full frame");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Preferred);

    let IoMode::Direct(sector) = fd.io_mode() else {
        panic!("a darwin direct open reports the F_NOCACHE Direct mode");
    };
    let misaligned = u64::from(sector.get()) + 1;

    let _ = drv.submit_read(&fd, ReadFrameIdx::new(0), misaligned);
}

#[cfg(target_os = "macos")]
#[test]
fn an_aligned_read_on_a_direct_handle_issues_and_completes() {
    let path = temp_path("aligned");
    std::fs::write(&path, [0x3Cu8; FRAME_BYTES as usize]).expect("seed a full frame");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Preferred);

    let IoMode::Direct(sector) = fd.io_mode() else {
        panic!("a darwin direct open reports the F_NOCACHE Direct mode");
    };
    assert_eq!(
        u64::from(FRAME_BYTES) % u64::from(sector.get()),
        0,
        "a granule-sized read at offset 0 is aligned by construction"
    );

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("an aligned read on a direct handle submits");
    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "the aligned direct read issues and completes with a full-frame transfer"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn direct_open_probe_encodes_alignment_or_falls_back_buffered() {
    let path = temp_path("linux-direct");
    std::fs::write(&path, vec![0u8; FRAME_BYTES as usize]).expect("seed a full frame");
    let drv = driver(1, FRAME_BYTES, 4);

    let direct = open_existing(&drv, &path, DirectIo::Preferred);
    match direct.io_mode() {
        IoMode::Direct(alignment) => assert!(
            alignment.get().is_power_of_two() && alignment.get() >= 512,
            "STATX_DIOALIGN yields a real sector alignment: {alignment:?}"
        ),
        IoMode::Buffered => {}
    }

    let buffered = open_existing(&drv, &path, DirectIo::Disabled);
    assert!(
        matches!(buffered.io_mode(), IoMode::Buffered),
        "a non-direct open always probes to Buffered — the mode is a reported enum, not a silent bool"
    );
}
