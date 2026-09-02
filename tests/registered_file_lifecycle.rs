use std::fs;
#[cfg(feature = "mock")]
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(feature = "mock")]
use dios::IoMode;
#[cfg(feature = "mock")]
use dios::testing::{
    DirectIoSupport, MockDriver, MockPoolTestingExt, MockRingDriver, MockRingPoolBuilderTestingExt,
    PoolBuilderTestingExt,
};
use dios::{
    DirectIo, FileId, FileRegistrationError, FrameGuard, Get, GetError, PageId, PendingToken, Pool,
    PoolBuilder, PoolCompletion, PoolCompletionBatch, PoolToken, ReaderCtx, ReadyResult,
    RetireStatus,
};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 5;
const POLL_BOUND: u32 = 32;

static DIRECTORY_SEQUENCE: AtomicU32 = AtomicU32::new(0);

struct RegisteredFileDirectory(PathBuf);

impl RegisteredFileDirectory {
    fn create() -> Self {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        fs::create_dir_all(&base).expect("target temp directory");
        let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        for attempt in 0_u32..128 {
            let name = format!(
                "registered-files-{}-{sequence}-{attempt}",
                std::process::id()
            );
            let path = base.join(name);
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create registered-file test directory: {error}"),
            }
        }
        panic!("could not create a unique registered-file test directory");
    }

    fn path(&self, leaf: impl AsRef<Path>) -> PathBuf {
        self.0.join(leaf)
    }
}

impl Drop for RegisteredFileDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove registered-file test directory");
    }
}

fn registered_file_pool(capacity: u32, max_retained_frames: u32) -> Pool {
    registered_file_pool_builder(capacity, max_retained_frames)
        .build()
        .expect("the registered-file fixture satisfies the pool watermark")
}

fn registered_file_pool_builder(capacity: u32, max_retained_frames: u32) -> PoolBuilder {
    registered_file_base_builder(max_retained_frames).registered_file_capacity(capacity)
}

fn registered_file_base_builder(max_retained_frames: u32) -> PoolBuilder {
    Pool::builder()
        .frame_count(FRAME_COUNT)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .max_retained_frames(max_retained_frames)
        .write_slots(1)
        .max_inflight_product_ops(1)
}

#[cfg(feature = "mock")]
fn registered_file_mock_pool(
    capacity: u32,
    direct_io_support: DirectIoSupport,
) -> Pool<MockDriver> {
    let driver = MockDriver::builder()
        .queue_capacity(2)
        .frames(FRAME_COUNT)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .direct_io_support(direct_io_support)
        .build();
    registered_file_pool_builder(capacity, 0)
        .build_on(driver)
        .expect("the mock registered-file fixture is valid")
}

#[cfg(feature = "mock")]
fn registered_file_mock_ring_pool(capacity: u32) -> Pool<MockRingDriver> {
    let driver = MockRingDriver::builder()
        .queue_capacity(2)
        .frames(FRAME_COUNT)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .build();
    registered_file_pool_builder(capacity, 0)
        .build_on_ring(driver)
        .expect("the mock-ring registered-file fixture is valid")
}

fn registered_file_pending(outcome: Result<Get<'_>, GetError>) -> PendingToken {
    match outcome.expect("the file generation is live") {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("the first read of the fixture cannot hit"),
        Get::Busy => panic!("the fixture has sufficient miss headroom"),
    }
}

fn registered_file_ready<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    mut token: PendingToken,
) -> FrameGuard<'pool> {
    for _ in 0..POLL_BOUND {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return guard,
            ReadyResult::NotYet(returned) => {
                token = returned;
                pool.poll();
            }
            ReadyResult::Err(error) => panic!("the whole-granule fixture must read: {error}"),
        }
    }
    panic!("the registered read must complete within {POLL_BOUND} polls");
}

fn registered_file_write_whole_granule(pool: &Pool, file: FileId, byte: u8) {
    let mut slot = pool
        .write_arena()
        .alloc()
        .expect("one write staging slot is configured");
    slot.fill(byte);
    let token = pool
        .submit_write(file, slot, 0)
        .expect("the created file accepts a whole-granule write");
    let mut completions = PoolCompletionBatch::with_capacity(1);

    for _ in 0..POLL_BOUND {
        pool.poll_wait(&mut completions, std::time::Duration::from_secs(5));
        let completed = completions.iter().find(|completion| match completion {
            PoolCompletion::Write { token: seen, .. }
            | PoolCompletion::Fsync { token: seen, .. } => *seen == token,
        });
        if let Some(completion) = completed {
            registered_file_assert_write_completion(completion, token);
            return;
        }
    }
    panic!("the created-file write must complete within {POLL_BOUND} polls");
}

