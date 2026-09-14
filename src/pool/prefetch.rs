//! Bounded speculative ownership beside the ordinary miss and EBR protocols.

pub(super) mod notifications;
mod pattern;
pub(super) mod state;
#[cfg(all(test, feature = "mock", not(loom)))]
mod tests;

use super::{
    Control, FrameState, PageId, Pool, PoolBackend, PoolBuilder, PoolConfigError, ReadFrameIdx,
    file_is_live,
};
use crate::sync::Ordering;
pub(super) use state::Prefetch;
use state::Terminal;

/// Automatic forward-sequential prediction. Explicit hints work in either mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Readahead {
    /// Learn consecutive cold demand and extend confirmed streams during polling.
    #[default]
    Automatic,
    /// Issue only demand reads and caller-provided prefetch requests.
    Disabled,
}

/// Admission feedback, partitioning the input window without promising completion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PrefetchReport {
    pub requested: u64,
    pub resident: u64,
    pub pending: u64,
    pub admitted: u64,
    pub deferred: u64,
    pub rejected: u64,
}

impl PrefetchReport {
    fn record(&mut self, admission: Admission, count: u64) {
        match admission {
            Admission::Resident => self.resident += count,
            Admission::Pending => self.pending += count,
            Admission::Admitted => self.admitted += count,
            Admission::Deferred => self.deferred += count,
            Admission::Rejected => self.rejected += count,
            Admission::Obsolete => unreachable!("explicit hints have no incarnation"),
        }
    }
}

/// Cumulative outcomes plus the current bounded speculative occupancy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PrefetchStats {
    pub admitted: u64,
    pub automatic_admitted: u64,
    pub demand_promoted: u64,
    pub evicted_unused: u64,
    pub failed: u64,
    pub deferred: u64,
    pub submission_refused: u64,
    pub occupied: u32,
    pub capacity: u32,
    pub reserve_free: u32,
    pub reads_in_flight: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Source {
    Explicit,
    Automatic { reader: u32, incarnation: u64 },
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Admission {
    Resident,
    Pending,
    Admitted,
    Deferred,
    Rejected,
    Obsolete,
}

impl PoolBuilder {
    /// Sets speculative credits, covering both in-flight and unconsumed pages.
    /// Zero disables all speculative admission. The default is 524288 bytes,
    /// rounded down to pages and bounded by spare frames and read capacity minus one.
    #[must_use]
    pub fn prefetch_headroom(mut self, credits: u32) -> Self {
        self.prefetch_headroom = Some(credits);
        self
    }

    /// Selects automatic sequential detection; the default is `Automatic`.
    #[must_use]
    pub fn readahead(mut self, mode: Readahead) -> Self {
        self.readahead = mode;
        self
    }

    pub(super) fn prefetch_capacity(&self) -> Result<u32, PoolConfigError> {
        let committed = u64::from(self.max_concurrent_readers)
            * u64::from(self.peak_guards_per_reader)
            + u64::from(self.miss_headroom)
            + u64::from(self.max_retained_frames);
        let spare = u32::try_from(u64::from(self.frame_count).saturating_sub(committed))
            .expect("spare frames fit u32");
        let credits = self.prefetch_headroom.unwrap_or(
            spare
                .min(self.max_inflight_reads.saturating_sub(1))
                .min(524_288 / self.granule),
        );
        if credits > spare {
            return Err(PoolConfigError::PrefetchHeadroomTooLarge {
                requested: credits,
                available: spare,
            });
        }
        Ok(credits)
    }
}

#[expect(
    private_bounds,
    reason = "the sealed backend preserves static dispatch"
)]
impl<D: PoolBackend> Pool<D> {
    /// Requests residency for a bounded prefix of `pages`, without creating guards.
    ///
    /// Call `poll` to drive accepted reads and retry deferred requests as useful.
    /// At most four times the larger of read capacity and speculative capacity
    /// is examined. The remaining suffix is deferred. Repeat the useful window
    /// when extending it: unconsumed pages outside a new window may be evicted
    /// to release speculative credits. Already accepted reads are never cancelled.
    /// Failed speculative reads release their credits; joined demand sees the
    /// error, while later demand can retry an abandoned failed read.
    /// Extending a sliding window by one page can issue a point read. To coalesce
    /// extensions, add vector-sized chunks while repeating the useful prefix.
    ///
    /// # Panics
    ///
    /// If an examined page belongs to another pool.
    #[must_use]
    pub fn prefetch(&self, pages: &[PageId]) -> PrefetchReport {
        let mut control = self.control();
        control.prefetch.reconcile(&self.clock);
        control.prefetch.active = control.prefetch.capacity > 0;
        let examined = &pages[..pages.len().min(control.prefetch.request_limit)];
        let mut report = PrefetchReport {
            requested: u64::try_from(pages.len()).expect("slice length fits u64"),
            deferred: u64::try_from(pages.len() - examined.len()).expect("suffix length fits u64"),
            ..PrefetchReport::default()
        };
        control.prefetch.begin_explicit();
        #[cfg(feature = "bench")]
        let clock_before = self.clock.visits();
        let protection_lookups = self.prefetch_protect(&mut control, examined);
        let admission_visits = self.prefetch_examined(&mut control, examined, &mut report);
        #[cfg(feature = "bench")]
        let protected_evictions = control.prefetch.end_explicit();
        #[cfg(feature = "bench")]
        if let Some(observation) = self.driver.read_observation() {
            observation.explicit(
                control.prefetch.capacity,
                [
                    examined.len() as u64,
                    protection_lookups,
                    u64::from(control.prefetch.replacement_cursor),
                    admission_visits,
                    self.clock.visits() - clock_before,
                    protected_evictions,
                ],
            );
        }
        #[cfg(not(feature = "bench"))]
        let _ = (protection_lookups, admission_visits);
        control.prefetch.stats.deferred += report.deferred;
        if report.admitted > 0 || report.deferred > 0 {
            self.wake.wake();
        }
        assert_eq!(
            report.requested,
            report.resident + report.pending + report.admitted + report.deferred + report.rejected
        );
        report
    }

