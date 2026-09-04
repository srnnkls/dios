//! External-user contract for the deliberately small dios surface.
//!
//! The pool is the product API. The completion driver remains available for
//! consumers that need explicit slots and batches, but its vocabulary lives in
//! `dios::driver` instead of competing at the crate root.

#![expect(
    clippy::elidable_lifetime_names,
    clippy::match_same_arms,
    reason = "explicit lifetimes and separate exhaustive arms pin the frozen public contract"
)]

use std::any::TypeId;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(feature = "mock")]
use dios::FileRegistrationError;
use dios::driver::{
    Completion, CompletionBatch, Driver, FileHandle, OpKind, OpToken, SubmitError, WriteArena,
    WriteSlot,
};
#[cfg(feature = "mock")]
use dios::testing::{DirectIoSupport, MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use dios::{
    DirectIo, FileId, FrameGuard, Get, GetError, IoError, PageId, PendingToken, PollReport, Pool,
    PoolBuildError, PoolBuilder, PoolCompletion, PoolCompletionBatch, PoolConfigError,
    PoolSubmitError, PoolToken, PoolWakeHandle, PoolWriteArena, PoolWriteSlot, ReaderCtx,
    ReadyResult, RegisterError, RegistrationPolicy, RegistrationPosture, RetainRefused,
    RetainRefusedReason, RetainedFrame, RetentionStats, RetireStatus, SyncMode,
};

const GRANULE: u32 = 4096;

fn compile_api_probe(name: &str, source: &str) -> Output {
    let probe = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    fs::create_dir_all(probe.join("src")).expect("create the isolated API probe");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    fs::write(
        probe.join("Cargo.toml"),
        format!(
            "[package]\nname = \"dios-{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\ndios = {{ path = \"{manifest_dir}\" }}\n"
        ),
    )
    .expect("write the API probe manifest");
    fs::write(probe.join("src/lib.rs"), source).expect("write the API probe source");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Command::new(cargo)
        .args(["check", "--offline", "--quiet"])
        .current_dir(&probe)
        .env(
            "CARGO_TARGET_DIR",
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("public-api-probe-target"),
        )
        .output()
        .expect("run the isolated API probe")
}

fn assert_resident_file_lease_not_clone() {
    let output = compile_api_probe(
        "resident-file-lease-not-clone",
        r"
use dios::ResidentFileLease;

fn assert_clone<T: Clone>() {}

fn resident_file_lease_is_clone() {
    assert_clone::<ResidentFileLease>();
}
",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "ResidentFileLease must not be Clone"
    );
    assert!(
        !stderr.contains("E0432") && !stderr.contains("unresolved import"),
        "non-Clone probe failed before resolving ResidentFileLease:\n{stderr}"
    );
    assert!(
        stderr.contains("ResidentFileLease: Clone") && stderr.contains("is not satisfied"),
        "non-Clone probe failed for an unexpected reason:\n{stderr}"
    );
}

fn configured_pool() -> Result<Pool, PoolBuildError> {
    Pool::builder()
        .frame_count(8)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build()
}

fn configure_product_capacity(builder: PoolBuilder) -> PoolBuilder {
    builder.write_slots(2).max_inflight_product_ops(3)
}

fn configured_driver() -> Result<Driver, IoError> {
    Driver::builder()
        .queue_capacity(1)
        .frames(1)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .build()
}

fn open_driver_file(
    driver: &Driver,
    path: &Path,
    direct_io: DirectIo,
) -> Result<FileHandle, IoError> {
    driver.open(path, direct_io)
}

fn advanced_driver_contract(driver: &Driver, file: &FileHandle) {
    let kind: OpKind = OpKind::Read;
    std::hint::black_box(kind);
    let completion: Option<Completion> = None;
    std::hint::black_box(completion);
    let arena: WriteArena<'_> = driver.write_arena();
    let slot = arena.alloc().expect("the fixture reserves a staging slot");
    let submitted: Result<OpToken, (SubmitError, WriteSlot<'_>)> =
        driver.submit_write(file, slot, 0);
    if let Err((_error, recovered_slot)) = submitted {
        assert_eq!(recovered_slot.len(), GRANULE as usize);
    }
    let _barrier: Result<OpToken, SubmitError> = driver.submit_fsync(file, SyncMode::Full);

    let mut completions = CompletionBatch::with_capacity(1);
    let drained = driver.poll(&mut completions);
    assert!(drained <= 1);
}

fn product_pool_write_contract(
    pool: &Pool,
    file: FileId,
    completions: &mut PoolCompletionBatch,
) -> PollReport {
    let arena: PoolWriteArena<'_> = pool.write_arena();
    let slot = arena.alloc().expect("the fixture reserves pool staging");
    let submitted: Result<PoolToken, (PoolSubmitError, PoolWriteSlot<'_>)> =
        pool.submit_write(file, slot, 0);
    if let Err((_error, returned)) = submitted {
        assert_eq!(returned.len(), GRANULE as usize);
    }
    let _barrier: Result<PoolToken, PoolSubmitError> = pool.submit_fsync(file, SyncMode::Full);
    pool.poll_report(completions)
}

fn default_pool_write_arena(pool: &Pool) -> PoolWriteArena<'_> {
    pool.write_arena()
}

// PoolWriteArena and PoolWriteSlot are opaque product wrappers over
// crate-owned backend arenas. Both shipping and mock pools expose this same
// closed wrapper type; no backend trait or associated arena type is public.
#[cfg(feature = "mock")]
fn mock_pool_write_arena(pool: &Pool<MockDriver>) -> PoolWriteArena<'_> {
    pool.write_arena()
}

fn default_pool_write_slot<'pool>(pool: &'pool Pool) -> Option<PoolWriteSlot<'pool>> {
    pool.write_arena().alloc()
}

