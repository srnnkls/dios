//! `io_uring` backend (T004) real-file behaviour, exercised only where the ring
//! exists — the whole file is Linux-only and empty on the darwin dev machine.
//!
//! `poll_wait` (`EXT_ARG` idle timeout, INV-3 submit-vs-parked) and deferred
//! `close`/`is_closed` are pinned in tests/driver.rs against the shared
//! `DriverCore` the uring `Driver` composes; they reach the public `Driver` only
//! once T004 forwards them, so the ring-level pins for them follow that.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use dios::driver::{CompletionBatch, Driver, FileHandle, IoMode, OpToken, SubmitError, SyncMode};
use dios::testing::{DriverObservation, DriverReadTestingExt, ReadFrameIdx};
use dios::DirectIo;

const FRAME_BYTES: u32 = 4096;
const ENOENT: i32 = 2;

const DRAIN_DEADLINE: Duration = Duration::from_secs(5);
const DRAIN_IDLE_BACKOFF: Duration = Duration::from_micros(50);
const DRAIN_POLLS_MAX: u32 = 1_000_000;

static UNIQUE: AtomicU32 = AtomicU32::new(0);

/// A fresh, unique path under Cargo's per-suite temp dir (inside `target/`). On
/// the bench host that dir sits on the `O_DIRECT`-supporting `NVMe` fs.
fn temp_path(tag: &str) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("uring-{tag}-{}-{n}", std::process::id()));
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
}

fn open_existing(drv: &Driver, path: &Path, direct_io: DirectIo) -> FileHandle {
    drv.open(path, direct_io)
        .expect("open of a pre-created file succeeds")
}

/// Drains the first available completion's result as `Copy` data: `Ok(bytes)` or
/// `Err(errno)`.
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

/// Drains completions across polls until `expected` are collected, returning each
/// completion's driver-issued token and `Copy` result in drain order.
fn drain_results(drv: &Driver, expected: usize) -> Vec<(OpToken, Result<u32, i32>)> {
    let mut out = CompletionBatch::with_capacity(expected.max(1));
    let mut drained = Vec::with_capacity(expected);
    let deadline = Instant::now() + DRAIN_DEADLINE;
    let mut polls = 0u32;
    while drained.len() < expected {
        let before = drained.len();
        drv.poll(&mut out);
        for completion in &out {
            let result = match completion.result() {
                Ok(bytes) => Ok(bytes),
                Err(err) => Err(err.raw_os_error().unwrap_or(-1)),
            };
            drained.push((completion.token(), result));
        }
        polls += 1;
        assert!(
            polls < DRAIN_POLLS_MAX,
            "drain exceeded its poll iteration cap"
        );
        assert!(
            Instant::now() < deadline,
            "not all completions reaped within the drain deadline"
        );
        if drained.len() == before {
            std::thread::sleep(DRAIN_IDLE_BACKOFF);
        }
    }
    drained
}

fn drain_tokens(drv: &Driver, expected: usize) -> Vec<OpToken> {
    drain_results(drv, expected)
        .into_iter()
        .map(|(token, _)| token)
        .collect()
}

// O_DIRECT is per-arch in the linux uapi asm/fcntl.h; aarch64 uses the generic value.
#[cfg(target_arch = "x86_64")]
const O_DIRECT: u32 = 0o40000;
#[cfg(target_arch = "aarch64")]
const O_DIRECT: u32 = 0o200_000;

/// The one open fd in this process whose `/proc/self/fd` target is `path`.
fn direct_read_retained_fd(path: &Path) -> u32 {
    let want = std::fs::canonicalize(path).expect("canonicalize the target path");
    let mut found: Vec<u32> = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").expect("read /proc/self/fd") {
        let entry = entry.expect("fd dir entry");
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        if target == want {
            let fd: u32 = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
                .expect("numeric fd name");
            found.push(fd);
        }
    }
    assert_eq!(
        found.len(),
        1,
        "exactly one retained fd must target the file; the probe must apply O_DIRECT to it, not open a separate fd: {found:?}"
    );
    found[0]
}

