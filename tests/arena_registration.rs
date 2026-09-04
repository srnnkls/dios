//! Registration posture (pinned-frame-retention T14): under the stock
//! `RLIMIT_MEMLOCK` an `Auto` build degrades to `Unregistered` instead of
//! failing, an explicit posture is honoured or refused with a typed
//! configuration error, and the two readbacks report what the build selected.
//! The limit-lowering cases serialise on one gate because `setrlimit` is
//! process-wide and the harness runs tests on threads.

use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use dios::{
    DirectIo, FileId, FrameGuard, Get, GetError, PageId, PendingToken, Pool, PoolBuildError,
    PoolBuilder, PoolConfigError, ReaderCtx, ReadyResult, RegistrationPolicy, RegistrationPosture,
};

const GRANULE: u32 = 4096;
const FRAME_COUNT: u32 = 16;
const WRITE_SLOTS: u32 = 1;
const ARENA_BYTES: u64 = (FRAME_COUNT as u64 + WRITE_SLOTS as u64) * GRANULE as u64;
#[cfg(target_os = "linux")]
const MEMLOCK_FLOOR_BYTES: u64 = 64 * 1024;
const POLLS_MAX: u32 = 100_000;
const EXTENTS: u32 = 8;

#[cfg(target_os = "linux")]
const RLIMIT_MEMLOCK: c_int = 8;
#[cfg(target_os = "macos")]
const RLIMIT_MEMLOCK: c_int = 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct Rlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

// getrlimit(2)/setrlimit(2) declared to match the C ABI of the supported
// targets, where `rlim_t` is a 64-bit unsigned integer.
unsafe extern "C" {
    fn getrlimit(resource: c_int, rlim: *mut Rlimit) -> c_int;
    fn setrlimit(resource: c_int, rlim: *const Rlimit) -> c_int;
}

static LIMIT_GATE: Mutex<()> = Mutex::new(());
static UNIQUE: AtomicU32 = AtomicU32::new(0);

fn memlock_limit() -> Rlimit {
    let mut limit = Rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a live, writable `Rlimit` for the call's duration.
    let rc = unsafe { getrlimit(RLIMIT_MEMLOCK, &raw mut limit) };
    assert_eq!(rc, 0, "getrlimit(RLIMIT_MEMLOCK) succeeds");
    limit
}

/// Holds the limit gate and restores the ambient soft limit on drop, so a
/// panicking case cannot leak a lowered limit into the next one.
struct MemlockLimitGuard {
    _gate: MutexGuard<'static, ()>,
    ambient: Rlimit,
}

impl MemlockLimitGuard {
    fn acquire() -> Self {
        let gate = LIMIT_GATE.lock().unwrap_or_else(PoisonError::into_inner);
        Self {
            _gate: gate,
            ambient: memlock_limit(),
        }
    }

    #[cfg(target_os = "linux")]
    fn lower_soft_to(&self, bytes: u64) {
        assert!(
            bytes <= self.ambient.rlim_max,
            "the floor stays within the hard limit"
        );
        let lowered = Rlimit {
            rlim_cur: bytes,
            rlim_max: self.ambient.rlim_max,
        };
        // SAFETY: `lowered` is a live `Rlimit` for the call's duration.
        let rc = unsafe { setrlimit(RLIMIT_MEMLOCK, &raw const lowered) };
        assert_eq!(rc, 0, "lowering the soft RLIMIT_MEMLOCK succeeds");
        assert_eq!(
            memlock_limit().rlim_cur,
            bytes,
            "the lowered soft limit reads back"
        );
    }
}

impl Drop for MemlockLimitGuard {
    fn drop(&mut self) {
        // SAFETY: `self.ambient` is a live `Rlimit` for the call's duration.
        let rc = unsafe { setrlimit(RLIMIT_MEMLOCK, &raw const self.ambient) };
        assert_eq!(rc, 0, "restoring the ambient RLIMIT_MEMLOCK succeeds");
    }
}

