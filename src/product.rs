//! Closed product-level operation, progress, and wake vocabulary.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use std::mem::size_of;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

#[cfg(target_os = "linux")]
const EFD_CLOEXEC: i32 = 0o2_000_000;
#[cfg(target_os = "linux")]
const EFD_NONBLOCK: i32 = 0o4_000;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn eventfd(initval: u32, flags: i32) -> i32;
    fn read(fd: i32, buf: *mut core::ffi::c_void, count: usize) -> isize;
    fn write(fd: i32, buf: *const core::ffi::c_void, count: usize) -> isize;
}

use crate::driver::FileId;
use crate::error::IoError;
use crate::pool::write_arena::{ArenaState, WriteSlot};

/// Pool-level submission backpressure or capability mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoolSubmitError {
    /// Every configured product-operation slot remains occupied.
    Full,
    /// The exact file generation is retired or has been reused.
    StaleFile { file: FileId },
    /// A staging slot was minted by another pool.
    ForeignPool,
}

impl std::fmt::Display for PoolSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("product operation capacity full"),
            Self::StaleFile { file } => write!(f, "stale product file {file:?}"),
            Self::ForeignPool => f.write_str("write slot belongs to another pool"),
        }
    }
}

impl std::error::Error for PoolSubmitError {}

/// Opaque pool operation slot and generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolToken(pub(crate) u64);

impl PoolToken {
    pub(crate) fn new(slot: u32, generation: u32) -> Self {
        Self((u64::from(generation) << 32) | u64::from(slot))
    }
}

/// Caller-owned terminal product result.
#[derive(Debug)]
pub enum PoolCompletion {
    /// A positional write result.
    Write {
        token: PoolToken,
        result: Result<u32, IoError>,
    },
    /// A durability barrier result.
    Fsync {
        token: PoolToken,
        result: Result<(), IoError>,
    },
}

/// Fixed-capacity delivery batch. Capacity zero requests progress only.
#[derive(Debug)]
pub struct PoolCompletionBatch {
    items: Vec<PoolCompletion>,
}

impl PoolCompletionBatch {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity),
        }
    }

    #[expect(
        clippy::iter_without_into_iter,
        reason = "the frozen product surface intentionally exposes only explicit borrowed iteration"
    )]
    pub fn iter(&self) -> std::slice::Iter<'_, PoolCompletion> {
        self.items.iter()
    }

    pub(crate) fn reset(&mut self) {
        self.items.clear();
    }

    pub(crate) fn push(&mut self, completion: PoolCompletion) {
        assert!(self.items.len() < self.items.capacity());
        self.items.push(completion);
    }

    pub(crate) fn remaining(&self) -> usize {
        self.items.capacity() - self.items.len()
    }
}

/// Truthful progress from one pool pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollReport {
    backend_completions: u32,
    reclaimed_frames: u32,
}

impl PollReport {
    pub(crate) fn new(backend_completions: u32, reclaimed_frames: u32) -> Self {
        Self {
            backend_completions,
            reclaimed_frames,
        }
    }

    #[must_use]
    pub fn backend_completions(self) -> u32 {
        self.backend_completions
    }

    #[must_use]
    pub fn reclaimed_frames(self) -> u32 {
        self.reclaimed_frames
    }
}

/// Typed state of a deferred product file retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetireStatus {
    /// At least one admitted capability or resource still drains.
    Retiring,
    /// Every capability, frame, completion, and backend file is gone.
    Retired,
}

/// Backend-erased borrowing view of this pool's staging slots.
#[derive(Debug, Clone, Copy)]
pub struct PoolWriteArena<'pool> {
    pub(crate) state: &'pool ArenaState,
    pub(crate) pool_identity: u64,
    pub(crate) enabled_slots: u32,
}

impl<'pool> PoolWriteArena<'pool> {
    #[must_use]
    pub fn alloc(&self) -> Option<PoolWriteSlot<'pool>> {
        self.state
            .alloc_within(self.enabled_slots)
            .map(|slot| PoolWriteSlot {
                slot,
                pool_identity: self.pool_identity,
            })
    }
}