fn registered_file_assert_write_completion(completion: &PoolCompletion, expected: PoolToken) {
    match completion {
        PoolCompletion::Write {
            token,
            result: Ok(bytes),
        } => {
            assert_eq!(*token, expected);
            assert_eq!(*bytes, GRANULE);
        }
        PoolCompletion::Write {
            result: Err(error), ..
        } => panic!("the created-file write failed: {error}"),
        PoolCompletion::Fsync { .. } => panic!("the write token completed as fsync"),
    }
}

fn registered_file_poll_until_held(pool: &Pool) {
    for _ in 0..POLL_BOUND {
        pool.poll();
        if pool.retention_stats().retained_evictions_held == 1 {
            return;
        }
    }
    panic!("the retained retiring frame must become physically held");
}

fn registered_file_poll_until_retired(pool: &Pool, file: FileId) {
    for _ in 0..POLL_BOUND {
        pool.poll();
        if pool.retire_file(file) == RetireStatus::Retired {
            return;
        }
    }
    panic!("the released retiring file must physically close");
}

#[cfg(feature = "mock")]
#[test]
fn configured_capacity_above_64_reaches_mock_driver() {
    const CAPACITY: u32 = 65;
    let pool = registered_file_mock_pool(CAPACITY, DirectIoSupport::Supported);

    for index in 0..CAPACITY {
        let result = pool.open(
            Path::new(&format!("mock-capacity-live-{index}")),
            DirectIo::Disabled,
        );
        assert!(
            result.is_ok(),
            "configured mock file slot {index} was refused: {result:?}"
        );
    }
    let overflow = pool.open(Path::new("mock-capacity-overflow"), DirectIo::Disabled);
    assert!(
        matches!(overflow, Err(FileRegistrationError::AtCapacity)),
        "mock capacity overflow was not AtCapacity: {overflow:?}"
    );
}

#[cfg(feature = "mock")]
#[test]
fn configured_capacity_above_64_reaches_mock_ring_driver() {
    const CAPACITY: u32 = 65;
    let pool = registered_file_mock_ring_pool(CAPACITY);

    for index in 0..CAPACITY {
        let result = pool.open(
            Path::new(&format!("mock-ring-capacity-live-{index}")),
            DirectIo::Disabled,
        );
        assert!(
            result.is_ok(),
            "configured mock-ring file slot {index} was refused: {result:?}"
        );
    }
    let overflow = pool.open(Path::new("mock-ring-capacity-overflow"), DirectIo::Disabled);
    assert!(
        matches!(overflow, Err(FileRegistrationError::AtCapacity)),
        "mock-ring capacity overflow was not AtCapacity: {overflow:?}"
    );
}

#[cfg(feature = "mock")]
#[test]
fn at_capacity_precedes_mock_driver_policy_inspection() {
    let pool = registered_file_mock_pool(1, DirectIoSupport::Unsupported);
    pool.open(Path::new("mock-policy-holder"), DirectIo::Disabled)
        .expect("the sole configured slot is initially free");

    let result = pool.open(
        Path::new("mock-policy-must-not-be-inspected"),
        DirectIo::Required,
    );
    assert!(
        matches!(&result, Err(FileRegistrationError::AtCapacity)),
        "mock policy inspection preceded capacity refusal: {result:?}"
    );
}

#[cfg(feature = "mock")]
#[test]
fn at_capacity_precedes_mock_ring_path_inspection() {
    let pool = registered_file_mock_ring_pool(1);
    pool.open(Path::new("mock-ring-path-holder"), DirectIo::Disabled)
        .expect("the sole configured slot is initially free");

    let result = pool.open(Path::new(""), DirectIo::Disabled);
    assert!(
        matches!(&result, Err(FileRegistrationError::AtCapacity)),
        "mock-ring path inspection preceded capacity refusal: {result:?}"
    );
}

#[cfg(feature = "mock")]
#[test]
fn operating_emfile_with_a_logical_slot_free_is_io() {
    const EMFILE: i32 = 24;
    let pool = registered_file_mock_pool(1, DirectIoSupport::Supported);
    pool.driver()
        .inject_next_open_error_after_reservation(EMFILE);

    let error = pool
        .open(Path::new("logical-slot-is-free"), DirectIo::Disabled)
        .expect_err("backend EMFILE remains an operating failure");

    pool.open(Path::new("reservation-was-returned"), DirectIo::Disabled)
        .expect("the failed open returned its sole logical reservation");

    let FileRegistrationError::Io(error) = error else {
        panic!("a successful logical reservation cannot report AtCapacity");
    };
    assert_eq!(error.raw_os_error(), Some(EMFILE));
}

