use super::pattern::Pattern;
use super::{PageId, PrefetchStats, Readahead, Source};
use crate::pool::clock::SpeculationOutcome;
use crate::pool::{Clock, FreeFrames, ReadFrameIdx};

#[derive(Debug, Clone, Copy)]
pub(super) struct Entry {
    pub(super) page: PageId,
    pub(super) frame: ReadFrameIdx,
    source: Source,
    membership: Option<Links>,
    protected: u64,
}

#[derive(Debug, Clone, Copy)]
struct Feedback {
    reader: u32,
    incarnation: u64,
    page: PageId,
}

impl Feedback {
    fn byte(self, ordinal: u32) -> usize {
        let byte = match ordinal {
            0..=3 => (u64::from(self.page.granule_idx()) >> (ordinal * 8)) & 255,
            4..=11 => (self.incarnation >> ((ordinal - 4) * 8)) & 255,
            12..=15 => (u64::from(self.reader) >> ((ordinal - 12) * 8)) & 255,
            _ => unreachable!("fixed-width feedback key"),
        };
        usize::try_from(byte).expect("one byte fits usize")
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

#[derive(Debug, Default, Clone, Copy)]
struct Links {
    previous: Option<u32>,
    next: Option<u32>,
}

#[derive(Debug)]
pub(super) struct IndexQueue {
    links: Box<[Option<Links>]>,
    first: Option<u32>,
    last: Option<u32>,
}

impl IndexQueue {
    fn try_new(capacity: u32) -> Option<Self> {
        Some(Self {
            links: crate::allocation::try_boxed_slice_with(capacity, || None)?,
            first: None,
            last: None,
        })
    }

    pub(super) fn front(&self) -> Option<u32> {
        self.first
    }

    fn push(&mut self, index: u32) {
        if self.links[index as usize].is_some() {
            return;
        }
        self.links[index as usize] = Some(Links {
            previous: self.last,
            next: None,
        });
        if let Some(last) = self.last {
            self.links[last as usize].as_mut().expect("queue tail").next = Some(index);
        } else {
            self.first = Some(index);
        }
        self.last = Some(index);
    }

    fn remove(&mut self, index: u32) {
        let Some(links) = self.links[index as usize].take() else {
            return;
        };
        if let Some(previous) = links.previous {
            self.links[previous as usize]
                .as_mut()
                .expect("previous link")
                .next = links.next;
        } else {
            self.first = links.next;
        }
        if let Some(next) = links.next {
            self.links[next as usize]
                .as_mut()
                .expect("next link")
                .previous = links.previous;
        } else {
            self.last = links.previous;
        }
    }

    pub(super) fn pop(&mut self) -> Option<u32> {
        let index = self.first?;
        self.remove(index);
        Some(index)
    }

    #[cfg(feature = "bench")]
    pub(super) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        let mut next = self.first;
        (0..self.links.len()).map_while(move |_| {
            let index = next?;
            next = self.links[index as usize].expect("queue member").next;
            Some(index)
        })
    }
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
    pub(super) vector_width: u32,
    pub(super) automatic_width: u32,
    pub(super) ready: IndexQueue,
    pub(super) cleanup: IndexQueue,
    waiting: IndexQueue,
    reader_entries: Box<[Option<u32>]>,
    frame_entries: Box<[Option<u32>]>,
    vacant: Box<[u32]>,
    vacant_count: u32,
    feedback: Box<[Option<Feedback>]>,
    feedback_scratch: Box<[Option<Feedback>]>,
    feedback_count: u32,
    stamp: u64,
    #[cfg(feature = "bench")]
    explicit_evictions: Option<u64>,
    pub(super) replacement_cursor: u32,
    pub(super) pending_releases: u32,
    #[cfg(feature = "bench")]
    pub(in crate::pool) observation:
        Option<std::sync::Arc<crate::driver::observation::ReadObservation>>,
}

impl Prefetch {
    #[cfg(all(test, loom))]
    pub(in crate::pool) fn try_new(
        capacity: u32,
        readers: u32,
        reads: u32,
        mode: Readahead,
    ) -> Option<Self> {
        Self::try_with_geometry(capacity, readers, reads, mode, capacity.max(1), 4096)
    }

