//! Fixed retention bookkeeping and the last-drop release ring.

use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::Arc;

use crate::driver::MAX_FILES;
use crate::product::WaitState;
#[cfg(test)]
use crate::sync::Mutex;
use crate::sync::{AtomicBool, AtomicU32, Ordering};

use super::{FrameGuard, ReadFrameIdx, epoch::FrameOutcome};

const HELD: u32 = 1 << 16;
const COUNT_MASK: u32 = u16::MAX as u32;
const RING_CAPACITY_MAX: u32 = 1 << 31;

pub struct RetainedFrame<'pool> {
    bytes: &'pool [u8],
    frame: ReadFrameIdx,
    retention: &'pool Retention,
    _thread_bound: PhantomData<*const ()>,
}

impl fmt::Debug for RetainedFrame<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedFrame")
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl Deref for RetainedFrame<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes
    }
}

impl Drop for RetainedFrame<'_> {
    fn drop(&mut self) {
        self.retention.release(self.frame);
    }
}

pub struct RetainRefused<'pool> {
    pub guard: FrameGuard<'pool>,
    pub reason: RetainRefusedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainRefusedReason {
    Exhausted,
    FileRetiring,
}

pub struct RetentionStats {
    pub occupied_budget: u32,
    pub refused_budget: u64,
    pub refused_ceiling: u64,
    pub refused_contention: u64,
    pub refused_retiring: u64,
    pub retained_evictions_held: u64,
}

enum Promotion {
    Retained,
    Retry,
    Refused(RetainRefusedReason),
}

impl<'pool> FrameGuard<'pool> {
    /// # Errors
    ///
    /// Returns the still-live guard when retention is refused.
    pub fn into_retained(self) -> Result<RetainedFrame<'pool>, RetainRefused<'pool>> {
        match self.retention.promote(
            self.frame,
            self.file_slot,
            self.retention.max_concurrent_readers,
        ) {
            Ok(()) => {
                let guard = ManuallyDrop::new(self);
                guard.slot.release_guard();
                Ok(RetainedFrame {
                    bytes: guard.bytes,
                    frame: guard.frame,
                    retention: guard.retention,
                    _thread_bound: PhantomData,
                })
            }
            Err(reason) => Err(RetainRefused {
                guard: self,
                reason,
            }),
        }
    }
}

#[derive(Debug)]
struct ReleaseSlot {
    sequence: crate::sync::AtomicU64,
    frame: AtomicU32,
}

/// Bounded multi-producer, single-consumer queue of frame last-drops.
#[derive(Debug)]
struct ReleaseRing {
    slots: Box<[ReleaseSlot]>,
    tail: crate::sync::AtomicU64,
    capacity: u64,
    mask: u64,
}

impl ReleaseRing {
    fn preallocated(max_retained_frames: u32) -> Self {
        assert!(
            max_retained_frames > 0,
            "a release ring needs a positive budget"
        );
        assert!(
            max_retained_frames <= RING_CAPACITY_MAX,
            "a release ring capacity must remain representable"
        );
        let capacity = u64::from(max_retained_frames.next_power_of_two().max(2));
        let slots = (0..capacity)
            .map(|index| ReleaseSlot {
                sequence: crate::sync::AtomicU64::new(index),
                frame: AtomicU32::new(0),
            })
            .collect();
        Self {
            slots,
            tail: crate::sync::AtomicU64::new(0),
            capacity,
            mask: capacity - 1,
        }
    }

    fn push(&self, frame: u32) {
        let ticket = self.tail.fetch_add(1, Ordering::AcqRel);
        assert!(ticket != u64::MAX, "release-ring ticket never wraps");
        let index = usize::try_from(ticket & self.mask)
            .expect("a representable ring capacity fits the platform index width");
        let slot = &self.slots[index];
        assert_eq!(
            slot.sequence.load(Ordering::Acquire),
            ticket,
            "release-ring capacity proof keeps a claimed slot free"
        );
        slot.frame.store(frame, Ordering::Relaxed);
        slot.sequence.store(ticket + 1, Ordering::Release);
    }

    fn pop(&self, cursor: &mut u64) -> Option<u32> {
        assert!(*cursor != u64::MAX, "release-ring cursor never wraps");
        let index = usize::try_from(*cursor & self.mask)
            .expect("a representable ring capacity fits the platform index width");
        let slot = &self.slots[index];
        if slot.sequence.load(Ordering::Acquire) != *cursor + 1 {
            return None;
        }
        let frame = slot.frame.load(Ordering::Relaxed);
        assert!(
            *cursor <= u64::MAX - self.capacity,
            "release-ring slot sequence never wraps"
        );
        slot.sequence
            .store(*cursor + self.capacity, Ordering::Release);
        *cursor += 1;
        Some(frame)
    }
}