/// The octal `flags:` field of `/proc/self/fdinfo/<fd>`.
fn direct_read_fd_flags(fd: u32) -> u32 {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .expect("read fdinfo for the retained fd");
    for line in info.lines() {
        if let Some(value) = line.strip_prefix("flags:") {
            return u32::from_str_radix(value.trim(), 8).expect("octal flags value");
        }
    }
    panic!("fdinfo carried no flags: line");
}

#[test]
fn a_direct_read_lands_the_file_bytes_through_the_ring() {
    let path = temp_path("direct-read");
    let payload: Vec<u8> = (0..FRAME_BYTES).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &payload).expect("seed a full frame of known bytes");
    let drv = driver(2, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Preferred);

    let retained = direct_read_retained_fd(&path);
    assert_eq!(
        direct_read_fd_flags(retained) & O_DIRECT,
        O_DIRECT,
        "the retained fd itself carries O_DIRECT — the probe applied its result to that fd, not to a separate probe fd"
    );

    match fd.io_mode() {
        IoMode::Direct(alignment) => assert!(
            alignment.get().is_power_of_two() && alignment.get() >= 512,
            "the probe applied O_DIRECT to the retained fd with a real sector alignment: {alignment:?}"
        ),
        IoMode::Buffered => panic!(
            "the bench-host NVMe fs supports O_DIRECT; a direct open must not fall back to buffered"
        ),
    }

    drv.submit_read(&fd, ReadFrameIdx::new(1), 0)
        .expect("submit within capacity");
    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "the ring reaps a full-frame O_DIRECT READ_FIXED completion"
    );

    let mut frame = vec![0u8; FRAME_BYTES as usize];
    let copied = drv.copy_frame(ReadFrameIdx::new(1), &mut frame);
    assert_eq!(
        copied, FRAME_BYTES as usize,
        "the whole frame is observable"
    );
    assert_eq!(
        frame, payload,
        "READ_FIXED landed the file's bytes into the registered frame — proving register_file retained the fd"
    );
}

#[test]
fn a_blocking_write_then_ring_read_round_trips() {
    let path = temp_path("write-read");
    std::fs::File::create(&path).expect("pre-create the target file");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    let payload: Vec<u8> = (0..FRAME_BYTES).map(|i| (i % 97) as u8).collect();
    drv.write_all_blocking(&fd, &payload, 0)
        .expect("the blocking metadata-plane write completes through the ring");
    drv.fsync_blocking(&fd, SyncMode::Full)
        .expect("the blocking fsync barrier completes through the ring");

    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");
    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "the async ring read reaps the freshly written frame"
    );

    let mut frame = vec![0u8; FRAME_BYTES as usize];
    let _ = drv.copy_frame(ReadFrameIdx::new(0), &mut frame);
    assert_eq!(
        frame, payload,
        "the bytes the blocking write plumbed are read back through the ring"
    );
}

#[test]
fn an_async_write_uses_the_registered_write_arena() {
    let path = temp_path("fixed-write");
    std::fs::File::create(&path).expect("pre-create the target file");
    let drv = driver(1, FRAME_BYTES, 1);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);
    let arena = drv.write_arena();
    let mut slot = arena.alloc().expect("one registered staging slot");
    slot.fill(0xC7);

    drv.submit_write(&fd, slot, 0)
        .expect("WRITE_FIXED submit within capacity");
    assert_eq!(
        drain_one(&drv),
        Ok(FRAME_BYTES),
        "the ring accepts registered buffer index 1 and writes one granule"
    );
    assert_eq!(
        std::fs::read(&path).expect("read the written file"),
        vec![0xC7; FRAME_BYTES as usize]
    );
}

#[test]
fn an_async_fsync_completes_with_a_zero_byte_transfer() {
    let path = temp_path("async-fsync");
    std::fs::write(&path, [0u8; 64]).expect("seed a file");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    drv.submit_fsync(&fd, SyncMode::Full)
        .expect("submit within capacity");
    assert_eq!(
        drain_one(&drv),
        Ok(0),
        "a clean fsync reaps through the ring as a zero-byte completion"
    );
}