/// Exclusive mutable lease of one product staging granule.
#[derive(Debug)]
pub struct PoolWriteSlot<'pool> {
    pub(crate) slot: WriteSlot<'pool>,
    pub(crate) pool_identity: u64,
}

impl Deref for PoolWriteSlot<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.slot
    }
}

impl DerefMut for PoolWriteSlot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.slot
    }
}

#[derive(Debug, Default)]
pub(crate) struct LifecycleCounters {
    pub(crate) registered_readers: AtomicU32,
    pub(crate) reader_releases: AtomicU32,
    pub(crate) live_pending_interests: AtomicU32,
    pub(crate) pending_releases: AtomicU32,
    pub(crate) backend_ops_in_flight: AtomicU32,
    pub(crate) backend_completions: AtomicU32,
    pub(crate) quiesce_calls: AtomicU32,
}

impl LifecycleCounters {
    pub(crate) fn register_reader(&self) {
        self.registered_readers.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn release_reader(&self) {
        self.registered_readers.fetch_sub(1, Ordering::AcqRel);
        self.reader_releases.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn register_pending(&self) {
        self.live_pending_interests.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn release_pending(&self) {
        self.live_pending_interests.fetch_sub(1, Ordering::AcqRel);
        self.pending_releases.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Default)]
struct WaitCounters {
    parks_entered: AtomicU32,
    parks_in_progress: AtomicU32,
    parks_exited: AtomicU32,
    wake_exits: AtomicU32,
    timeout_exits: AtomicU32,
}

#[derive(Debug, Default)]
struct WaitGeneration {
    current: u64,
    consumed: u64,
    ring_pending: bool,
}

/// One preallocated generation latch used by product I/O and external ingress.
#[derive(Debug, Default)]
pub(crate) struct WaitState {
    generation: Mutex<WaitGeneration>,
    ring_pending_hint: AtomicBool,
    changed: Condvar,
    counters: WaitCounters,
    #[cfg(target_os = "linux")]
    platform: std::sync::OnceLock<Arc<PlatformWake>>,
}

impl WaitState {
    fn lock(&self) -> MutexGuard<'_, WaitGeneration> {
        self.generation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn wake(&self) {
        let mut generation = self.lock();
        generation.current = generation.current.wrapping_add(1);
        drop(generation);
        self.changed.notify_all();
        #[cfg(target_os = "linux")]
        if let Some(platform) = self.platform.get() {
            platform.notify();
        }
    }

    pub(crate) fn wake_if_parked(&self) {
        let mut generation = self.lock();
        if self.counters.parks_in_progress.load(Ordering::Acquire) == 0 {
            return;
        }
        generation.current = generation.current.wrapping_add(1);
        self.changed.notify_all();
        #[cfg(target_os = "linux")]
        if let Some(platform) = self.platform.get() {
            platform.notify();
        }
        drop(generation);
    }

    pub(crate) fn set_ring_pending(&self) {
        let mut generation = self.lock();
        generation.ring_pending = true;
        self.ring_pending_hint.store(true, Ordering::Release);
    }

    pub(crate) fn clear_ring_pending(&self) {
        let mut generation = self.lock();
        generation.ring_pending = false;
        self.ring_pending_hint.store(false, Ordering::Release);
    }

    pub(crate) fn ring_may_be_pending(&self) -> bool {
        self.ring_pending_hint.load(Ordering::Acquire)
    }

    pub(crate) fn consume_current(&self) {
        let mut generation = self.lock();
        generation.consumed = generation.current;
    }

    pub(crate) fn wait(&self, timeout: Duration) {
        let mut generation = self.lock();
        if generation.current != generation.consumed {
            generation.consumed = generation.current;
            return;
        }
        let armed = generation.current;
        self.counters
            .parks_in_progress
            .fetch_add(1, Ordering::AcqRel);
        if generation.ring_pending {
            self.counters
                .parks_in_progress
                .fetch_sub(1, Ordering::AcqRel);
            return;
        }
        self.counters.parks_entered.fetch_add(1, Ordering::AcqRel);
        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        while generation.current == armed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                timed_out = true;
                break;
            }
            let (next, result) = self
                .changed
                .wait_timeout(generation, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            generation = next;
            if result.timed_out() && generation.current == armed {
                timed_out = true;
                break;
            }
        }
        if !timed_out {
            generation.consumed = generation.current;
        }
        self.counters
            .parks_in_progress
            .fetch_sub(1, Ordering::AcqRel);
        self.counters.parks_exited.fetch_add(1, Ordering::AcqRel);
        if timed_out {
            self.counters.timeout_exits.fetch_add(1, Ordering::AcqRel);
        } else {
            self.counters.wake_exits.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn attach_platform(&self, platform: Arc<PlatformWake>) {
        if let Err(platform) = self.platform.set(platform) {
            assert!(
                Arc::ptr_eq(
                    self.platform
                        .get()
                        .expect("an occupied platform slot has a value"),
                    &platform,
                ),
                "one Pool wait state attaches to one platform wake source"
            );
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn begin_platform_wait(&self) -> Option<u64> {
        let mut generation = self.lock();
        if generation.current != generation.consumed {
            generation.consumed = generation.current;
            return None;
        }
        let armed = generation.current;
        self.counters
            .parks_in_progress
            .fetch_add(1, Ordering::AcqRel);
        if generation.ring_pending {
            self.counters
                .parks_in_progress
                .fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        self.counters.parks_entered.fetch_add(1, Ordering::AcqRel);
        Some(armed)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn platform_woken(&self, armed: u64) -> bool {
        self.lock().current != armed
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn finish_platform_wait(&self, armed: u64) {
        let mut generation = self.lock();
        let woken = generation.current != armed;
        if woken {
            generation.consumed = generation.current;
        }
        drop(generation);
        self.counters
            .parks_in_progress
            .fetch_sub(1, Ordering::AcqRel);
        self.counters.parks_exited.fetch_add(1, Ordering::AcqRel);
        if woken {
            self.counters.wake_exits.fetch_add(1, Ordering::AcqRel);
        } else {
            self.counters.timeout_exits.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Private Linux event source shared by a pool wake handle and its `io_uring`
/// backend. The owned descriptor may outlive the ring when a wake handle does.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct PlatformWake(OwnedFd);

#[cfg(target_os = "linux")]
impl PlatformWake {
    pub(crate) fn new() -> std::io::Result<Arc<Self>> {
        // SAFETY: `eventfd` has no pointer arguments; the constant flags are
        // Linux ABI values and the returned descriptor is checked below.
        let raw = unsafe { eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a successful `eventfd` call returns a new owned descriptor,
        // transferred exactly once into `OwnedFd`.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Arc::new(Self(owned)))
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub(crate) fn notify(&self) {
        let value = 1u64;
        let eventfd_bytes =
            isize::try_from(size_of::<u64>()).expect("a u64 byte width always fits isize");
        loop {
            // SAFETY: the pointer addresses one live `u64`, and eventfd accepts
            // an exact eight-byte write for the owned descriptor's lifetime.
            let written = unsafe {
                write(
                    self.raw_fd(),
                    (&raw const value).cast::<core::ffi::c_void>(),
                    size_of::<u64>(),
                )
            };
            if written == eventfd_bytes {
                return;
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock => return,
                _ => panic!("eventfd wake write failed: {error}"),
            }
        }
    }

    pub(crate) fn drain(&self) {
        let mut value = 0u64;
        let eventfd_bytes =
            isize::try_from(size_of::<u64>()).expect("a u64 byte width always fits isize");
        loop {
            // SAFETY: the pointer addresses one live writable `u64`, and
            // eventfd returns one exact counter or a checked error.
            let read_bytes = unsafe {
                read(
                    self.raw_fd(),
                    (&raw mut value).cast::<core::ffi::c_void>(),
                    size_of::<u64>(),
                )
            };
            if read_bytes == eventfd_bytes {
                return;
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock => return,
                _ => panic!("eventfd wake read failed: {error}"),
            }
        }
    }
}

/// Cloneable, thread-safe external product wake capability.
#[derive(Debug, Clone)]
pub struct PoolWakeHandle {
    pub(crate) state: Arc<WaitState>,
}

impl PoolWakeHandle {
    pub fn wake(&self) {
        self.state.wake();
    }
}

/// Read-only wait-hook observation used by deterministic tests.
#[derive(Debug, Clone)]
pub(crate) struct WaitObservation {
    pub(crate) state: Arc<WaitState>,
}

impl WaitObservation {
    #[must_use]
    pub(crate) fn wait_until_parked(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.parks_in_progress() > 0 {
                return true;
            }
            std::thread::yield_now();
        }
        false
    }

    #[must_use]
    pub(crate) fn parks_entered(&self) -> u32 {
        self.state.counters.parks_entered.load(Ordering::Acquire)
    }

    #[must_use]
    pub(crate) fn parks_in_progress(&self) -> u32 {
        self.state
            .counters
            .parks_in_progress
            .load(Ordering::Acquire)
    }

    #[must_use]
    pub(crate) fn parks_exited(&self) -> u32 {
        self.state.counters.parks_exited.load(Ordering::Acquire)
    }

    #[must_use]
    pub(crate) fn wake_exits(&self) -> u32 {
        self.state.counters.wake_exits.load(Ordering::Acquire)
    }

    #[must_use]
    pub(crate) fn timeout_exits(&self) -> u32 {
        self.state.counters.timeout_exits.load(Ordering::Acquire)
    }
}

#[cfg(all(test, feature = "mock"))]
mod ring_pending_tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{WaitObservation, WaitState};
    use crate::testing::{FrameState, MockDriver, PoolBuilderTestingExt, PoolTestingExt};
    use crate::{DirectIo, Get, PageId, Pool};

    fn observation(wait: &Arc<WaitState>) -> WaitObservation {
        WaitObservation {
            state: Arc::clone(wait),
        }
    }

    fn assert_never_parked(observation: &WaitObservation) {
        assert_eq!(observation.parks_entered(), 0);
        assert_eq!(observation.parks_in_progress(), 0);
        assert_eq!(observation.parks_exited(), 0);
        assert_eq!(observation.wake_exits(), 0);
        assert_eq!(observation.timeout_exits(), 0);
    }

    #[test]
    fn held_final_release_without_a_parker_sets_ring_pending() {
        let mock = MockDriver::builder()
            .queue_capacity(1)
            .frames(6)
            .frame_bytes(4096)
            .build();
        let file = mock
            .open(Path::new("ring-pending-held-release"), DirectIo::Disabled)
            .expect("mock file opens");
        let file_id = file.file_id();
        let pool = Pool::builder()
            .frame_count(6)
            .granule(4096)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(2)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .max_retained_frames(1)
            .build_on(mock)
            .expect("retention fixture satisfies its watermark");
        pool.register_file(file);
        let reader = pool.register_reader().expect("reader slot is available");
        let page = PageId::new(file_id, 0);
        let frame = pool.insert_resident_frame(page, 0xA5);
        let Get::Hit(guard) = pool.get(&reader, page).expect("inserted page remains live") else {
            panic!("the inserted page must hit");
        };
        let Ok(retained) = guard.into_retained() else {
            panic!("the configured budget must admit one retained frame");
        };
        assert_eq!(pool.evict_frame(page), frame);
        for _ in 0..4 {
            pool.poll();
        }
        assert_eq!(pool.frame_state(frame), FrameState::Evicting);
        let wait = pool.wait_internal();
        let observation = observation(&wait);
        assert_never_parked(&observation);

        drop(retained);

        assert!(wait.lock().ring_pending);
        assert_never_parked(&observation);
        pool.poll();
    }

    #[test]
    fn eager_wait_does_not_park_when_a_ring_release_is_pending() {
        let wait = Arc::new(WaitState::default());
        let observation = observation(&wait);
        wait.set_ring_pending();

        wait.wait(Duration::from_millis(10));

        assert_never_parked(&observation);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn platform_wait_is_not_armed_when_a_ring_release_is_pending() {
        let wait = Arc::new(WaitState::default());
        let observation = observation(&wait);
        wait.set_ring_pending();

        assert_eq!(wait.begin_platform_wait(), None);

        assert_never_parked(&observation);
    }
}