fn inspect_get_error(error: &GetError) {
    match error {
        GetError::StaleFile { page } => {
            let page: PageId = *page;
            std::hint::black_box(page);
        }
    }
}

fn inspect_retire_status(status: RetireStatus) {
    match status {
        RetireStatus::Retiring => {}
        RetireStatus::Retired => {}
    }
}

fn inspect_pool_submit_error(error: &PoolSubmitError) {
    match error {
        PoolSubmitError::Full => {}
        PoolSubmitError::StaleFile { file } => {
            let file: FileId = *file;
            std::hint::black_box(file);
        }
        PoolSubmitError::ForeignPool => {}
    }
}

fn assert_clone_send_sync<T: Clone + Send + Sync>() {}

#[test]
fn resident_hint_callable_scaffold_is_root_exported() {
    let output = compile_api_probe(
        "resident-hint-callable-scaffold",
        r"
use std::mem::size_of;

use dios::driver::Driver;
use dios::{
    FileId, Get, GetError, PageId, Pool, ReaderCtx, ResidentFileLease, ResidentHint,
    ResidentLeaseError,
};

const _: [(); 16] = [(); size_of::<ResidentHint>()];
const _: [(); 16] = [(); size_of::<Option<ResidentHint>>()];

fn assert_hint_traits<T: std::fmt::Debug + Clone + Copy + PartialEq + Eq>() {}
fn assert_error_traits<T: std::fmt::Debug + Clone + Copy + PartialEq + Eq>() {}

fn inspect_lease_error(error: ResidentLeaseError) -> FileId {
    match error {
        ResidentLeaseError::StaleFile { file } => {
            let file: FileId = file;
            file
        }
        ResidentLeaseError::Exhausted { file } => {
            let file: FileId = file;
            file
        }
    }
}

fn pin_callable_scaffold() {
    type D = Driver;
    let lease_file: fn(&Pool<D>, FileId) -> Result<ResidentFileLease, ResidentLeaseError> =
        Pool::<D>::lease_file;
    let resident_hint: fn(&Pool<D>, &ResidentFileLease, PageId) -> Option<ResidentHint> =
        Pool::<D>::resident_hint;
    let get_with_hint: for<'pool> fn(
        &'pool Pool<D>,
        &'pool ReaderCtx,
        &ResidentFileLease,
        PageId,
        Option<ResidentHint>,
    ) -> Result<Get<'pool>, GetError> = Pool::<D>::get_with_hint;
    assert_hint_traits::<ResidentHint>();
    assert_error_traits::<ResidentLeaseError>();
    std::hint::black_box((lease_file, resident_hint, get_with_hint, inspect_lease_error));
}
",
    );
    assert!(
        output.status.success(),
        "resident callable scaffold probe failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_resident_file_lease_not_clone();
}