    pub(in crate::pool) fn try_with_geometry(
        capacity: u32,
        readers: u32,
        reads: u32,
        mode: Readahead,
        frames: u32,
        granule: u32,
    ) -> Option<Self> {
        let pattern_count = match mode {
            Readahead::Automatic if capacity > 0 => readers,
            Readahead::Automatic | Readahead::Disabled => 0,
        };
        let feedback_capacity = if pattern_count == 0 { 0 } else { capacity };
        let vector_width = (crate::driver::read_vector::VECTOR_BYTES_MAX / granule).clamp(1, 32);
        let automatic_width = if capacity.min(reads.saturating_sub(1)) >= 2 * vector_width {
            vector_width
        } else {
            1
        };
        let mut next = 0;
        let vacant = crate::allocation::try_boxed_slice_with(capacity, || {
            let index = next;
            next += 1;
            index
        })?;
        Some(Self {
            reserve: FreeFrames::try_empty(capacity)?,
            entries: crate::allocation::try_boxed_slice_with(capacity, || None)?,
            patterns: crate::allocation::try_boxed_slice_with(pattern_count, Pattern::new)?,
            stats: PrefetchStats::default(),
            capacity,
            request_limit: capacity.max(reads).saturating_mul(4).max(1) as usize,
            active: false,
            vector_width,
            automatic_width,
            ready: IndexQueue::try_new(pattern_count)?,
            cleanup: IndexQueue::try_new(capacity)?,
            waiting: IndexQueue::try_new(capacity)?,
            reader_entries: crate::allocation::try_boxed_slice_with(pattern_count, || None)?,
            frame_entries: crate::allocation::try_boxed_slice_with(frames, || None)?,
            vacant,
            vacant_count: capacity,
            feedback: crate::allocation::try_boxed_slice_with(feedback_capacity, || None)?,
            feedback_scratch: crate::allocation::try_boxed_slice_with(feedback_capacity, || None)?,
            feedback_count: 0,
            stamp: 0,
            #[cfg(feature = "bench")]
            explicit_evictions: None,
            replacement_cursor: 0,
            pending_releases: 0,
            #[cfg(feature = "bench")]
            observation: None,
        })
    }

    pub(super) fn available(&self) -> u32 {
        self.capacity
            .checked_sub(self.stats.occupied)
            .expect("bounded speculation")
    }