#[test]
fn batched_submits_before_any_poll_all_reap_out_of_order_tolerated() {
    const N: u32 = 6;
    let path = temp_path("batched");
    let contents: Vec<u8> = (0..N)
        .flat_map(|f| {
            let fill = u8::try_from(f + 1).expect("frame fill byte fits u8");
            std::iter::repeat_n(fill, FRAME_BYTES as usize)
        })
        .collect();
    std::fs::write(&path, &contents).expect("seed N distinct frames");
    let drv = driver(N, FRAME_BYTES, N + 2);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    let mut submitted: HashSet<OpToken> = HashSet::new();
    for f in 0..N {
        let token = drv
            .submit_read(
                &fd,
                ReadFrameIdx::new(f),
                u64::from(f) * u64::from(FRAME_BYTES),
            )
            .expect("all N submit before any poll — batched SQE fill");
        assert!(
            submitted.insert(token),
            "each submit mints a distinct token"
        );
    }

    let drained = drain_results(&drv, N as usize);
    assert_eq!(
        drained.len(),
        N as usize,
        "exactly N completions reap in total — a double-reap is caught here since the unique-token count is asserted separately"
    );
    let tokens: HashSet<OpToken> = drained.iter().map(|(token, _)| *token).collect();
    assert_eq!(
        tokens.len(),
        N as usize,
        "exactly N unique completions arrive — no duplicate, no missing token"
    );
    assert_eq!(
        tokens, submitted,
        "every batched completion is reaped by echoed token; completion order need not match submission order"
    );
    for (token, result) in &drained {
        assert_eq!(
            *result,
            Ok(FRAME_BYTES),
            "batched completion {token:?} reaps a full frame, not an error CQE"
        );
    }
    for f in 0..N {
        let mut frame = vec![0u8; FRAME_BYTES as usize];
        let copied = drv.copy_frame(ReadFrameIdx::new(f), &mut frame);
        assert_eq!(
            copied, FRAME_BYTES as usize,
            "the whole frame is observable"
        );
        let fill = u8::try_from(f + 1).expect("frame fill byte fits u8");
        assert!(
            frame.iter().all(|&byte| byte == fill),
            "frame {f} holds its distinct fill byte {fill} — offset routing landed each read in its own frame"
        );
    }
}

#[test]
fn a_full_queue_backpressures_without_blocking_then_recovers_after_a_poll() {
    let path = temp_path("sq-full");
    std::fs::write(&path, vec![0u8; FRAME_BYTES as usize * 2]).expect("seed two frames");
    let drv = driver(2, FRAME_BYTES, 1);
    let fd = open_existing(&drv, &path, DirectIo::Disabled);

    let first = drv
        .submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("the one queue slot fits");
    let start = Instant::now();
    let refused = drv.submit_read(&fd, ReadFrameIdx::new(1), u64::from(FRAME_BYTES));
    let elapsed = start.elapsed();
    assert!(
        matches!(refused, Err(SubmitError::Full)),
        "the overflow submit is refused with Full after the flush-retry-once, never a block"
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "SQ-full backpressure returns at once, never waiting on ring space: {elapsed:?}"
    );

    assert_eq!(
        drain_tokens(&drv, 1),
        vec![first],
        "the first op reaps through the ring and frees its slot"
    );
    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("capacity recovered once the completion drained");
}

#[test]
fn a_short_read_at_eof_reports_the_partial_count_through_the_ring() {
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
        "the ring surfaces the true byte count at EOF, not a padded frame"
    );
}

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
        "the uring open path maps the syscall errno into a typed IoError"
    );
}

#[test]
#[should_panic(expected = "is not aligned")]
fn a_misaligned_read_on_a_direct_handle_panics_before_an_op_issues() {
    let path = temp_path("misaligned");
    std::fs::write(&path, vec![0u8; FRAME_BYTES as usize]).expect("seed a full frame");
    let drv = driver(1, FRAME_BYTES, 4);
    let fd = open_existing(&drv, &path, DirectIo::Preferred);

    let IoMode::Direct(sector) = fd.io_mode() else {
        panic!("the bench-host fs must probe a direct O_DIRECT handle for this contract");
    };
    let misaligned = u64::from(sector.get()) + 1;

    let _ = drv.submit_read(&fd, ReadFrameIdx::new(0), misaligned);
}