#[test]
fn retention_api_is_directly_callable() {
    fn promote<'pool>(
        guard: FrameGuard<'pool>,
    ) -> Result<RetainedFrame<'pool>, RetainRefused<'pool>> {
        guard.into_retained()
    }

    let configure: fn(PoolBuilder, u32) -> PoolBuilder = PoolBuilder::max_retained_frames;
    let stats: fn(&Pool) -> RetentionStats = Pool::retention_stats;
    let exhausted = RetainRefusedReason::Exhausted;

    std::hint::black_box((configure, promote, stats, exhausted));
}

#[cfg(feature = "mock")]
#[test]
fn zero_budget_retention_refuses_with_live_guard_and_budget_stat() {
    let mock = MockDriver::builder()
        .queue_capacity(1)
        .frames(5)
        .frame_bytes(GRANULE)
        .build();
    let file = mock
        .open(Path::new("conservative-retention"), DirectIo::Disabled)
        .expect("mock open");
    let file_id = file.file_id();
    mock.seed_page(&file, 0, 0xA5);
    let pool = Pool::builder()
        .frame_count(5)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .max_retained_frames(0)
        .build_on(mock)
        .expect("valid zero-budget pool");
    pool.register_file(file);
    let reader = pool.register_reader().expect("one reader slot");
    let Get::Pending(token) = pool
        .get(&reader, PageId::new(file_id, 0))
        .expect("the registered file is live")
    else {
        panic!("the seeded page starts cold");
    };
    pool.poll();
    let ReadyResult::Ready(guard) = pool.ready(&reader, token) else {
        panic!("the deterministic read completes in one poll");
    };

    let Err(RetainRefused { guard, reason }) = guard.into_retained() else {
        panic!("disabled retention must refuse promotion");
    };
    assert!(matches!(reason, RetainRefusedReason::Exhausted));
    assert_eq!(guard.len(), GRANULE as usize);
    assert!(guard.iter().all(|&byte| byte == 0xA5));

    let stats = pool.retention_stats();
    assert_eq!(stats.occupied_budget, 0);
    assert_eq!(stats.refused_budget, 1);
    assert_eq!(stats.refused_ceiling, 0);
    assert_eq!(stats.refused_contention, 0);
    assert_eq!(stats.refused_retiring, 0);
    assert_eq!(stats.retained_evictions_held, 0);
}

#[test]
fn product_tokens_batches_and_slots_are_not_advanced_driver_types() {
    assert_ne!(TypeId::of::<PoolToken>(), TypeId::of::<OpToken>());
    assert_ne!(
        TypeId::of::<PoolCompletionBatch>(),
        TypeId::of::<CompletionBatch>()
    );
    assert_ne!(
        TypeId::of::<PoolWriteSlot<'static>>(),
        TypeId::of::<WriteSlot<'static>>()
    );
    assert_ne!(
        TypeId::of::<PoolWriteArena<'static>>(),
        TypeId::of::<WriteArena<'static>>()
    );
}

fn inspect_pool_completion(completion: &PoolCompletion) {
    match completion {
        PoolCompletion::Write { token, result } => {
            let token: PoolToken = *token;
            std::hint::black_box(token);
            match result {
                Ok(bytes) => {
                    let bytes: u32 = *bytes;
                    std::hint::black_box(bytes);
                }
                Err(error) => {
                    let error: &IoError = error;
                    std::hint::black_box(error);
                }
            }
        }
        PoolCompletion::Fsync { token, result } => {
            let token: PoolToken = *token;
            std::hint::black_box(token);
            match result {
                Ok(()) => {}
                Err(error) => {
                    let error: &IoError = error;
                    std::hint::black_box(error);
                }
            }
        }
    }
}