    /// Returns reconciled counters. Promotion records demand interest, including
    /// joining an in-flight read; it does not promise a successful completion.
    #[must_use]
    pub fn prefetch_stats(&self) -> PrefetchStats {
        let mut control = self.control();
        control.prefetch.reconcile(&self.clock);
        PrefetchStats {
            capacity: control.prefetch.capacity,
            reserve_free: control.prefetch.reserve.len(),
            reads_in_flight: control.reads_in_flight,
            ..control.prefetch.stats
        }
    }

    fn prefetch_examined(
        &self,
        control: &mut Control,
        examined: &[PageId],
        report: &mut PrefetchReport,
    ) -> u64 {
        let mut admission_visits = 0;
        let mut cursor = 0;
        for _ in 0..examined.len() {
            if cursor == examined.len() {
                break;
            }
            let page = examined[cursor];
            admission_visits += 1;
            if let Some(outcome) = self.prefetch_classify(control, page) {
                report.record(outcome, 1);
                cursor += 1;
                continue;
            }
            let end =
                self.prefetch_examined_run_end(control, examined, cursor, &mut admission_visits);
            let wanted = u32::try_from(end - cursor).expect("bounded explicit run");
            self.prefetch_examined_replace_unused(control, wanted);
            let count = wanted
                .min(control.prefetch.available())
                .min(self.max_inflight_reads - control.reads_in_flight);
            if count == 0 {
                report.deferred += u64::from(wanted);
                cursor = end;
                continue;
            }
            let run = &examined[cursor..cursor + count as usize];
            control.prefetch.reconcile_feedback(&self.clock);
            let outcome = self.prefetch_admit_run(control, run, Source::Explicit);
            report.record(outcome, u64::from(count));
            cursor += count as usize;
        }
        assert_eq!(cursor, examined.len());
        admission_visits
    }

    fn prefetch_protect(&self, control: &mut Control, pages: &[PageId]) -> u64 {
        let mut lookups = 0;
        for &page in pages {
            lookups += 1;
            assert_eq!(
                page.file().driver(),
                self.identity,
                "prefetch uses its owning pool"
            );
            let frame = self.table.lookup(page).or_else(|| {
                control
                    .miss
                    .find_pending(page)
                    .map(|index| control.miss.entry(index).frame())
            });
            if let Some(frame) = frame {
                control.prefetch.protect(frame);
            }
        }
        lookups
    }

    fn prefetch_classify(&self, control: &Control, page: PageId) -> Option<Admission> {
        assert_eq!(
            page.file().driver(),
            self.identity,
            "prefetch uses its owning pool"
        );
        if !file_is_live(&control.files, page.file(), self.identity) {
            return Some(Admission::Rejected);
        }
        if self.table.lookup(page).is_some() {
            return Some(Admission::Resident);
        }
        if control.miss.find_pending(page).is_some() {
            return Some(Admission::Pending);
        }
        None
    }