    pub(super) fn fill_reserve(&mut self, free: &mut FreeFrames) {
        if !self.active {
            return;
        }
        for _ in self.reserve.len()..self.available() {
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
        self.vacant_count -= 1;
        let index = self.vacant[self.vacant_count as usize];
        assert!(self.entries[index as usize].is_none());
        assert!(
            self.frame_entries[frame.get() as usize]
                .replace(index)
                .is_none()
        );
        let membership = if let Source::Automatic { reader, .. } = source {
            let next = self.reader_entries[reader as usize].replace(index);
            if let Some(next) = next {
                self.entries[next as usize]
                    .as_mut()
                    .expect("reader entry")
                    .membership
                    .as_mut()
                    .expect("linked entry")
                    .previous = Some(index);
            }
            Some(Links {
                previous: None,
                next,
            })
        } else {
            None
        };
        self.entries[index as usize] = Some(Entry {
            page,
            frame,
            source,
            membership,
            protected: self.stamp,
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
        clock.completed.drain(|frame| {
            if let Some(index) = self.frame_entries[frame.get() as usize] {
                if self.obsolete(self.entries[index as usize].expect("indexed entry")) {
                    self.cleanup.push(index);
                }
                self.record_control("completion", 1, 1, 0, 0);
            }
        });
        // Acquiring a later notification can reveal an earlier notification in
        // a bitmap branch already drained. Each nonempty round returns credits.
        for _ in 0..=self.capacity {
            let before = self.stats.occupied;
            clock.consumed.drain(|frame| {
                if let Some(index) = self.frame_entries[frame.get() as usize] {
                    let recovered = u32::from(clock.speculation_consumed(frame));
                    if recovered > 0 {
                        self.finish_index(clock, index, Terminal::Promoted(None));
                    }
                    self.record_control("consumption", 1, 1, 0, recovered);
                }
            });
            if before == self.stats.occupied {
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

    pub(in crate::pool) fn finish(
        &mut self,
        clock: &Clock,
        frame: ReadFrameIdx,
        terminal: Terminal,
    ) {
        if let Some(index) = self.frame_entries[frame.get() as usize] {
            let before = self.stats.demand_promoted;
            self.finish_index(clock, index, terminal);
            let recovered = u32::from(self.stats.demand_promoted > before);
            let cause = match terminal {
                Terminal::Promoted(_) => "consumption",
                Terminal::Failed => "completion",
                Terminal::Evicted => "invalidation",
            };
            self.record_control(cause, 1, 1, 0, recovered);
        } else {
            assert!(!clock.is_speculative(frame));
        }
    }

    fn finish_index_unlink_reader(&mut self, index: u32, entry: Entry) {
        let Some(links) = entry.membership else {
            return;
        };
        let Source::Automatic { reader, .. } = entry.source else {
            unreachable!("automatic member");
        };
        if let Some(previous) = links.previous {
            self.entries[previous as usize]
                .as_mut()
                .expect("previous member")
                .membership
                .as_mut()
                .expect("linked member")
                .next = links.next;
        } else {
            assert_eq!(self.reader_entries[reader as usize], Some(index));
            self.reader_entries[reader as usize] = links.next;
        }
        if let Some(next) = links.next {
            self.entries[next as usize]
                .as_mut()
                .expect("next member")
                .membership
                .as_mut()
                .expect("linked member")
                .previous = links.previous;
        }
    }

    fn finish_index(&mut self, clock: &Clock, index: u32, terminal: Terminal) {
        let entry = self.entries[index as usize]
            .take()
            .expect("one speculative credit");
        #[cfg(feature = "bench")]
        match (&mut self.explicit_evictions, terminal) {
            (Some(evictions), Terminal::Evicted) if entry.protected == self.stamp => {
                *evictions += 1;
            }
            _ => {}
        }
        self.finish_index_unlink_reader(index, entry);
        self.cleanup.remove(index);
        self.waiting.remove(index);
        assert_eq!(
            self.frame_entries[entry.frame.get() as usize].take(),
            Some(index)
        );
        self.vacant[self.vacant_count as usize] = index;
        self.vacant_count += 1;
        let outcome = clock.take_speculation(entry.frame);
        assert_ne!(outcome, SpeculationOutcome::Absent);
        self.stats.occupied = self
            .stats
            .occupied
            .checked_sub(1)
            .expect("one credit returned once");
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
        self.finish_index_feedback(entry, terminal);
        assert_eq!(
            self.stats.admitted,
            self.stats.demand_promoted
                + self.stats.evicted_unused
                + self.stats.failed
                + u64::from(self.stats.occupied)
        );
    }

    fn finish_index_feedback(&mut self, entry: Entry, terminal: Terminal) {
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
                    if self.patterns[reader as usize].owns(incarnation) {
                        self.reset_reader(reader);
                    }
                }
            }
        }
    }

    fn flush_feedback_sort(&mut self) {
        let count = self.feedback_count as usize;
        for ordinal in 0..16 {
            let mut counts = [0_usize; 256];
            for feedback in &self.feedback[..count] {
                counts[feedback.expect("feedback prefix").byte(ordinal)] += 1;
            }
            let mut offset = 0;
            for count in &mut counts {
                let length = *count;
                *count = offset;
                offset += length;
            }
            for feedback in &self.feedback[..count] {
                let byte = feedback.expect("feedback prefix").byte(ordinal);
                self.feedback_scratch[counts[byte]] = *feedback;
                counts[byte] += 1;
            }
            std::mem::swap(&mut self.feedback, &mut self.feedback_scratch);
        }
    }

    fn flush_feedback(&mut self) {
        if self.feedback_count == 0 {
            return;
        }
        if self.feedback_count > 1 {
            self.flush_feedback_sort();
        }
        for index in 0..self.feedback_count as usize {
            let feedback = self.feedback[index].take().expect("feedback prefix");
            let reader = feedback.reader;
            let before = self.patterns[reader as usize].incarnation();
            self.patterns[reader as usize].promoted(
                feedback.page,
                feedback.incarnation,
                self.capacity,
            );
            self.pattern_changed(reader, before);
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
            let before = pattern.incarnation();
            pattern.observe(page, self.capacity);
            self.pattern_changed(reader, before);
        }
    }

    pub(in crate::pool) fn reset_reader(&mut self, reader: u32) {
        if let Some(pattern) = self.patterns.get_mut(reader as usize) {
            let before = pattern.incarnation();
            pattern.reset();
            self.pattern_changed(reader, before);
        }
    }

    fn pattern_changed(&mut self, reader: u32, before: u64) {
        if !self.patterns[reader as usize].owns(before) {
            self.invalidate_reader(reader);
        }
        self.refresh_ready(reader);
        #[cfg(feature = "bench")]
        if let Some(observation) = self
            .observation
            .as_ref()
            .filter(|_| self.patterns[reader as usize].width() >= 32)
        {
            observation.confirmed_window();
        }
    }

    fn invalidate_reader(&mut self, reader: u32) {
        let mut next = self.reader_entries[reader as usize].take();
        let mut visited = 0;
        for _ in 0..self.capacity {
            let Some(index) = next else {
                break;
            };
            next = self.entries[index as usize]
                .as_mut()
                .expect("reader member")
                .membership
                .take()
                .expect("linked entry")
                .next;
            self.cleanup.push(index);
            visited += 1;
        }
        assert!(next.is_none(), "bounded incarnation membership");
        if visited > 0 {
            self.record_control("invalidation", visited, 0, visited, 0);
        }
    }

    pub(super) fn refresh_ready(&mut self, reader: u32) {
        if self.patterns[reader as usize]
            .window(reader, self.automatic_width)
            .is_some()
        {
            self.ready.push(reader);
        } else {
            self.ready.remove(reader);
        }
    }

    pub(super) fn issued(&mut self, reader: u32, page: PageId) {
        let before = self.patterns[reader as usize].incarnation();
        self.patterns[reader as usize].issued(page);
        if !self.patterns[reader as usize].owns(before) {
            self.invalidate_reader(reader);
        }
        self.refresh_ready(reader);
    }

    pub(super) fn advance_turn(&mut self, reader: u32) {
        self.ready.remove(reader);
        self.refresh_ready(reader);
    }

    pub(super) fn wait_cleanup(&mut self, index: u32) {
        self.waiting.push(index);
    }

    pub(super) fn retry_cleanup(&mut self, releases: u32) {
        if self.pending_releases == releases {
            return;
        }
        self.pending_releases = releases;
        for _ in 0..self.capacity {
            let Some(index) = self.waiting.pop() else {
                break;
            };
            self.cleanup.push(index);
        }
    }

    pub(super) fn begin_explicit(&mut self) {
        #[cfg(feature = "bench")]
        assert!(self.explicit_evictions.replace(0).is_none());
        self.stamp = self
            .stamp
            .checked_add(1)
            .expect("prefetch protection stamp exhausted");
        self.replacement_cursor = 0;
    }

    #[cfg(feature = "bench")]
    pub(super) fn end_explicit(&mut self) -> u64 {
        self.explicit_evictions
            .take()
            .expect("one active explicit call")
    }

    pub(super) fn protect(&mut self, frame: ReadFrameIdx) {
        if let Some(index) = self.frame_entries[frame.get() as usize] {
            self.entries[index as usize]
                .as_mut()
                .expect("indexed entry")
                .protected = self.stamp;
        }
    }

    pub(super) fn protected(&self, entry: Entry) -> bool {
        entry.protected == self.stamp
    }

    pub(super) fn record_control(
        &self,
        cause: &'static str,
        visits: u32,
        affected: u32,
        cleanup: u32,
        recovered: u32,
    ) {
        #[cfg(feature = "bench")]
        if let Some(observation) = &self.observation {
            observation.control(self.capacity, cause, visits, affected, cleanup, recovered);
        }
        #[cfg(not(feature = "bench"))]
        let _ = (self, cause, visits, affected, cleanup, recovered);
    }

    #[cfg(feature = "bench")]
    pub(in crate::pool) fn metadata_bytes(&self) -> u64 {
        let bytes = size_of_val(&*self.entries)
            + size_of_val(&*self.patterns)
            + size_of_val(&*self.ready.links)
            + size_of_val(&*self.cleanup.links)
            + size_of_val(&*self.waiting.links)
            + size_of_val(&*self.reader_entries)
            + size_of_val(&*self.frame_entries)
            + size_of_val(&*self.vacant)
            + size_of_val(&*self.feedback)
            + size_of_val(&*self.feedback_scratch);
        u64::try_from(bytes).expect("allocated metadata fits u64")
    }
}