#[cfg(feature = "mock")]
#[test]
fn negotiated_io_mode_tracks_exact_pool_file_generation() {
    let pool = registered_file_mock_pool(2, DirectIoSupport::Supported);
    let old = pool
        .open(Path::new("io-mode-old"), DirectIo::Preferred)
        .expect("the old generation registers with direct IO");
    let Some(IoMode::Direct(alignment)) = pool.io_mode(old) else {
        panic!("the preferred supported file must report direct IO");
    };
    assert_eq!(alignment.get(), GRANULE);

    let absent_handle = pool
        .driver()
        .open(Path::new("io-mode-absent"), DirectIo::Disabled)
        .expect("the second driver slot remains outside Pool registration");
    let absent = absent_handle.file_id();
    assert_eq!(pool.io_mode(absent), None);

    let lease = pool
        .lease_file(old)
        .expect("the live generation admits a resident lease");
    assert_eq!(pool.retire_file(old), RetireStatus::Retiring);
    pool.poll();
    assert_eq!(pool.retire_file(old), RetireStatus::Retiring);
    let Some(IoMode::Direct(retiring_alignment)) = pool.io_mode(old) else {
        panic!("the retiring generation must retain its negotiated mode");
    };
    assert_eq!(retiring_alignment.get(), GRANULE);

    drop(lease);
    for _ in 0..POLL_BOUND {
        pool.poll();
        if pool.retire_file(old) == RetireStatus::Retired {
            break;
        }
    }
    assert_eq!(pool.retire_file(old), RetireStatus::Retired);
    assert_eq!(pool.io_mode(old), None);

    let replacement = pool
        .open(Path::new("io-mode-replacement"), DirectIo::Disabled)
        .expect("physical close returns the old slot");
    assert!(old.aliases_slot(&replacement));
    assert_ne!(old, replacement);
    assert_eq!(pool.io_mode(replacement), Some(IoMode::Buffered));
    assert_eq!(pool.io_mode(old), None);

    let foreign_pool = registered_file_mock_pool(1, DirectIoSupport::Supported);
    let foreign = foreign_pool
        .open(Path::new("io-mode-foreign"), DirectIo::Disabled)
        .expect("the foreign pool registers one file");
    let panic = catch_unwind(AssertUnwindSafe(|| pool.io_mode(foreign)));
    assert!(panic.is_err(), "a foreign Pool identity must panic");
}

