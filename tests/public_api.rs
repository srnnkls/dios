//! External-user contract for the deliberately small dios surface.
//!
//! The pool is the product API. The completion driver remains available for
//! consumers that need explicit slots and batches, but its vocabulary lives in
//! `dios::driver` instead of competing at the crate root.

use std::path::Path;

use dios::driver::{CompletionBatch, Driver, FileHandle, OpToken, SubmitError, WriteSlot};
#[cfg(feature = "mock")]
use dios::testing::{DirectIoSupport, MockDriver, PoolBuilderTestingExt};
use dios::{DirectIo, FileId, Get, IoError, PageId, Pool, PoolBuildError, PoolConfigError};

const GRANULE: u32 = 4096;

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

fn configured_driver() -> Result<Driver, IoError> {
    Driver::builder()
        .queue_capacity(1)
        .frames(1)
        .frame_bytes(GRANULE)
        .write_slots(1)
        .build()
}

fn open_pool_file(pool: &Pool, path: &Path) -> Result<FileId, IoError> {
    pool.open(path, DirectIo::Disabled)
}

fn open_driver_file(
    driver: &Driver,
    path: &Path,
    direct_io: DirectIo,
) -> Result<FileHandle, IoError> {
    driver.open(path, direct_io)
}

fn advanced_driver_contract(driver: &Driver, file: &FileHandle) {
    let arena = driver.write_arena();
    let slot = arena.alloc().expect("the fixture reserves a staging slot");
    let submitted: Result<OpToken, (SubmitError, WriteSlot<'_>)> =
        driver.submit_write(file, slot, 0);
    if let Err((_error, recovered_slot)) = submitted {
        assert_eq!(recovered_slot.len(), GRANULE as usize);
    }

    let mut completions = CompletionBatch::with_capacity(1);
    let drained = driver.poll(&mut completions);
    assert!(drained <= 1);
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

    let driver = PoolBuildError::Driver(IoError::from(std::io::Error::from_raw_os_error(5)));
    assert_ne!(
        std::mem::discriminant(&configuration),
        std::mem::discriminant(&driver),
        "configuration rejection and backend initialization failure are distinct values"
    );
    assert!(matches!(driver, PoolBuildError::Driver(_)));
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
    fn inspect(outcome: Get<'_>) {
        match outcome {
            Get::Hit(frame) => assert!(!frame.is_empty()),
            Get::Pending(token) => {
                let _page = token.page();
            }
            Get::Busy => {}
        }
    }

    let inspect_signature: fn(Get<'_>) = inspect;
    let driver_contract_signature: fn(&Driver, &FileHandle) = advanced_driver_contract;
    let build_pool_signature: fn() -> Result<Pool, PoolBuildError> = configured_pool;
    let open_pool_file_signature: fn(&Pool, &Path) -> Result<FileId, IoError> = open_pool_file;
    let open_driver_file_signature: fn(&Driver, &Path, DirectIo) -> Result<FileHandle, IoError> =
        open_driver_file;
    let build_driver_signature: fn() -> Result<Driver, IoError> = configured_driver;
    let construct_page_signature: fn(FileId, u32) -> PageId = PageId::new;
    let io_error_type: Option<IoError> = None;
    std::hint::black_box((
        inspect_signature,
        driver_contract_signature,
        build_pool_signature,
        open_pool_file_signature,
        open_driver_file_signature,
        build_driver_signature,
        construct_page_signature,
        io_error_type,
    ));
}

#[test]
#[should_panic(expected = "completion batch capacity must be positive")]
fn completion_batches_reject_zero_capacity_at_construction() {
    let _ = CompletionBatch::with_capacity(0);
}