    fn prefetch_examined_run_end(
        &self,
        control: &Control,
        pages: &[PageId],
        start: usize,
        visits: &mut u64,
    ) -> usize {
        let mut end = start + 1;
        let limit = pages
            .len()
            .min(start + control.prefetch.vector_width as usize);
        for index in start + 1..limit {
            if pages[index].file() != pages[index - 1].file() {
                break;
            }
            if pages[index - 1].granule_idx().checked_add(1) != Some(pages[index].granule_idx()) {
                break;
            }
            *visits += 1;
            if self.prefetch_classify(control, pages[index]).is_some() {
                break;
            }
            end += 1;
        }
        end
    }

    #[cfg(all(test, feature = "mock", not(loom)))]
    fn prefetch_admit(&self, control: &mut Control, page: PageId, source: Source) -> Admission {
        control.prefetch.reconcile_feedback(&self.clock);
        if control.prefetch.source_obsolete(source) {
            return Admission::Obsolete;
        }
        if let Some(outcome) = self.prefetch_classify(control, page) {
            return outcome;
        }
        self.prefetch_admit_run(control, &[page], source)
    }

    pub(super) fn prefetch_admit_run(
        &self,
        control: &mut Control,
        pages: &[PageId],
        source: Source,
    ) -> Admission {
        assert!(!pages.is_empty());
        if pages.len() > 1 {
            let outcome = self.prefetch_admit_vector(control, pages, source);
            if matches!(outcome, Admission::Admitted) {
                self.prefetch_admit_run_record(pages, source);
            }
            return outcome;
        }
        let page = pages[0];
        if control.prefetch.available() == 0 || control.reads_in_flight >= self.max_inflight_reads {
            return Admission::Deferred;
        }
        let Some(slot) = control.miss.admission_slot(&self.miss_interests) else {
            return Admission::Deferred;
        };
        control.prefetch.fill_reserve(&mut control.free_frames);
        let Some(frame) = control.prefetch.reserve.pop() else {
            return Admission::Deferred;
        };
        let write = self
            .frames
            .claim(frame, page)
            .expect("reserve entries are exclusively Free");
        let token = match self.submit_page_read(
            control,
            page,
            write,
            crate::driver::ReadPurpose::Speculative,
        ) {
            Ok(token) => token,
            Err(refusal) => {
                control
                    .prefetch
                    .reserve
                    .push(self.frames.abort(refusal.token));
                control.prefetch.stats.submission_refused += 1;
                return Admission::Deferred;
            }
        };
        control.reads_in_flight += 1;
        control
            .miss
            .admit_speculative(slot, page, frame, token, &self.miss_interests);
        control.prefetch.admit(&self.clock, page, frame, source);
        self.prefetch_admit_run_record(pages, source);
        Admission::Admitted
    }

    fn prefetch_admit_vector(
        &self,
        control: &mut Control,
        pages: &[PageId],
        source: Source,
    ) -> Admission {
        use super::read_spans::ReadSpan;
        use crate::driver::read_vector::{VECTOR_BYTES_MAX, VECTOR_FRAMES_MAX};
        let count = u32::try_from(pages.len()).expect("bounded classified run");
        assert!((2..=VECTOR_FRAMES_MAX).contains(&count));
        assert!(count <= VECTOR_BYTES_MAX / self.granule);
        assert!(pages.windows(2).all(|pair| pair[0].file() == pair[1].file()
            && pair[0].granule_idx().checked_add(1) == Some(pair[1].granule_idx())));
        if control.prefetch.available() < count
            || self.max_inflight_reads - control.reads_in_flight < count
        {
            return Admission::Deferred;
        }
        let Some(route) = control.read_spans.reserve() else {
            return Admission::Deferred;
        };
        let mut slots = [None; VECTOR_FRAMES_MAX as usize];
        if !control
            .miss
            .admission_slots(&self.miss_interests, &mut slots[..pages.len()])
        {
            control.read_spans.cancel(route);
            return Admission::Deferred;
        }
        control.prefetch.fill_reserve(&mut control.free_frames);
        if control.prefetch.reserve.len() < count {
            control.read_spans.cancel(route);
            return Admission::Deferred;
        }
        let (vector, frames) = self.prefetch_admit_vector_claim(control, pages);
        let offset = u64::from(pages[0].granule_idx()) * u64::from(self.granule);
        let fd = super::registered_file(&control.files, pages[0]);
        let token = match self.driver.submit_read_vector(fd, vector, offset) {
            Ok(token) => token,
            Err((_error, mut vector)) => {
                vector.take_remaining(|_, frame| {
                    control.prefetch.reserve.push(self.frames.abort(frame));
                });
                control.read_spans.cancel(route);
                control.prefetch.stats.submission_refused += 1;
                return Admission::Deferred;
            }
        };
        let mut span = ReadSpan::new(pages[0], token, count, self.granule);
        for (ordinal, &page) in pages.iter().enumerate() {
            let frame = frames[ordinal].expect("claimed destination");
            let slot = slots[ordinal].expect("reserved miss slot");
            let generation =
                control
                    .miss
                    .admit_speculative(slot, page, frame, token, &self.miss_interests);
            span.install(ordinal, slot, generation);
            control.prefetch.admit(&self.clock, page, frame, source);
        }
        control.reads_in_flight += count;
        control.read_spans.commit(route, span);
        Admission::Admitted
    }