#[test]
fn pool_build_errors_distinguish_configuration_from_driver_initialization() {
    let configuration = Pool::builder()
        .frame_count(1)
        .granule(3)
        .max_concurrent_readers(0)
        .peak_guards_per_reader(0)
        .max_inflight_reads(0)
        .miss_headroom(0)
        .build()
        .expect_err("a non-power-of-two granule is invalid before backend init");
    assert!(matches!(
        &configuration,
        PoolBuildError::Configuration(PoolConfigError::GranuleNotPowerOfTwo { granule: 3 })
    ));

    let allocation = PoolBuildError::Allocation;
    let driver = PoolBuildError::Driver(IoError::from(std::io::Error::from_raw_os_error(5)));
    assert_ne!(
        std::mem::discriminant(&configuration),
        std::mem::discriminant(&allocation),
        "configuration rejection and allocation failure are distinct values"
    );
    assert_ne!(
        std::mem::discriminant(&allocation),
        std::mem::discriminant(&driver),
        "allocation failure and backend initialization failure are distinct values"
    );
    assert!(matches!(allocation, PoolBuildError::Allocation));
    assert!(matches!(driver, PoolBuildError::Driver(_)));

    let capacity_overflow = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .max_inflight_product_ops(u32::MAX)
        .build()
        .expect_err("read and product queue reservation must add without wrapping");
    assert!(matches!(
        capacity_overflow,
        PoolBuildError::Configuration(PoolConfigError::QueueCapacityOverflow {
            max_inflight_reads: 1,
            max_inflight_product_ops: u32::MAX,
        })
    ));
}

#[cfg(feature = "mock")]
#[test]
fn required_direct_io_refuses_unsupported_files_while_preferred_falls_back() {
    let mock = MockDriver::builder()
        .queue_capacity(1)
        .frames(4)
        .frame_bytes(GRANULE)
        .direct_io_support(DirectIoSupport::Unsupported)
        .build();
    let pool = Pool::builder()
        .frame_count(4)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build_on(mock)
        .expect("the deterministic mock pool configuration is valid");

    let path = Path::new("mock-direct-unsupported");
    let required = pool
        .open(path, DirectIo::Required)
        .expect_err("Required never silently falls back");
    let FileRegistrationError::Io(required) = required else {
        panic!("direct-I/O refusal must remain an operating failure");
    };
    assert_eq!(
        required.kind(),
        std::io::ErrorKind::Unsupported,
        "Required reports why the otherwise-valid open was refused"
    );
    let file = pool
        .open(path, DirectIo::Preferred)
        .expect("Preferred falls back to buffered IO on an unsupported file");
    let _page = PageId::new(file, 0);
}