/// Preallocated retention bookkeeping. Promotion and frame reclamation join this
/// state in later scope tasks; this task establishes its fixed storage and drain.
#[derive(Debug)]
pub(crate) struct Retention {
    words: Box<[AtomicU32]>,
    tags: Box<[crate::sync::AtomicU64]>,
    retiring: Box<[AtomicBool]>,
    occupied_budget: AtomicU32,
    refused_budget: std::sync::atomic::AtomicU64, // refused_budget
    refused_ceiling: std::sync::atomic::AtomicU64, // refused_ceiling
    refused_contention: std::sync::atomic::AtomicU64, // refused_contention
    refused_retiring: std::sync::atomic::AtomicU64, // refused_retiring
    retained_evictions_held: std::sync::atomic::AtomicU64, // retained_evictions_held
    release_ring: Option<ReleaseRing>,
    wait: Arc<WaitState>,
    max_retained_frames: u32,
    max_concurrent_readers: u32,
    frame_count: u32,
    #[cfg(test)]
    release_drain_test_hook: Mutex<Option<ReleaseDrainTestHook>>,
    #[cfg(all(test, not(loom)))]
    promotion_test_hook: Mutex<Option<PromotionTestHook>>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct ReleaseDrainTestHook {
    pending_cleared: Arc<std::sync::Barrier>,
    release_published: Arc<std::sync::Barrier>,
}

#[cfg(all(test, not(loom)))]
const PROMOTION_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(all(test, not(loom)))]
#[derive(Debug, Clone)]
struct PromotionTestHook {
    target: std::thread::ThreadId,
    paused: std::sync::mpsc::Sender<()>,
    resume: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}

impl Retention {
    pub(crate) fn preallocated(
        frame_count: u32,
        max_retained_frames: u32,
        max_concurrent_readers: u32,
        wait: Arc<WaitState>,
    ) -> Self {
        if max_retained_frames == 0 {
            return Self::disabled(frame_count, max_concurrent_readers, wait);
        }
        let words = (0..frame_count).map(|_| AtomicU32::new(0)).collect();
        let tags = (0..frame_count)
            .map(|_| crate::sync::AtomicU64::new(0))
            .collect();
        let retiring = (0..MAX_FILES).map(|_| AtomicBool::new(false)).collect();
        Self {
            words,
            tags,
            retiring,
            occupied_budget: AtomicU32::new(0),
            refused_budget: std::sync::atomic::AtomicU64::new(0), // refused_budget
            refused_ceiling: std::sync::atomic::AtomicU64::new(0), // refused_ceiling
            refused_contention: std::sync::atomic::AtomicU64::new(0), // refused_contention
            refused_retiring: std::sync::atomic::AtomicU64::new(0), // refused_retiring
            retained_evictions_held: std::sync::atomic::AtomicU64::new(0), // retained_evictions_held
            release_ring: Some(ReleaseRing::preallocated(max_retained_frames)),
            wait,
            max_retained_frames,
            max_concurrent_readers,
            frame_count,
            #[cfg(test)]
            release_drain_test_hook: Mutex::new(None),
            #[cfg(all(test, not(loom)))]
            promotion_test_hook: Mutex::new(None),
        }
    }

    fn disabled(frame_count: u32, max_concurrent_readers: u32, wait: Arc<WaitState>) -> Self {
        Self {
            words: Box::new([]),
            tags: Box::new([]),
            retiring: Box::new([]),
            occupied_budget: AtomicU32::new(0),
            refused_budget: std::sync::atomic::AtomicU64::new(0), // refused_budget
            refused_ceiling: std::sync::atomic::AtomicU64::new(0), // refused_ceiling
            refused_contention: std::sync::atomic::AtomicU64::new(0), // refused_contention
            refused_retiring: std::sync::atomic::AtomicU64::new(0), // refused_retiring
            retained_evictions_held: std::sync::atomic::AtomicU64::new(0), // retained_evictions_held
            release_ring: None,
            wait,
            max_retained_frames: 0,
            max_concurrent_readers,
            frame_count,
            #[cfg(test)]
            release_drain_test_hook: Mutex::new(None),
            #[cfg(all(test, not(loom)))]
            promotion_test_hook: Mutex::new(None),
        }
    }