    fn prefetch_admit_vector_claim(
        &self,
        control: &mut Control,
        pages: &[PageId],
    ) -> (
        crate::driver::read_vector::ReadVector,
        [Option<ReadFrameIdx>; 32],
    ) {
        let mut vector = crate::driver::read_vector::ReadVector::new(&self.frames);
        let mut frames = [None; 32];
        for (ordinal, &page) in pages.iter().enumerate() {
            assert!(self.table.lookup(page).is_none());
            assert!(control.miss.find_pending(page).is_none());
            let frame = control.prefetch.reserve.pop().expect("reserved inventory");
            vector.push(
                self.frames
                    .claim(frame, page)
                    .expect("reserved frames are exclusively free"),
            );
            frames[ordinal] = Some(frame);
        }
        (vector, frames)
    }

    #[cfg(any(feature = "mock", feature = "bench"))]
    pub(crate) fn prefetch_span_internal(&self, pages: &[PageId]) -> PrefetchReport {
        let count = u64::try_from(pages.len()).expect("slice length fits u64");
        let mut report = PrefetchReport {
            requested: count,
            ..PrefetchReport::default()
        };
        let Some(&first) = pages.first() else {
            return report;
        };
        let mut control = self.control();
        assert_eq!(first.file().driver(), self.identity);
        control.prefetch.reconcile(&self.clock);
        control.prefetch.active = control.prefetch.capacity > 0;
        if !file_is_live(&control.files, first.file(), self.identity) {
            report.rejected = count;
            return report;
        }
        match self.prefetch_admit_run(&mut control, pages, Source::Explicit) {
            Admission::Admitted => report.admitted = count,
            Admission::Deferred => {
                report.deferred = count;
                control.prefetch.stats.deferred += count;
            }
            _ => unreachable!("the seam receives an already classified absent run"),
        }
        self.wake.wake();
        report
    }

    fn prefetch_examined_replace_unused(&self, control: &mut Control, wanted: u32) {
        for _ in control.prefetch.replacement_cursor..control.prefetch.capacity {
            if control.prefetch.available() >= wanted {
                break;
            }
            let index = control.prefetch.replacement_cursor as usize;
            control.prefetch.replacement_cursor += 1;
            let Some(entry) = control.prefetch.entries[index] else {
                continue;
            };
            if control.prefetch.protected(entry) {
                continue;
            }
            if self.frames.state(entry.frame) != FrameState::Resident {
                continue;
            }
            if !control
                .miss
                .prepare_eviction(entry.frame, &self.miss_interests)
            {
                continue;
            }
            self.evict_resident(control, entry.frame);
        }
    }

