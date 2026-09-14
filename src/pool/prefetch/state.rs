use super::pattern::Pattern;
use super::{PageId, PrefetchStats, Readahead, Source};
use crate::pool::clock::SpeculationOutcome;
use crate::pool::{Clock, FreeFrames, ReadFrameIdx};

#[derive(Debug, Clone, Copy)]
pub(super) struct Entry {
    pub(super) page: PageId,
    pub(super) frame: ReadFrameIdx,
    source: Source,
}

#[derive(Debug, Clone, Copy)]
struct Feedback {
    reader: u32,
    incarnation: u64,
    page: PageId,
}

impl Feedback {
    fn key(self) -> (u32, u64, u32) {
        (self.reader, self.incarnation, self.page.granule_idx())
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use crate::driver::FileId;
    use crate::sync::{Arc, Mutex};

    #[test]
    fn concurrent_feedback_preserves_reader_order_across_recycled_slots() {
        let mut model = loom::model::Builder::new();
        model.max_branches = 2_000;
        model.preemption_bound = Some(2);
        model.max_permutations = Some(16_384);
        model.check(|| {
            let clock = Arc::new(Clock::with_frame_count(2));
            let registry = std::sync::Arc::new(
                crate::pool::epoch::ReaderRegistry::try_with_capacity(
                    1,
                    1,
                    std::sync::Arc::new(crate::product::LifecycleCounters::default()),
                )
                .expect("reader registry"),
            );
            let mut ledger = Prefetch::try_new(2, 1, 2, Readahead::Automatic).expect("ledger");
            let page = |index| PageId::new(FileId::new(0, 0, 0), index);
            for index in 0..3 {
                ledger.observe(&clock, 0, page(index));
            }
            let (_, source) = ledger.patterns[0].request(0).expect("stream");
            ledger.patterns[0].issued(page(3));
            ledger.patterns[0].issued(page(4));
            ledger.admit(&clock, page(4), ReadFrameIdx::new(0), source);
            ledger.admit(&clock, page(3), ReadFrameIdx::new(1), source);
            let ledger = Arc::new(Mutex::new(ledger));
            let consumer_clock = Arc::clone(&clock);
            let reader = loom::thread::spawn(move || {
                let reader = registry.register().expect("reader");
                assert!(consumer_clock.reference_from(ReadFrameIdx::new(1), reader.slot()));
                assert!(consumer_clock.reference_from(ReadFrameIdx::new(0), reader.slot()));
            });
            let poll_clock = Arc::clone(&clock);
            let poll_ledger = Arc::clone(&ledger);
            let poller = loom::thread::spawn(move || {
                poll_ledger.lock().expect("control").reconcile(&poll_clock);
            });
            reader.join().expect("reader");
            poller.join().expect("poller");
            let mut ledger = ledger.lock().expect("control");
            ledger.reconcile(&clock);
            assert_eq!(ledger.stats.demand_promoted, 2);
            assert_eq!(ledger.stats.occupied, 0);
            assert_eq!(
                ledger.patterns[0].request(0).expect("confirmed stream").0,
                page(5)
            );
        });
    }

    #[test]
    fn consumption_reconciliation_and_eviction_return_one_credit_before_reuse() {
        let mut model = loom::model::Builder::new();
        model.max_branches = 2_000;
        model.preemption_bound = Some(2);
        model.max_permutations = Some(16_384);
        model.check(|| {
            let clock = Arc::new(Clock::with_frame_count(1));
            let mut ledger = Prefetch::try_new(1, 0, 1, Readahead::Disabled).expect("ledger");
            let frame = ReadFrameIdx::new(0);
            let page = PageId::new(FileId::new(0, 0, 0), 7);
            ledger.admit(&clock, page, frame, Source::Explicit);
            let ledger = Arc::new(Mutex::new(ledger));
            let reader_clock = Arc::clone(&clock);
            let reader = loom::thread::spawn(move || {
                let _ = reader_clock.reference(frame);
            });
            let poll_clock = Arc::clone(&clock);
            let poll_ledger = Arc::clone(&ledger);
            let poller = loom::thread::spawn(move || {
                poll_ledger.lock().expect("control").reconcile(&poll_clock);
                poll_ledger
                    .lock()
                    .expect("control")
                    .finish(&poll_clock, frame, Terminal::Evicted);
            });
            reader.join().expect("reader");
            poller.join().expect("poller");
            let mut ledger = ledger.lock().expect("control");
            assert_eq!(ledger.stats.occupied, 0);
            assert_eq!(
                ledger.stats.demand_promoted + ledger.stats.evicted_unused,
                1
            );
            assert!(!clock.is_speculative(frame));
            // The EBR frame tests prove quiescence before reuse; both old users
            // have joined here, so this exercises the real marker reset at reuse.
            ledger.admit(&clock, page, frame, Source::Explicit);
            assert!(clock.reference(frame));
            ledger.reconcile(&clock);
            assert_eq!(ledger.stats.occupied, 0);
            assert_eq!(
                ledger.stats.demand_promoted + ledger.stats.evicted_unused,
                2
            );
        });
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::pool) enum Terminal {
    Promoted(Option<u32>),
    Evicted,
    Failed,
}

#[derive(Debug)]
pub(crate) struct Prefetch {
    pub(in crate::pool) reserve: FreeFrames,
    pub(super) entries: Box<[Option<Entry>]>,
    pub(super) patterns: Box<[Pattern]>,
    pub(super) stats: PrefetchStats,
    pub(super) capacity: u32,
    pub(super) request_limit: usize,
    pub(super) active: bool,
    pub(super) reader_cursor: u32,
    feedback: Box<[Option<Feedback>]>,
    feedback_count: u32,
}

impl Prefetch {
    pub(in crate::pool) fn try_new(
        capacity: u32,
        readers: u32,
        reads: u32,
        mode: Readahead,
    ) -> Option<Self> {
        let pattern_count = match mode {
            Readahead::Automatic if capacity > 0 => readers,
            Readahead::Automatic | Readahead::Disabled => 0,
        };
        Some(Self {
            reserve: FreeFrames::try_empty(capacity)?,
            entries: crate::allocation::try_boxed_slice_with(capacity, || None)?,
            patterns: crate::allocation::try_boxed_slice_with(pattern_count, Pattern::new)?,
            stats: PrefetchStats::default(),
            capacity,
            request_limit: capacity.max(reads).saturating_mul(4).max(1) as usize,
            active: false,
            reader_cursor: 0,
            feedback: crate::allocation::try_boxed_slice_with(
                if pattern_count == 0 { 0 } else { capacity },
                || None,
            )?,
            feedback_count: 0,
        })
    }

    pub(super) fn available(&self) -> u32 {
        self.capacity
            .checked_sub(self.stats.occupied)
            .expect("speculation stays within its credit bound")
    }

    pub(super) fn fill_reserve(&mut self, free: &mut FreeFrames) {
        if !self.active {
            return;
        }
        let target = self.available();
        for _ in self.reserve.len()..target {
            let Some(frame) = free.pop() else {
                break;
            };
            self.reserve.push(frame);
        }
    }

    pub(super) fn admit(
        &mut self,
        clock: &Clock,
        page: PageId,
        frame: ReadFrameIdx,
        source: Source,
    ) {
        assert_eq!(self.feedback_count, 0, "admission follows ordered feedback");
        assert!(self.available() > 0);
        let slot = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_none())
            .expect("a credit has a metadata slot");
        *slot = Some(Entry {
            page,
            frame,
            source,
        });
        clock.begin_speculation(frame);
        self.stats.admitted += 1;
        self.stats.occupied += 1;
        if matches!(source, Source::Automatic { .. }) {
            self.stats.automatic_admitted += 1;
        }
        assert!(self.stats.occupied <= self.capacity);
    }

    pub(in crate::pool) fn reconcile(&mut self, clock: &Clock) {
        // A later acquire can reveal an earlier consumption in a recycled slab
        // slot already scanned. Every nonempty pass removes at least one entry.
        for _ in 0..=self.capacity {
            let before = self.stats.occupied;
            self.reconcile_pass(clock);
            if self.stats.occupied == before {
                break;
            }
        }
        self.flush_feedback();
    }

    pub(super) fn reconcile_feedback(&mut self, clock: &Clock) {
        if self.feedback_count > 0 {
            self.reconcile(clock);
        }
    }

    fn reconcile_pass(&mut self, clock: &Clock) {
        for index in 0..self.entries.len() {
            let Some(entry) = self.entries[index] else {
                continue;
            };
            if clock.speculation_consumed(entry.frame) {
                self.finish_index(clock, index, Terminal::Promoted(None));
            }
        }
    }

    pub(in crate::pool) fn finish(
        &mut self,
        clock: &Clock,
        frame: ReadFrameIdx,
        terminal: Terminal,
    ) {
        if !clock.is_speculative(frame) {
            return;
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.frame == frame))
        {
            self.finish_index(clock, index, terminal);
        } else {
            assert!(!clock.is_speculative(frame));
        }
    }

