#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use dios::DirectIo;
use dios::driver::{CompletionBatch, Driver};
use dios::testing::{DriverReadTestingExt, ReadFrameIdx};

const FRAME_BYTES: u32 = 4096;
const TEARDOWN_BUDGET: Duration = Duration::from_millis(300);
const IN_FLIGHT_PROBE: Duration = Duration::from_millis(50);
const DROP_SLACK: Duration = Duration::from_secs(10);

static UNIQUE: AtomicU32 = AtomicU32::new(0);

fn never_completing_fifo(tag: &str) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target tmp dir");
    path.push(format!("teardown-{tag}-{}-{n}", std::process::id()));
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path has no interior NUL");
    // SAFETY: `c_path` is a valid NUL-terminated string for the call's duration.
    let status = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(status, 0, "mkfifo creates the never-completing read source");
    path
}

#[test]
fn ring_drop_with_a_never_completing_read_blocks_for_the_init_budget_then_returns_without_panicking()
 {
    let path = never_completing_fifo("stuck-read");
    let drv = Driver::builder()
        .queue_capacity(2)
        .frames(1)
        .frame_bytes(FRAME_BYTES)
        .teardown_budget(TEARDOWN_BUDGET)
        .build()
        .expect("the io_uring driver initializes");
    let fd = drv
        .open(&path, DirectIo::Disabled)
        .expect("the driver opens the FIFO read-write");
    drv.submit_read(&fd, ReadFrameIdx::new(0), 0)
        .expect("submit within capacity");
    let mut out = CompletionBatch::with_capacity(1);
    assert_eq!(
        drv.poll_wait(&mut out, IN_FLIGHT_PROBE),
        0,
        "the FIFO read is kernel-visible and has no data to complete with"
    );

    let (sender, receiver) = mpsc::channel();
    let dropper = thread::spawn(move || {
        let start = Instant::now();
        let outcome = panic::catch_unwind(AssertUnwindSafe(move || drop(drv)));
        sender
            .send((outcome.is_ok(), start.elapsed()))
            .expect("the test thread awaits the drop outcome");
    });
    let (returned_cleanly, elapsed) = receiver
        .recv_timeout(TEARDOWN_BUDGET + DROP_SLACK)
        .expect("drop honors the init-set budget instead of a longer default");
    dropper.join().expect("the dropping thread exits");

    assert!(
        returned_cleanly,
        "drop never panics because an op is undrained"
    );
    assert!(
        elapsed >= TEARDOWN_BUDGET,
        "drop blocks for the whole budget before leaking, returned after {elapsed:?}"
    );
}