    pub(super) fn retention_stats(&self) -> RetentionStats {
        RetentionStats {
            occupied_budget: self.occupied_budget.load(Ordering::Acquire),
            refused_budget: self.refused_budget.load(Ordering::Relaxed),
            refused_ceiling: self.refused_ceiling.load(Ordering::Relaxed),
            refused_contention: self.refused_contention.load(Ordering::Relaxed),
            refused_retiring: self.refused_retiring.load(Ordering::Relaxed),
            retained_evictions_held: self.retained_evictions_held.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.max_retained_frames == 0
    }

    pub(crate) fn has_occupied_budget(&self) -> bool {
        self.occupied_budget.load(Ordering::Acquire) > 0
    }

    pub(crate) fn mark_file_retiring(&self, file_slot: u32) {
        if self.is_disabled() {
            return;
        }
        let index = usize::try_from(file_slot).expect("file slots fit platform indexes");
        assert!(
            index < self.retiring.len(),
            "retention covers every file slot"
        );
        self.retiring[index].store(true, Ordering::Release);
    }

    pub(crate) fn clear_file_retiring(&self, file_slot: u32) {
        if self.is_disabled() {
            return;
        }
        let index = usize::try_from(file_slot).expect("file slots fit platform indexes");
        assert!(
            index < self.retiring.len(),
            "retention covers every file slot"
        );
        self.retiring[index].store(false, Ordering::Release);
    }

    pub(crate) fn drain_releases<F: FnMut(ReadFrameIdx)>(
        &self,
        cursor: &mut u64,
        pass_start_epoch: u64,
        mut reclaim: F,
    ) {
        let ring = self
            .release_ring
            .as_ref()
            .expect("release drain requires enabled retention");
        self.clear_ring_pending_before_scan();
        for _ in 0..self.max_retained_frames {
            let Some(frame) = ring.pop(cursor) else {
                break;
            };
            let frame = ReadFrameIdx::new(frame);
            let tag = self.tag(frame).load(Ordering::Relaxed);
            assert!(
                tag.saturating_add(2) <= pass_start_epoch,
                "a release-ring tag matured before this reclaim pass"
            );
            let word = self.word(frame).load(Ordering::Acquire);
            assert_eq!(word & COUNT_MASK, 0, "a release-ring frame has no handles");
            assert_ne!(word & HELD, 0, "a release-ring frame remains held");
            self.word(frame)
                .compare_exchange(word, word & !HELD, Ordering::AcqRel, Ordering::Acquire)
                .expect("a held release-ring frame has no concurrent mutator");
            reclaim(frame);
            self.release_budget_unit();
        }
    }

    pub(crate) fn matured_outcome(&self, frame: ReadFrameIdx, tag: u64) -> FrameOutcome {
        if self.is_disabled() {
            return FrameOutcome::Freed;
        }
        let word = self.word(frame);
        let mut previous = word.load(Ordering::Acquire);
        let attempts = previous & COUNT_MASK;
        if attempts == 0 {
            assert_eq!(
                previous & HELD,
                0,
                "an evict queue entry is not already held"
            );
            return FrameOutcome::Freed;
        }
        self.tag(frame).store(tag, Ordering::Relaxed);
        for _ in 0..attempts {
            assert_eq!(previous & HELD, 0, "only the mature drain sets held");
            let count = previous & COUNT_MASK;
            assert!(
                count > 0,
                "the bounded held transition starts with a handle"
            );
            match word.compare_exchange(
                previous,
                previous | HELD,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.retained_evictions_held.fetch_add(1, Ordering::Relaxed);
                    return FrameOutcome::Held;
                }
                Err(observed) => {
                    assert!(
                        observed & COUNT_MASK < count,
                        "a mature-drain CAS failure is a completed handle drop"
                    );
                    if observed & COUNT_MASK == 0 {
                        assert_eq!(
                            observed & HELD,
                            0,
                            "a concurrent last drop cannot publish held"
                        );
                        return FrameOutcome::Freed;
                    }
                    previous = observed;
                }
            }
        }
        panic!("mature-drain retries are bounded by the strictly decreasing count");
    }