fn temp_path(tag: &str) -> PathBuf {
    let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("target temp directory");
    path.push(format!(
        "arena-registration-{tag}-{}-{sequence}",
        std::process::id()
    ));
    path
}

fn builder() -> PoolBuilder {
    Pool::builder()
        .frame_count(FRAME_COUNT)
        .granule(GRANULE)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(2)
        .miss_headroom(6)
        .write_slots(WRITE_SLOTS)
        .max_inflight_product_ops(1)
}

fn patterned_extent(seed: u8) -> Vec<u8> {
    (0..GRANULE)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

fn extent_seed(extent: u32) -> u8 {
    u8::try_from(extent).expect("the fixture holds fewer than 256 extents")
}

fn patterned_file(path: &Path) {
    let mut bytes = Vec::with_capacity((GRANULE * EXTENTS) as usize);
    for extent in 0..EXTENTS {
        bytes.extend_from_slice(&patterned_extent(extent_seed(extent)));
    }
    std::fs::write(path, bytes).expect("write the patterned fixture");
}

fn pending(outcome: Result<Get<'_>, GetError>) -> PendingToken {
    match outcome.expect("the registered file is live") {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a first lookup of an uncached extent cannot hit"),
        Get::Busy => panic!("the configured pool has miss headroom"),
    }
}

fn ready<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    mut token: PendingToken,
) -> FrameGuard<'pool> {
    for _ in 0..POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return guard,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(error) => panic!("a complete file extent must read: {error}"),
        }
    }
    panic!("the real backend did not complete within the bounded poll budget");
}

fn read_extent(pool: &Pool, reader: &ReaderCtx, file: FileId, extent: u32) {
    let page = PageId::new(file, extent);
    let guard = match pool.get(reader, page).expect("the registered file is live") {
        Get::Hit(guard) => guard,
        Get::Pending(token) => ready(pool, reader, token),
        Get::Busy => panic!("the configured pool has miss headroom"),
    };
    assert_eq!(
        &guard[..],
        patterned_extent(extent_seed(extent)).as_slice(),
        "extent {extent} bytes land intact under the selected posture"
    );
}

/// The round trip every posture must pass: reads land the file's bytes, and a
/// pending read whose token is dropped before completion recycles its frame
/// only after the completion is reaped (INV-4 by quiesce-before-free, not by
/// the buffer table).
fn assert_reads_land(pool: &Pool, path: &Path) {
    let file = pool
        .open(path, DirectIo::Preferred)
        .expect("the pool opens and retains the fixture");
    let reader = pool.register_reader().expect("one reader slot");
    let abandoned = pending(pool.get(&reader, PageId::new(file, 0)));
    drop(abandoned);
    for extent in 1..EXTENTS {
        read_extent(pool, &reader, file, extent);
    }
    read_extent(pool, &reader, file, 0);
    for extent in 0..EXTENTS {
        read_extent(pool, &reader, file, extent);
    }
}

#[test]
fn ambient_limit_selects_registered_when_the_arena_fits() {
    let guard = MemlockLimitGuard::acquire();
    if guard.ambient.rlim_cur < ARENA_BYTES {
        eprintln!("skipped: the ambient RLIMIT_MEMLOCK admits no registered posture");
        return;
    }
    let pool = builder()
        .build()
        .expect("an Auto build never fails on the limit");
    #[cfg(target_os = "linux")]
    assert_eq!(
        pool.registration_posture(),
        RegistrationPosture::Registered,
        "Auto registers when the limit admits the arena"
    );
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        pool.registration_posture(),
        RegistrationPosture::Unregistered,
        "the eager backend has one posture"
    );
    assert!(
        pool.arena_locked(),
        "the arena locks when the limit admits it"
    );
    let path = temp_path("ambient");
    patterned_file(&path);
    assert_reads_land(&pool, &path);
    drop(pool);
    let _ = std::fs::remove_file(&path);
}

