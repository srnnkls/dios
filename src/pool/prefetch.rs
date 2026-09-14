//! Bounded speculative ownership beside the ordinary miss and EBR protocols.

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
enum Source {
    Explicit,
    Automatic { reader: u32, incarnation: u64 },
}

#[derive(Debug, Clone, Copy)]
enum Admission {
    Resident,
    Pending,
    Admitted,
    Deferred,
    Rejected,
    Obsolete,
}

impl PoolBuilder {
    /// Sets speculative credits, covering both in-flight and unconsumed pages.
    /// Zero disables all speculative admission. The default is at most 32,
    /// bounded by spare frames and the configured read capacity minus one.
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
        let credits = self
            .prefetch_headroom
            .unwrap_or(spare.min(self.max_inflight_reads.saturating_sub(1)).min(32));
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
        for &page in examined {
            let mut outcome = self.prefetch_admit(&mut control, page, Source::Explicit);
            if matches!(outcome, Admission::Deferred) {
                self.prefetch_replace_unused(&mut control, examined);
                outcome = self.prefetch_admit(&mut control, page, Source::Explicit);
            }
            match outcome {
                Admission::Resident => report.resident += 1,
                Admission::Pending => report.pending += 1,
                Admission::Admitted => report.admitted += 1,
                Admission::Deferred => report.deferred += 1,
                Admission::Rejected => report.rejected += 1,
                Admission::Obsolete => unreachable!("an explicit hint has no pattern incarnation"),
            }
        }
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

    fn prefetch_admit(&self, control: &mut Control, page: PageId, source: Source) -> Admission {
        control.prefetch.reconcile_feedback(&self.clock);
        if control.prefetch.source_obsolete(source) {
            return Admission::Obsolete;
        }
        assert_eq!(
            page.file().driver(),
            self.identity,
            "prefetch uses its owning pool"
        );
        if !file_is_live(&control.files, page.file(), self.identity) {
            return Admission::Rejected;
        }
        if self.table.lookup(page).is_some() {
            return Admission::Resident;
        }
        if control.miss.find_pending(page).is_some() {
            return Admission::Pending;
        }
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
        let token = match self.submit_page_read(control, page, write, 0) {
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
        Admission::Admitted
    }

    fn prefetch_replace_unused(&self, control: &mut Control, protected: &[PageId]) {
        if control.prefetch.available() == 0 {
            self.prefetch_evict_unused(control, protected);
        }
    }

    pub(super) fn progress_prefetch(&self, control: &mut Control) {
        control.prefetch.reconcile(&self.clock);
        self.prefetch_evict_obsolete(control);
        self.prefetch_top_up(control);
        let readers =
            u32::try_from(control.prefetch.patterns.len()).expect("reader count fits u32");
        if readers == 0 {
            return;
        }
        let mut admitted = 0;
        for _ in 0..control.prefetch.capacity {
            let Some((page, source)) = Self::prefetch_next(control, readers) else {
                break;
            };
            let Source::Automatic { reader, .. } = source else {
                unreachable!("automatic request source");
            };
            control.prefetch.active = true;
            match self.prefetch_admit(control, page, source) {
                Admission::Deferred => break,
                Admission::Obsolete => {}
                Admission::Rejected => control.prefetch.patterns[reader as usize].reset(),
                outcome => {
                    control.prefetch.patterns[reader as usize].issued(page);
                    if matches!(outcome, Admission::Admitted) {
                        admitted += 1;
                    }
                }
            }
        }
        self.prefetch_top_up(control);
        if admitted > 0 {
            self.wake.wake();
        }
    }

    fn prefetch_next(control: &mut Control, readers: u32) -> Option<(PageId, Source)> {
        assert!(readers > 0);
        for _ in 0..readers {
            let reader = control.prefetch.reader_cursor;
            control.prefetch.reader_cursor = (reader + 1) % readers;
            if let Some(request) = control.prefetch.patterns[reader as usize].request(reader) {
                return Some(request);
            }
        }
        None
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
    }

    pub(super) fn prefetch_evict_unused(
        &self,
        control: &mut Control,
        protected: &[PageId],
    ) -> bool {
        control.prefetch.reconcile(&self.clock);
        for index in 0..control.prefetch.entries.len() {
            let Some(entry) = control.prefetch.entries[index] else {
                continue;
            };
            if protected.contains(&entry.page) {
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
            return true;
        }
        false
    }

    fn prefetch_evict_obsolete(&self, control: &mut Control) {
        for index in 0..control.prefetch.entries.len() {
            let Some(entry) = control.prefetch.entries[index] else {
                continue;
            };
            if !control.prefetch.obsolete(entry) {
                continue;
            }
            if self.frames.state(entry.frame) != FrameState::Resident {
                continue;
            }
            if control
                .miss
                .prepare_eviction(entry.frame, &self.miss_interests)
            {
                self.evict_resident(control, entry.frame);
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
