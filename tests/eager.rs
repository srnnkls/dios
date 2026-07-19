//! Eager backend (T003, AD-7) real-file behaviour, against the scope API Contract:
//!
//!  * `Driver::builder()` is `MockDriver::builder()` minus the mock-only `seed`,
//!    and its `build()` is infallible — the eager slab is a plain preallocation,
//!    no ring to open.
//!  * `Driver::open` opens an existing file (no create mode in T003), so a
//!    missing path maps to `ENOENT`.
//!  * `OpenHow::read_write().direct()` requests direct IO; `io_mode()` reports
//!    the probe outcome as `IoMode::{Direct(Alignment), Buffered}`, never a
//!    silent bool (scope Constraints).
//!  * `Driver::copy_frame` is the read-observation seam until T007's `FrameGuard`.
//!
//! `WriteSlot`'s aligned backing buffer is T006's, so a data-carrying async
//! `submit_write` is not exercisable here.
//!
//! On Linux `Driver` binds the uring backend (T004) and `src/backend/eager.rs`
//! does not compile there, so the eager submit path cannot be reached on Linux;
//! the Linux test covers only the backend-agnostic `src/open.rs` probe, and the
//! submit-path misalignment crash is pinned by the macOS pair. Linux submit-path
//! coverage lands with T004's uring test file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(not(target_os = "linux"))]
use dios::driver::{CompletionBatch, Driver, FileHandle, IoMode, OpenHow, ReadFrameIdx, SyncMode};
use dios::testing::DriverObservation;

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
        .retry_bound(3)
        .build()
        .expect("the eager driver initializes")
}

fn open_existing(drv: &Driver, path: &Path, how: OpenHow) -> FileHandle {
    drv.open(path, how)
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
    let fd = open_existing(&drv, &path, OpenHow::read_write());

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
fn submit_read_executes_at_poll_not_at_submit() {
    let path = temp_path("defer");
    std::fs::write(&path, [0x11u8; 100]).expect("seed a short file");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, OpenHow::read_write());

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
    let fd = open_existing(&drv, &path, OpenHow::read_write());

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
    let fd = open_existing(&drv, &path, OpenHow::read_write());

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
    let fd = open_existing(&drv, &path, OpenHow::read_write());

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
    let fd = open_existing(&drv, &path, OpenHow::read_write());

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
        .open(&path, OpenHow::read_write())
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
    let fd = open_existing(&drv, &path, OpenHow::read_write());

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
    let fd = open_existing(&drv, &path, OpenHow::read_write().direct());

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
    let fd = open_existing(&drv, &path, OpenHow::read_write().direct());

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
    let fd = open_existing(&drv, &path, OpenHow::read_write().direct());

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

    let direct = open_existing(&drv, &path, OpenHow::read_write().direct());
    match direct.io_mode() {
        IoMode::Direct(alignment) => assert!(
            alignment.get().is_power_of_two() && alignment.get() >= 512,
            "STATX_DIOALIGN yields a real sector alignment: {alignment:?}"
        ),
        IoMode::Buffered => {}
    }

    let buffered = open_existing(&drv, &path, OpenHow::read_write());
    assert!(
        matches!(buffered.io_mode(), IoMode::Buffered),
        "a non-direct open always probes to Buffered — the mode is a reported enum, not a silent bool"
    );
}