#[test]
fn public_signatures_preserve_the_existing_residency_adt() {
    fn inspect(outcome: Result<Get<'_>, GetError>) {
        match outcome.expect("the file is live") {
            Get::Hit(frame) => assert!(!frame.is_empty()),
            Get::Pending(token) => {
                let _page = token.page();
            }
            Get::Busy => {}
        }
    }

    let inspect_signature: fn(Result<Get<'_>, GetError>) = inspect;
    let driver_contract_signature: fn(&Driver, &FileHandle) = advanced_driver_contract;
    let configure_product_capacity_signature: fn(PoolBuilder) -> PoolBuilder =
        configure_product_capacity;
    let write_slots_signature: fn(PoolBuilder, u32) -> PoolBuilder = PoolBuilder::write_slots;
    let max_inflight_product_ops_signature: fn(PoolBuilder, u32) -> PoolBuilder =
        PoolBuilder::max_inflight_product_ops;
    let pool_write_contract_signature: fn(&Pool, FileId, &mut PoolCompletionBatch) -> PollReport =
        product_pool_write_contract;
    let default_pool_write_arena_signature: fn(&Pool) -> PoolWriteArena<'_> =
        default_pool_write_arena;
    let default_pool_write_slot_signature: for<'pool> fn(
        &'pool Pool,
    ) -> Option<PoolWriteSlot<'pool>> = default_pool_write_slot;
    #[cfg(feature = "mock")]
    let mock_pool_write_arena_signature: fn(&Pool<MockDriver>) -> PoolWriteArena<'_> =
        mock_pool_write_arena;
    let inspect_pool_completion_signature: fn(&PoolCompletion) = inspect_pool_completion;
    let inspect_get_error_signature: fn(&GetError) = inspect_get_error;
    let inspect_retire_status_signature: fn(RetireStatus) = inspect_retire_status;
    let inspect_pool_submit_error_signature: fn(&PoolSubmitError) = inspect_pool_submit_error;
    let register_reader_signature: fn(&Pool) -> Result<ReaderCtx, RegisterError> =
        Pool::register_reader;
    let retire_file_signature: fn(&Pool, FileId) -> RetireStatus = Pool::retire_file;
    let pool_get_signature: for<'pool> fn(
        &'pool Pool,
        &'pool ReaderCtx,
        PageId,
    ) -> Result<Get<'pool>, GetError> = Pool::get;
    let pool_ready_signature: for<'pool> fn(
        &'pool Pool,
        &'pool ReaderCtx,
        PendingToken,
    ) -> ReadyResult<'pool> = Pool::ready;
    let wake_handle_signature: fn(&Pool) -> PoolWakeHandle = Pool::wake_handle;
    let wake_signature: fn(&PoolWakeHandle) = PoolWakeHandle::wake;
    assert_clone_send_sync::<PoolWakeHandle>();
    let build_pool_signature: fn() -> Result<Pool, PoolBuildError> = configured_pool;
    let open_driver_file_signature: fn(&Driver, &Path, DirectIo) -> Result<FileHandle, IoError> =
        open_driver_file;
    let build_driver_signature: fn() -> Result<Driver, IoError> = configured_driver;
    let construct_page_signature: fn(FileId, u32) -> PageId = PageId::new;
    let registration_posture_knob_signature: fn(PoolBuilder, RegistrationPolicy) -> PoolBuilder =
        PoolBuilder::registration_posture;
    let require_locked_knob_signature: fn(PoolBuilder) -> PoolBuilder = PoolBuilder::require_locked;
    let registration_posture_readback_signature: fn(&Pool) -> RegistrationPosture =
        Pool::registration_posture;
    let arena_locked_readback_signature: fn(&Pool) -> bool = Pool::arena_locked;
    let registration_policy_default: RegistrationPolicy = RegistrationPolicy::default();
    assert_eq!(registration_policy_default, RegistrationPolicy::Auto);
    let io_error_type: Option<IoError> = None;
    std::hint::black_box((
        inspect_signature,
        driver_contract_signature,
        configure_product_capacity_signature,
        write_slots_signature,
        max_inflight_product_ops_signature,
        pool_write_contract_signature,
        default_pool_write_arena_signature,
        default_pool_write_slot_signature,
        #[cfg(feature = "mock")]
        mock_pool_write_arena_signature,
        inspect_pool_completion_signature,
        inspect_get_error_signature,
        inspect_retire_status_signature,
        inspect_pool_submit_error_signature,
        register_reader_signature,
        retire_file_signature,
        pool_get_signature,
        pool_ready_signature,
        wake_handle_signature,
        wake_signature,
        build_pool_signature,
        open_driver_file_signature,
        build_driver_signature,
        construct_page_signature,
        registration_posture_knob_signature,
        require_locked_knob_signature,
        registration_posture_readback_signature,
        arena_locked_readback_signature,
        io_error_type,
    ));
}

#[test]
#[should_panic(expected = "completion batch capacity must be positive")]
fn completion_batches_reject_zero_capacity_at_construction() {
    let _ = CompletionBatch::with_capacity(0);
}

#[test]
fn zero_capacity_pool_completion_batches_construct_without_panicking() {
    let _batch = PoolCompletionBatch::with_capacity(0);
}