    pub(super) fn progress_prefetch(&self, control: &mut Control) {
        #[cfg(feature = "bench")]
        let event_before = self
            .driver
            .read_observation()
            .map(|observation| observation.control_count());
        control.prefetch.reconcile(&self.clock);
        control
            .prefetch
            .retry_cleanup(self.lifecycle.pending_releases.load(Ordering::Acquire));
        self.prefetch_evict_obsolete(control);
        self.prefetch_top_up(control);
        let mut admitted = 0;
        for _ in 0..control.prefetch.capacity {
            let Some(reader) = control.prefetch.ready.front() else {
                break;
            };
            let Some((page, source, count)) = control.prefetch.patterns[reader as usize]
                .window(reader, control.prefetch.automatic_width)
            else {
                unreachable!("ready stream has eligible horizon");
            };
            control.prefetch.active = true;
            let (outcome, committed) = self.prefetch_automatic_window(control, page, source, count);
            admitted += committed;
            if matches!(outcome, Admission::Deferred) {
                break;
            }
        }
        self.prefetch_top_up(control);
        if admitted > 0 {
            self.wake.wake();
        }
        #[cfg(feature = "bench")]
        match self.driver.read_observation() {
            Some(observation) if event_before == Some(observation.control_count()) => {
                control.prefetch.record_control("idle", 0, 0, 0, 0);
            }
            Some(_) | None => {}
        }
    }

    fn prefetch_automatic_window(
        &self,
        control: &mut Control,
        first: PageId,
        source: Source,
        count: u32,
    ) -> (Admission, u32) {
        let Source::Automatic { reader, .. } = source else {
            unreachable!("automatic source");
        };
        assert_eq!(control.prefetch.ready.front(), Some(reader));
        let (pages, classes) = self.prefetch_automatic_window_classify(control, first, count);
        if !self.prefetch_automatic_window_has_credits(control, &classes[..count as usize]) {
            let _ =
                self.prefetch_automatic_window_record_turn(control, reader, 0, "credit_deferred");
            return (Admission::Deferred, 0);
        }
        let recorded = self.prefetch_automatic_window_record_turn(control, reader, 0, "pending");
        let mut cursor = 0;
        let mut admitted = 0;
        let mut result = Admission::Resident;
        for _ in 0..count {
            if cursor == count as usize {
                break;
            }
            let mut end = cursor + 1;
            let outcome = if let Some(outcome) = classes[cursor] {
                outcome
            } else {
                for class in &classes[end..count as usize] {
                    if class.is_some() {
                        break;
                    }
                    end += 1;
                }
                control.prefetch.reconcile_feedback(&self.clock);
                if control.prefetch.source_obsolete(source) {
                    result = Admission::Obsolete;
                    break;
                }
                self.prefetch_admit_run(control, &pages[cursor..end], source)
            };
            if matches!(outcome, Admission::Deferred) {
                result = outcome;
                break;
            }
            if matches!(outcome, Admission::Rejected) {
                control.prefetch.reset_reader(reader);
                result = outcome;
                break;
            }
            if matches!(outcome, Admission::Admitted) {
                admitted += u32::try_from(end - cursor).expect("bounded run");
            }
            for &page in &pages[cursor..end] {
                control.prefetch.issued(reader, page);
            }
            cursor = end;
        }
        self.prefetch_automatic_window_finish(control, reader, recorded, result, admitted)
    }

    fn prefetch_automatic_window_finish(
        &self,
        control: &mut Control,
        reader: u32,
        recorded: Option<usize>,
        outcome: Admission,
        admitted: u32,
    ) -> (Admission, u32) {
        match (outcome, admitted) {
            (Admission::Deferred | Admission::Rejected | Admission::Obsolete, 0) => {}
            _ => control.prefetch.advance_turn(reader),
        }
        #[cfg(not(feature = "bench"))]
        let _ = (self, recorded);
        #[cfg(feature = "bench")]
        if let Some(observation) = self.driver.read_observation() {
            let recorded_outcome = if admitted > 0 {
                "admitted"
            } else {
                match outcome {
                    Admission::Resident => "resident",
                    Admission::Deferred => "resource_deferred",
                    Admission::Rejected => "rejected",
                    Admission::Obsolete => "obsolete",
                    Admission::Admitted | Admission::Pending => unreachable!("window outcome"),
                }
            };
            observation.reader_turn_after(
                recorded,
                control.prefetch.ready.front(),
                admitted,
                recorded_outcome,
            );
        }
        (outcome, admitted)
    }

    fn prefetch_automatic_window_has_credits(
        &self,
        control: &Control,
        classes: &[Option<Admission>],
    ) -> bool {
        let missing = u32::try_from(classes.iter().filter(|class| class.is_none()).count())
            .expect("bounded window");
        if control.prefetch.available() < missing {
            return false;
        }
        self.max_inflight_reads - control.reads_in_flight >= missing
    }