#[test]
fn zero_capacity_builds_and_refuses_before_pathname_mutation() {
    let directory = RegisteredFileDirectory::create();
    let pool = registered_file_pool_builder(0, 0)
        .build()
        .expect("zero registered-file capacity is a valid configuration");
    let absent = directory.path("zero-capacity-absent");

    assert!(matches!(
        pool.create(&absent, DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
    assert!(!absent.exists(), "capacity refusal must not create a path");

    let existing = directory.path("zero-capacity-sentinel");
    let sentinel = b"zero capacity must not touch this artifact";
    fs::write(&existing, sentinel).expect("write sentinel fixture");
    assert!(matches!(
        pool.open(&existing, DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
    assert_eq!(fs::read(existing).expect("read sentinel fixture"), sentinel);
}

#[test]
fn default_registered_file_capacity_is_64() {
    const DEFAULT_CAPACITY: u32 = 64;
    let directory = RegisteredFileDirectory::create();
    let pool = registered_file_base_builder(0)
        .build()
        .expect("the default registered-file fixture builds");

    for index in 0..DEFAULT_CAPACITY {
        pool.create(
            &directory.path(format!("default-capacity-live-{index}")),
            DirectIo::Disabled,
        )
        .expect("every default file slot is usable");
    }
    assert!(matches!(
        pool.create(
            &directory.path("default-capacity-overflow"),
            DirectIo::Disabled
        ),
        Err(FileRegistrationError::AtCapacity)
    ));
}

#[test]
fn configured_capacity_above_64_accepts_exactly_that_many_live_files() {
    const CAPACITY: u32 = 65;
    let directory = RegisteredFileDirectory::create();
    let pool = registered_file_pool(CAPACITY, 1);
    let mut files = Vec::with_capacity(CAPACITY as usize);

    for index in 0..CAPACITY {
        let path = directory.path(format!("capacity-live-{index}"));
        let file = pool
            .create(&path, DirectIo::Disabled)
            .expect("every configured live-file slot is usable");
        assert!(
            files
                .iter()
                .all(|prior: &FileId| !prior.aliases_slot(&file)),
            "a newly registered live file must occupy a distinct slot"
        );
        files.push(file);
    }
    assert_eq!(files.len(), CAPACITY as usize);

    let overflow = directory.path("capacity-overflow");
    assert!(matches!(
        pool.create(&overflow, DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
    assert!(!overflow.exists());
}

#[test]
fn at_capacity_create_precedes_all_pathname_mutation() {
    let directory = RegisteredFileDirectory::create();
    let pool = registered_file_pool(1, 0);
    let holder = directory.path("capacity-holder");
    pool.create(&holder, DirectIo::Disabled)
        .expect("the sole configured slot is initially free");

    let absent = directory.path("capacity-absent");
    assert!(matches!(
        pool.create(&absent, DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
    assert!(!absent.exists(), "capacity refusal must not create a path");

    let existing = directory.path("capacity-sentinel");
    let sentinel = b"existing artifact must remain byte-for-byte intact";
    fs::write(&existing, sentinel).expect("write sentinel fixture");
    assert!(matches!(
        pool.create(&existing, DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
    assert!(matches!(
        pool.open(&existing, DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
    assert_eq!(fs::read(existing).expect("read sentinel fixture"), sentinel);
}

#[test]
fn create_new_failure_preserves_content_and_releases_its_reservation() {
    let directory = RegisteredFileDirectory::create();
    let pool = registered_file_pool(1, 0);
    let existing = directory.path("create-new-sentinel");
    let sentinel = b"never truncate this artifact";
    fs::write(&existing, sentinel).expect("write existing artifact");

    let error = pool
        .create(&existing, DirectIo::Disabled)
        .expect_err("create-new must reject an existing path");
    let FileRegistrationError::Io(error) = error else {
        panic!("an existing path is an operating failure, not capacity exhaustion");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs::read(existing).expect("read existing artifact"),
        sentinel
    );

    let replacement = directory.path("create-new-replacement");
    pool.create(&replacement, DirectIo::Disabled)
        .expect("the failed registration returned its sole reservation");
    assert!(matches!(
        pool.create(&directory.path("create-new-overflow"), DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
}

#[test]
fn retiring_retained_file_owns_capacity_until_close_then_reuses_a_fresh_generation() {
    const FILL: u8 = 0xC7;
    let directory = RegisteredFileDirectory::create();
    let pool = registered_file_pool(1, 1);
    let old_path = directory.path("retiring-old");
    let old = pool
        .create(&old_path, DirectIo::Disabled)
        .expect("the sole file slot is initially free");
    registered_file_write_whole_granule(&pool, old, FILL);
    assert_eq!(
        fs::read(&old_path).expect("read the completed created-file write"),
        vec![FILL; GRANULE as usize]
    );
    let reader = pool
        .register_reader()
        .expect("one reader slot is configured");
    let old_page = PageId::new(old, 0);
    let token = registered_file_pending(pool.get(&reader, old_page));
    let guard = registered_file_ready(&pool, &reader, token);
    let Ok(retained) = guard.into_retained() else {
        panic!("the configured retention budget admits the fixture frame");
    };
    assert!(retained.iter().all(|&byte| byte == FILL));

    assert_eq!(pool.retire_file(old), RetireStatus::Retiring);
    let replacement_path = directory.path("retiring-replacement");
    assert!(matches!(
        pool.create(&replacement_path, DirectIo::Disabled),
        Err(FileRegistrationError::AtCapacity)
    ));
    registered_file_poll_until_held(&pool);
    assert_eq!(pool.retire_file(old), RetireStatus::Retiring);
    assert!(!replacement_path.exists());

    drop(retained);
    registered_file_poll_until_retired(&pool, old);
    let replacement = pool
        .create(&replacement_path, DirectIo::Disabled)
        .expect("physical close returns the configured slot");
    assert!(old.aliases_slot(&replacement));
    assert_ne!(old, replacement, "slot reuse must mint a fresh generation");
    assert_eq!(
        pool.get(&reader, old_page)
            .expect_err("the stale identity must not resolve to its replacement"),
        GetError::StaleFile { page: old_page }
    );
}