    fn release_budget_unit(&self) {
        let previous = self.occupied_budget.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "a release-ring entry owns one occupied unit");
    }

    fn promote(
        &self,
        frame: ReadFrameIdx,
        file_slot: u32,
        max_concurrent_readers: u32,
    ) -> Result<(), RetainRefusedReason> {
        if self.is_disabled() {
            self.refused_budget.fetch_add(1, Ordering::Relaxed);
            return Err(RetainRefusedReason::Exhausted);
        }
        let attempts = max_concurrent_readers
            .checked_add(1)
            .expect("retention validation bounds promotion attempts");
        for _ in 0..attempts {
            match self.promote_once(frame) {
                Promotion::Retained => return self.reject_retiring(frame, file_slot),
                Promotion::Retry => {}
                Promotion::Refused(reason) => return Err(reason),
            }
        }
        self.refused_contention.fetch_add(1, Ordering::Relaxed);
        Err(RetainRefusedReason::Exhausted)
    }

    fn promote_once(&self, frame: ReadFrameIdx) -> Promotion {
        let word = self.word(frame);
        let previous = word.load(Ordering::Acquire);
        assert_eq!(previous & HELD, 0, "a guard never promotes a held frame");
        let count = previous & COUNT_MASK;
        if count == COUNT_MASK {
            self.refused_ceiling.fetch_add(1, Ordering::Relaxed);
            return Promotion::Refused(RetainRefusedReason::Exhausted);
        }
        #[cfg(all(test, not(loom)))]
        self.pause_promotion_for_test();
        if count > 0 {
            return Self::compare_increment(word, previous);
        }
        self.reserve_then_promote(word)
    }

    fn compare_increment(word: &AtomicU32, previous: u32) -> Promotion {
        match word.compare_exchange(previous, previous + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Promotion::Retained,
            Err(_) => Promotion::Retry,
        }
    }

    fn reserve_then_promote(&self, word: &AtomicU32) -> Promotion {
        let previous = self.occupied_budget.fetch_add(1, Ordering::AcqRel);
        if previous >= self.max_retained_frames {
            self.release_budget_unit();
            if word.load(Ordering::Acquire) & COUNT_MASK > 0 {
                return Promotion::Retry;
            }
            self.refused_budget.fetch_add(1, Ordering::Relaxed);
            return Promotion::Refused(RetainRefusedReason::Exhausted);
        }
        if word
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Promotion::Retained
        } else {
            self.release_budget_unit();
            Promotion::Retry
        }
    }

    fn reject_retiring(
        &self,
        frame: ReadFrameIdx,
        file_slot: u32,
    ) -> Result<(), RetainRefusedReason> {
        let index = usize::try_from(file_slot).expect("file slots fit platform indexes");
        assert!(
            index < self.retiring.len(),
            "retention covers every file slot"
        );
        if !self.retiring[index].load(Ordering::Acquire) {
            return Ok(());
        }
        self.release(frame);
        self.refused_retiring.fetch_add(1, Ordering::Relaxed);
        Err(RetainRefusedReason::FileRetiring)
    }

    fn release(&self, frame: ReadFrameIdx) {
        let previous = self.word(frame).fetch_sub(1, Ordering::AcqRel);
        assert!(
            previous & COUNT_MASK > 0,
            "a retained handle owns one count"
        );
        let remaining = previous - 1;
        if remaining & COUNT_MASK > 0 {
            return;
        }
        if remaining & HELD == 0 {
            self.release_budget_unit();
            return;
        }
        let ring = self
            .release_ring
            .as_ref()
            .expect("held frames need a release ring");
        ring.push(frame.get());
        self.wait.set_ring_pending();
        self.wait.wake_if_parked();
    }

    fn word(&self, frame: ReadFrameIdx) -> &AtomicU32 {
        let index = usize::try_from(frame.get()).expect("frame indexes fit platform indexes");
        assert!(index < self.words.len(), "retention covers every frame");
        &self.words[index]
    }

    fn tag(&self, frame: ReadFrameIdx) -> &crate::sync::AtomicU64 {
        let index = usize::try_from(frame.get()).expect("frame indexes fit platform indexes");
        assert!(index < self.tags.len(), "retention tags cover every frame");
        &self.tags[index]
    }

    fn clear_ring_pending_before_scan(&self) {
        self.wait.clear_ring_pending();
        #[cfg(test)]
        self.wait_after_pending_clear_for_test();
    }

    #[cfg(all(test, not(loom)))]
    fn pause_promotion_for_test(&self) {
        let hook = self
            .promotion_test_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook
            && hook.target == std::thread::current().id()
        {
            hook.paused
                .send(())
                .expect("promotion coordinator receives each target attempt");
            let resumed = hook
                .resume
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv_timeout(PROMOTION_TEST_TIMEOUT);
            assert!(resumed.is_ok(), "promotion target resume is bounded");
        }
    }

    #[cfg(all(test, not(loom)))]
    fn install_promotion_test_hook(&self, hook: PromotionTestHook) {
        let mut installed = self
            .promotion_test_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            installed.is_none(),
            "a promotion test hook is installed once"
        );
        *installed = Some(hook);
    }

    #[cfg(test)]
    fn install_release_drain_test_hook(&self, hook: ReleaseDrainTestHook) {
        #[cfg(not(loom))]
        let mut installed = self
            .release_drain_test_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(loom)]
        let mut installed = self
            .release_drain_test_hook
            .lock()
            .expect("loom mutex is never poisoned");
        assert!(
            installed.is_none(),
            "a release-drain test hook is installed once"
        );
        *installed = Some(hook);
    }

    #[cfg(test)]
    fn wait_after_pending_clear_for_test(&self) {
        #[cfg(not(loom))]
        let hook = self
            .release_drain_test_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(loom)]
        let hook = self
            .release_drain_test_hook
            .lock()
            .expect("loom mutex is never poisoned")
            .clone();
        if let Some(hook) = hook {
            hook.pending_cleared.wait();
            hook.release_published.wait();
        }
    }
}