    fn prefetch_automatic_window_classify(
        &self,
        control: &Control,
        first: PageId,
        count: u32,
    ) -> ([PageId; 32], [Option<Admission>; 32]) {
        assert!((1..=32).contains(&count));
        let mut pages = [first; 32];
        let mut classes = [None; 32];
        for index in 0..count as usize {
            pages[index] = PageId::new(
                first.file(),
                first
                    .granule_idx()
                    .checked_add(u32::try_from(index).expect("bounded vector index"))
                    .expect("confirmed horizon"),
            );
            classes[index] = self.prefetch_classify(control, pages[index]);
        }
        (pages, classes)
    }

    fn prefetch_admit_run_record(&self, pages: &[PageId], source: Source) {
        #[cfg(feature = "bench")]
        if let Some(observation) = self.driver.read_observation() {
            let stream = match source {
                Source::Explicit => None,
                Source::Automatic {
                    reader,
                    incarnation,
                } => Some((reader, incarnation)),
            };
            observation.committed_run(
                pages[0],
                u32::try_from(pages.len()).expect("bounded run"),
                stream,
            );
        }
        #[cfg(not(feature = "bench"))]
        let _ = (self, pages, source);
    }

    fn prefetch_automatic_window_record_turn(
        &self,
        control: &Control,
        reader: u32,
        admitted: u32,
        outcome: &'static str,
    ) -> Option<usize> {
        #[cfg(feature = "bench")]
        {
            self.driver.read_observation().and_then(|observation| {
                observation.reader_turn(
                    reader,
                    control.prefetch.ready.front(),
                    admitted,
                    outcome,
                    control.prefetch.ready.iter(),
                )
            })
        }
        #[cfg(not(feature = "bench"))]
        {
            let _ = (self, control, reader, admitted, outcome);
            None
        }
    }

    fn prefetch_top_up(&self, control: &mut Control) {
        if !control.prefetch.active {
            return;
        }
        control.prefetch.fill_reserve(&mut control.free_frames);
        let deficit = control
            .prefetch
            .available()
            .saturating_sub(control.prefetch.reserve.len())
            .saturating_sub(control.evict_queue.len());
        for _ in 0..deficit {
            if !self.evict_clock_victim(control) {
                break;
            }
        }
        if deficit > 0 {
            for _ in 0..2 {
                self.advance_and_reclaim(control);
            }
            control.prefetch.fill_reserve(&mut control.free_frames);
        }
    }

    pub(super) fn prefetch_evict_unused(&self, control: &mut Control) -> bool {
        control.prefetch.reconcile(&self.clock);
        for index in 0..control.prefetch.entries.len() {
            let Some(entry) = control.prefetch.entries[index] else {
                continue;
            };
            if self.frames.state(entry.frame) != FrameState::Resident {
                continue;
            }
            if !control
                .miss
                .prepare_eviction(entry.frame, &self.miss_interests)
            {
                continue;
            }
            self.evict_resident(control, entry.frame);
            return true;
        }
        false
    }

    fn prefetch_evict_obsolete(&self, control: &mut Control) {
        for _ in 0..control.prefetch.capacity {
            let Some(index) = control.prefetch.cleanup.pop() else {
                break;
            };
            let entry = control.prefetch.entries[index as usize].expect("queued entry");
            assert!(control.prefetch.obsolete(entry));
            control.prefetch.record_control("invalidation", 1, 0, 1, 0);
            if self.frames.state(entry.frame) != FrameState::Resident {
                continue;
            }
            if control
                .miss
                .prepare_eviction(entry.frame, &self.miss_interests)
            {
                self.evict_resident(control, entry.frame);
            } else {
                control.prefetch.wait_cleanup(index);
            }
        }
    }

    pub(super) fn prefetch_promote(&self, control: &mut Control, frame: ReadFrameIdx, reader: u32) {
        control
            .prefetch
            .finish(&self.clock, frame, Terminal::Promoted(Some(reader)));
    }

    pub(super) fn prefetch_fail(&self, control: &mut Control, frame: ReadFrameIdx) {
        control
            .prefetch
            .finish(&self.clock, frame, Terminal::Failed);
    }

    pub(super) fn evict_resident(&self, control: &mut Control, frame: ReadFrameIdx) {
        let page = self.frames.exact_page_locked(frame, control);
        assert_eq!(self.table.remove_shared(page), Some(frame));
        control
            .prefetch
            .finish(&self.clock, frame, Terminal::Evicted);
        self.frames.advance(frame, FrameState::Evicting);
        control
            .evict_queue
            .push(frame, self.global_epoch.load(Ordering::Acquire));
    }
}