    fn finish_index(&mut self, clock: &Clock, index: usize, terminal: Terminal) {
        let entry = self.entries[index]
            .take()
            .expect("a terminal transition owns one speculative credit");
        let outcome = clock.take_speculation(entry.frame);
        assert_ne!(outcome, SpeculationOutcome::Absent);
        self.stats.occupied = self
            .stats
            .occupied
            .checked_sub(1)
            .expect("one credit is returned once");
        let terminal = if let SpeculationOutcome::Consumed(reader) = outcome {
            Terminal::Promoted(reader)
        } else {
            terminal
        };
        match terminal {
            Terminal::Promoted(_) => self.stats.demand_promoted += 1,
            Terminal::Evicted => self.stats.evicted_unused += 1,
            Terminal::Failed => self.stats.failed += 1,
        }
        self.finish_feedback(entry, terminal);
        assert_eq!(
            self.stats.admitted,
            self.stats.demand_promoted
                + self.stats.evicted_unused
                + self.stats.failed
                + u64::from(self.stats.occupied)
        );
    }

    fn finish_feedback(&mut self, entry: Entry, terminal: Terminal) {
        if let Source::Automatic {
            reader,
            incarnation,
        } = entry.source
        {
            match terminal {
                Terminal::Promoted(Some(consumer)) if consumer == reader => {
                    let index = self.feedback_count as usize;
                    assert!(index < self.feedback.len());
                    assert!(self.feedback[index].is_none());
                    self.feedback[index] = Some(Feedback {
                        reader,
                        incarnation,
                        page: entry.page,
                    });
                    self.feedback_count += 1;
                }
                Terminal::Promoted(_) => {}
                Terminal::Evicted | Terminal::Failed => {
                    self.patterns[reader as usize].failed(incarnation);
                }
            }
        }
    }