impl Drop for Retention {
    fn drop(&mut self) {
        assert_eq!(
            self.occupied_budget.load(Ordering::Acquire),
            0,
            "retention handles must not be forgotten"
        );
        assert_eq!(
            self.words.len(),
            self.tags.len(),
            "retention words and tags cover the same frames"
        );
        assert!(
            self.is_disabled() || self.words.len() == self.frame_count as usize,
            "enabled retention covers every frame"
        );
        assert!(
            self.is_disabled() || self.retiring.len() == MAX_FILES as usize,
            "enabled retention covers every file slot"
        );
        assert_eq!(HELD & COUNT_MASK, 0, "held and count bits never overlap");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    #[cfg(not(loom))]
    use std::sync::mpsc;

    use super::{
        COUNT_MASK, HELD, ReleaseDrainTestHook, ReleaseRing, RetainRefusedReason, Retention,
    };
    #[cfg(not(loom))]
    use super::{PROMOTION_TEST_TIMEOUT, Promotion, PromotionTestHook};
    use crate::pool::ReadFrameIdx;
    use crate::product::WaitState;
    #[cfg(not(loom))]
    use crate::sync::Mutex;
    use crate::sync::Ordering;

    #[test]
    fn promotion_at_count_ceiling_is_exhausted_without_mutation() {
        let wait = Arc::new(WaitState::default());
        let retention = Retention::preallocated(1, 1, 1, wait);
        let frame = ReadFrameIdx::new(0);
        retention.word(frame).store(COUNT_MASK, Ordering::Release);
        retention.occupied_budget.store(1, Ordering::Release);
        let word_before = retention.word(frame).load(Ordering::Acquire);
        let budget_before = retention.retention_stats().occupied_budget;

        assert!(matches!(
            retention.promote(frame, 0, 1),
            Err(RetainRefusedReason::Exhausted)
        ));
        let stats = retention.retention_stats();
        assert_eq!(retention.word(frame).load(Ordering::Acquire), word_before);
        assert_eq!(stats.occupied_budget, budget_before);
        assert_eq!(
            (
                stats.refused_budget,
                stats.refused_ceiling,
                stats.refused_contention,
                stats.refused_retiring,
                stats.retained_evictions_held,
            ),
            (0, 1, 0, 0, 0)
        );

        retention.word(frame).store(0, Ordering::Release);
        retention.occupied_budget.store(0, Ordering::Release);
    }

    #[cfg(not(loom))]
    #[test]
    fn bounded_same_frame_refusal_is_attributed_only_to_contention() {
        let wait = Arc::new(WaitState::default());
        let retention = Retention::preallocated(1, 1, 1, wait);
        let frame = ReadFrameIdx::new(0);
        assert!(retention.promote(frame, 0, 1).is_ok());
        let before = retention.retention_stats();
        let (paused_sender, paused) = mpsc::channel();
        let (resume, target_resume) = mpsc::channel();
        let (start, target_start) = mpsc::channel();
        let (target_results, results) = mpsc::channel();
        let mut observed = (0u32, 0u32);
        let (target_result, target_joined) = std::thread::scope(|scope| {
            let target_retention = &retention;
            let target = scope.spawn(move || {
                let started = target_start.recv_timeout(PROMOTION_TEST_TIMEOUT);
                assert!(started.is_ok(), "promotion target start is bounded");
                let result = target_retention.promote(frame, 0, 1);
                let _ = target_results.send(result);
            });
            retention.install_promotion_test_hook(PromotionTestHook {
                target: target.thread().id(),
                paused: paused_sender,
                resume: Arc::new(Mutex::new(target_resume)),
            });
            start.send(()).expect("promotion target starts");
            for _ in 0..2 {
                if paused.recv_timeout(PROMOTION_TEST_TIMEOUT).is_err() {
                    break;
                }
                observed.0 += 1;
                let promoted = matches!(retention.promote_once(frame), Promotion::Retained);
                observed.1 += u32::from(promoted);
                let resumed = resume.send(()).is_ok();
                if !promoted || !resumed {
                    break;
                }
            }
            drop(resume);
            let result = results.recv_timeout(PROMOTION_TEST_TIMEOUT);
            (result, target.join().is_ok())
        });
        let after = retention.retention_stats();
        let target_promoted = u32::from(matches!(&target_result, Ok(Ok(()))));
        for _ in 0..1 + observed.1 + target_promoted {
            retention.release(frame);
        }
        assert!(target_joined);
        assert_eq!(observed, (2, 2));
        let target_refused = matches!(target_result, Ok(Err(RetainRefusedReason::Exhausted)));
        assert!(target_refused);
        assert_eq!(after.occupied_budget, before.occupied_budget);
        assert_eq!(after.refused_budget, before.refused_budget);
        assert_eq!(after.refused_ceiling, before.refused_ceiling);
        assert_eq!(after.refused_contention, before.refused_contention + 1);
        assert_eq!(after.refused_retiring, before.refused_retiring);
        let held_delta = after.retained_evictions_held - before.retained_evictions_held;
        assert_eq!(held_delta, 0);
        assert_eq!(retention.retention_stats().occupied_budget, 0);
    }

    #[test]
    fn release_ring_pops_frames_in_ticket_order() {
        let ring = ReleaseRing::preallocated(2);
        let mut cursor = 0;
        ring.push(7);
        ring.push(3);
        assert_eq!(ring.pop(&mut cursor), Some(7));
        assert_eq!(ring.pop(&mut cursor), Some(3));
        assert_eq!(ring.pop(&mut cursor), None);
    }

    #[test]
    fn release_ring_stops_at_an_unpublished_ticket() {
        let ring = ReleaseRing::preallocated(2);
        let mut cursor = 0;
        let _ticket = ring.tail.fetch_add(1, Ordering::AcqRel);
        assert_eq!(ring.pop(&mut cursor), None);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn release_ring_reuses_a_slot_only_after_consumer_turnover() {
        let ring = ReleaseRing::preallocated(2);
        let mut cursor = 0;
        ring.push(1);
        ring.push(2);
        assert_eq!(ring.pop(&mut cursor), Some(1));
        ring.push(3);
        assert_eq!(ring.pop(&mut cursor), Some(2));
        assert_eq!(ring.pop(&mut cursor), Some(3));
    }

    #[test]
    fn release_published_after_pending_clear_is_reclaimed_before_the_same_scan() {
        let wait = Arc::new(WaitState::default());
        let retention = Retention::preallocated(1, 1, 1, Arc::clone(&wait));
        let frame = ReadFrameIdx::new(0);
        retention.word(frame).store(HELD | 1, Ordering::Release);
        retention.occupied_budget.store(1, Ordering::Release);

        let pending_cleared = Arc::new(std::sync::Barrier::new(2));
        let release_published = Arc::new(std::sync::Barrier::new(2));
        retention.install_release_drain_test_hook(ReleaseDrainTestHook {
            pending_cleared: Arc::clone(&pending_cleared),
            release_published: Arc::clone(&release_published),
        });

        std::thread::scope(|scope| {
            let drain = scope.spawn(|| {
                let mut cursor = 0;
                let mut reclaimed = 0;
                retention.drain_releases(&mut cursor, 2, |released| {
                    assert_eq!(released, frame);
                    reclaimed += 1;
                });
                reclaimed
            });

            pending_cleared.wait();
            retention.release(frame);
            release_published.wait();

            assert_eq!(
                drain.join().expect("release-drain thread does not panic"),
                1,
                "a release published after pending clear is drained before the same scan ends"
            );
        });
    }
}