#[cfg(target_os = "linux")]
#[test]
fn auto_degrades_to_unregistered_at_the_memlock_floor() {
    let guard = MemlockLimitGuard::acquire();
    guard.lower_soft_to(MEMLOCK_FLOOR_BYTES);
    let pool = builder()
        .registration_posture(RegistrationPolicy::Auto)
        .build()
        .expect("Auto never fails on ENOMEM");
    assert_eq!(
        pool.registration_posture(),
        RegistrationPosture::Unregistered,
        "Auto degrades when registration is refused"
    );
    assert!(
        !pool.arena_locked(),
        "locking charges the same limit and is refused at the floor"
    );
    let path = temp_path("auto-floor");
    patterned_file(&path);
    assert_reads_land(&pool, &path);
    drop(pool);
    let _ = std::fs::remove_file(&path);
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_unregistered_builds_at_the_memlock_floor() {
    let guard = MemlockLimitGuard::acquire();
    guard.lower_soft_to(MEMLOCK_FLOOR_BYTES);
    let pool = builder()
        .registration_posture(RegistrationPolicy::Unregistered)
        .build()
        .expect("an unregistered arena charges nothing");
    assert_eq!(
        pool.registration_posture(),
        RegistrationPosture::Unregistered
    );
    assert!(!pool.arena_locked());
    let path = temp_path("unregistered-floor");
    patterned_file(&path);
    assert_reads_land(&pool, &path);
    drop(pool);
    let _ = std::fs::remove_file(&path);
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_registered_is_refused_typed_at_the_memlock_floor() {
    let guard = MemlockLimitGuard::acquire();
    guard.lower_soft_to(MEMLOCK_FLOOR_BYTES);
    let Err(error) = builder()
        .registration_posture(RegistrationPolicy::Registered)
        .build()
    else {
        panic!("an explicit posture the limit refuses is a typed build error");
    };
    let PoolBuildError::Configuration(PoolConfigError::RegistrationRefused {
        arena_bytes,
        memlock_limit_bytes,
    }) = error
    else {
        panic!("expected a registration refusal, got {error:?}");
    };
    assert_eq!(
        arena_bytes, ARENA_BYTES,
        "the refusal names the charged span"
    );
    assert_eq!(
        memlock_limit_bytes, MEMLOCK_FLOOR_BYTES,
        "the refusal names the limit that refused it"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn require_locked_is_refused_typed_at_the_memlock_floor() {
    let guard = MemlockLimitGuard::acquire();
    guard.lower_soft_to(MEMLOCK_FLOOR_BYTES);
    let Err(error) = builder()
        .registration_posture(RegistrationPolicy::Unregistered)
        .require_locked()
        .build()
    else {
        panic!("a required lock the limit refuses is a typed build error");
    };
    let PoolBuildError::Configuration(PoolConfigError::ArenaLockRefused {
        arena_bytes,
        memlock_limit_bytes,
    }) = error
    else {
        panic!("expected an arena lock refusal, got {error:?}");
    };
    assert_eq!(arena_bytes, ARENA_BYTES);
    assert_eq!(memlock_limit_bytes, MEMLOCK_FLOOR_BYTES);
}

#[test]
fn require_locked_succeeds_when_the_ambient_limit_admits_the_arena() {
    let guard = MemlockLimitGuard::acquire();
    if guard.ambient.rlim_cur < ARENA_BYTES {
        eprintln!("skipped: the ambient RLIMIT_MEMLOCK admits no locked arena");
        return;
    }
    let pool = builder()
        .registration_posture(RegistrationPolicy::Unregistered)
        .require_locked()
        .build()
        .expect("the ambient limit admits the lock");
    assert!(pool.arena_locked(), "a required lock reads back locked");
    assert_eq!(
        pool.registration_posture(),
        RegistrationPosture::Unregistered
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn explicit_registered_is_refused_by_the_eager_backend() {
    let _guard = MemlockLimitGuard::acquire();
    let Err(error) = builder()
        .registration_posture(RegistrationPolicy::Registered)
        .build()
    else {
        panic!("the eager backend registers no buffers");
    };
    assert!(
        matches!(
            error,
            PoolBuildError::Configuration(PoolConfigError::RegistrationUnsupported)
        ),
        "expected an unsupported-registration refusal, got {error:?}"
    );
}