    fn flush_feedback(&mut self) {
        let count = self.feedback_count as usize;
        for index in 1..count {
            for cursor in (1..=index).rev() {
                let before = self.feedback[cursor - 1].expect("feedback prefix");
                let after = self.feedback[cursor].expect("feedback prefix");
                if before.key() <= after.key() {
                    break;
                }
                self.feedback.swap(cursor - 1, cursor);
            }
        }
        for index in 0..count {
            let feedback = self.feedback[index].take().expect("feedback prefix");
            self.patterns[feedback.reader as usize].promoted(
                feedback.page,
                feedback.incarnation,
                self.capacity,
            );
        }
        self.feedback_count = 0;
    }

    pub(super) fn obsolete(&self, entry: Entry) -> bool {
        self.source_obsolete(entry.source)
    }

    pub(super) fn source_obsolete(&self, source: Source) -> bool {
        match source {
            Source::Explicit => false,
            Source::Automatic {
                reader,
                incarnation,
            } => !self.patterns[reader as usize].owns(incarnation),
        }
    }

    pub(in crate::pool) fn observe(&mut self, clock: &Clock, reader: u32, page: PageId) {
        self.reconcile_feedback(clock);
        if let Some(pattern) = self.patterns.get_mut(reader as usize) {
            pattern.observe(page, self.capacity);
        }
    }

    pub(in crate::pool) fn reset_reader(&mut self, reader: u32) {
        if let Some(pattern) = self.patterns.get_mut(reader as usize) {
            pattern.reset();
        }
    }
}
