//! Construction-sized, clock-free observations of accepted read attempts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use super::{FileId, FrameLease, OpEntry, OpKind, OpToken, ReadDestination, ReadPurpose};
use crate::allocation::{try_boxed_slice_with, try_vec_with_exact_capacity};
use crate::pool::{PageId, ReadFrameIdx};

/// Selects bounded detailed capture; counters cover every observed read.
#[derive(Debug, Clone, Copy)]
pub struct ReadObservationConfig {
    pub event_capacity: u32,
    /// A zero-length interval records all events, as used by fault witnesses.
    pub interval_start_page: u32,
    pub interval_pages: u32,
    pub consumer_stop_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Owner {
    token: OpToken,
    page: PageId,
    pass: u32,
    attempts: u64,
    end: u64,
    purpose: ReadPurpose,
}

#[derive(Debug, Clone, Copy)]
enum Shape {
    Point(ReadFrameIdx),
    Vector,
}

#[derive(Debug, Clone, Copy)]
struct Operation {
    token: OpToken,
    file: FileId,
    pass: u32,
    purpose: ReadPurpose,
    attempts: u64,
    end: u64,
    shape: Shape,
    active: Option<Record>,
}

#[derive(Debug, Clone, Copy)]
enum Event {
    Attempt,
    Completion,
    Publication,
    Failure,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    event: Event,
    token: OpToken,
    file: FileId,
    pass: u32,
    purpose: ReadPurpose,
    attempt: u64,
    offset: u64,
    bytes: u32,
    pages: u32,
    frame: Option<ReadFrameIdx>,
    result: Option<Result<u32, i32>>,
}

#[derive(Debug, Default)]
struct Counters {
    read_sqes: u64,
    demand_read_sqes: u64,
    speculative_read_sqes: u64,
    continuation_sqes: u64,
    cqes: u64,
    read_bytes: u64,
    requested_bytes: u64,
    publications: u64,
    failures: u64,
    read_credits: u64,
    destinations: u64,
    eof_sqes: u64,
    eof_bytes: u64,
    beyond_stop_sqes: u64,
    beyond_stop_bytes: u64,
    short_bytes: u64,
    pending_requests: u64,
    pending_bytes: u64,
    pending_requests_max: u64,
    pending_bytes_max: u64,
    refills_while_pending: u64,
    initial_lengths: [u64; 32],
    continuation_lengths: [u64; 32],
}

#[derive(Debug, Clone, Copy)]
struct ControlRecord {
    capacity: u32,
    cause: &'static str,
    visits: u32,
    affected: u32,
    cleanup: u32,
    recovered: u32,
    cqes: u64,
    pending_requests: u64,
}

#[derive(Debug, Clone, Copy)]
struct ExplicitRecord {
    capacity: u32,
    work: [u64; 6],
}

#[derive(Debug, Clone, Copy)]
struct RunRecord {
    page: PageId,
    pages: u32,
    pass: u32,
    stream: Option<(u32, u64)>,
}

#[derive(Debug, Clone, Copy)]
struct TurnRecord {
    reader: u32,
    after: Option<u32>,
    admitted: u32,
    outcome: &'static str,
    ready_start: usize,
    ready_end: usize,
}

#[derive(Debug)]
struct State {
    operations: Box<[Option<Operation>]>,
    owners: Box<[Option<Owner>]>,
    records: Vec<Record>,
    counters: Counters,
    dropped: u64,
    units: Option<&'static str>,
    control: Vec<ControlRecord>,
    explicit: Vec<ExplicitRecord>,
    runs: Vec<RunRecord>,
    turns: Vec<TurnRecord>,
    ready_members: Vec<u32>,
    confirmed_window: Option<u32>,
    automatic: bool,
    control_count: u64,
    control_entry_visits_total: u64,
}

/// Bench-only capture shared by driver attempts and pool terminal transitions.
/// Snapshot construction is outside the timed region; recording never allocates.
#[derive(Debug)]
pub struct ReadObservation {
    config: ReadObservationConfig,
    granule: u32,
    position: AtomicU64,
    state: Mutex<State>,
    metadata_bytes: u64,
    pool_metadata: (u64, u64),
}

impl ReadObservation {
    pub(crate) fn try_new(
        config: ReadObservationConfig,
        granule: u32,
        operations: u32,
        frames: u32,
        product_metadata_bytes: u64,
        readers: u32,
        pool_metadata: (u64, u64),
    ) -> Option<Self> {
        let state = State {
            operations: try_boxed_slice_with(operations, || None)?,
            owners: try_boxed_slice_with(frames, || None)?,
            records: try_vec_with_exact_capacity(config.event_capacity)?,
            counters: Counters::default(),
            dropped: 0,
            units: None,
            control: try_vec_with_exact_capacity(config.event_capacity)?,
            explicit: try_vec_with_exact_capacity(config.event_capacity)?,
            runs: try_vec_with_exact_capacity(config.event_capacity)?,
            turns: try_vec_with_exact_capacity(config.event_capacity)?,
            ready_members: try_vec_with_exact_capacity(
                config.event_capacity.checked_mul(readers)?,
            )?,
            confirmed_window: None,
            automatic: false,
            control_count: 0,
            control_entry_visits_total: 0,
        };
        let capture_bytes = size_of_val(&*state.operations)
            + size_of_val(&*state.owners)
            + state.records.capacity() * size_of::<Record>()
            + state.control.capacity() * size_of::<ControlRecord>()
            + state.explicit.capacity() * size_of::<ExplicitRecord>()
            + state.runs.capacity() * size_of::<RunRecord>()
            + state.turns.capacity() * size_of::<TurnRecord>()
            + state.ready_members.capacity() * size_of::<u32>()
            + size_of::<Self>();
        Some(Self {
            config,
            granule,
            pool_metadata,
            position: AtomicU64::new(0),
            state: Mutex::new(state),
            metadata_bytes: product_metadata_bytes
                .checked_add(u64::try_from(capture_bytes).ok()?)?,
        })
    }

    /// Supplies consumer position without a clock or a pool poll.
    pub fn scan_position(&self, pass: u32, useful_page: u32) {
        self.position.store(
            (u64::from(pass) << 32) | u64::from(useful_page),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn position(&self) -> (u32, u32) {
        let position = self.position.load(Ordering::Relaxed);
        (
            u32::try_from(position >> 32).expect("pass word"),
            u32::try_from(position & u64::from(u32::MAX)).expect("page word"),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn admit(&self, token: OpToken, entry: &OpEntry) {
        if entry.kind != OpKind::Read || entry.frame_lease != FrameLease::Pool {
            return;
        }
        let mut state = self.lock();
        let pass = self.position().0;
        let end = entry.file_offset + u64::from(entry.requested_len);
        let mut first = None;
        let mut install = |ordinal: u32, frame: ReadFrameIdx| {
            first.get_or_insert(frame);
            if entry.read_purpose != ReadPurpose::Continuation {
                let page =
                    u32::try_from(entry.file_offset / u64::from(self.granule)).expect("page index");
                assert!(
                    state.owners[frame.get() as usize]
                        .replace(Owner {
                            token,
                            page: PageId::new(entry.fd, page + ordinal),
                            pass,
                            attempts: 0,
                            end,
                            purpose: entry.read_purpose,
                        })
                        .is_none()
                );
                state.counters.read_credits += 1;
                state.counters.destinations += 1;
            }
        };
        match entry.frame.as_ref().expect("read destination") {
            ReadDestination::Point(frame) => install(0, frame.frame()),
            ReadDestination::Vector(vector) => vector.visit_frames(install),
        }
        let first = first.expect("at least one read destination");
        let owner = state.owners[first.get() as usize].expect("initial destination owner");
        if let Some(previous) = state.operations[token.slot() as usize] {
            assert!(previous.active.is_none(), "slot reuse follows its last CQE");
        }
        state.operations[token.slot() as usize] = Some(Operation {
            token,
            file: entry.fd,
            pass: owner.pass,
            purpose: entry.read_purpose,
            attempts: owner.attempts,
            end: owner.end,
            shape: match entry.frame.as_ref().expect("read destination") {
                ReadDestination::Point(_) => Shape::Point(first),
                ReadDestination::Vector(_) => Shape::Vector,
            },
            active: None,
        });
    }

    pub(super) fn attempt(
        &self,
        token: OpToken,
        offset: u64,
        bytes: u32,
        pages: u32,
        units: &'static str,
    ) {
        let mut state = self.lock();
        let Some(mut operation) = state.operations[token.slot() as usize] else {
            return;
        };
        if operation.token != token {
            return;
        }
        if let Some(prior) = state.units {
            assert_eq!(prior, units);
        } else {
            state.units = Some(units);
        }
        assert!(operation.active.is_none());
        assert!((1..=32).contains(&pages));
        let purpose = if operation.attempts == 0 {
            operation.purpose
        } else {
            ReadPurpose::Continuation
        };
        let record = Record {
            event: Event::Attempt,
            token,
            file: operation.file,
            pass: operation.pass,
            purpose,
            attempt: operation.attempts,
            offset,
            bytes,
            pages,
            frame: None,
            result: None,
        };
        state
            .counters
            .attempt(record, self.config.consumer_stop_bytes);
        operation.attempts += 1;
        operation.active = Some(record);
        state.operations[token.slot() as usize] = Some(operation);
        self.record(&mut state, record);
    }

    pub(super) fn complete(&self, token: OpToken, result: Result<u32, i32>) {
        let mut state = self.lock();
        let Some(mut operation) = state.operations[token.slot() as usize] else {
            return;
        };
        if operation.token != token {
            return;
        }
        let mut record = operation
            .active
            .take()
            .expect("each CQE ends one accepted attempt");
        record.event = Event::Completion;
        record.result = Some(result);
        let counters = &mut state.counters;
        counters.cqes += 1;
        counters.pending_requests = counters
            .pending_requests
            .checked_sub(1)
            .expect("accepted request");
        counters.pending_bytes = counters
            .pending_bytes
            .checked_sub(u64::from(record.bytes))
            .expect("accepted bytes");
        if let Ok(bytes) = result {
            assert!(bytes <= record.bytes);
            counters.read_bytes += u64::from(bytes);
            if bytes == 0 {
                let wasted = operation.end - record.offset;
                counters.eof_sqes += operation.attempts;
                counters.eof_bytes += operation.attempts * wasted;
            } else if bytes < record.bytes {
                counters.short_bytes += u64::from(bytes);
            }
        }
        if let Shape::Point(first) = operation.shape {
            state.owners[first.get() as usize]
                .as_mut()
                .expect("the point tail remains owned")
                .attempts = operation.attempts;
        }
        state.operations[token.slot() as usize] = Some(operation);
        self.record(&mut state, record);
    }

    pub(crate) fn terminal(&self, frame: ReadFrameIdx, page: PageId, outcome: Result<(), i32>) {
        let mut state = self.lock();
        let owner = state.owners[frame.get() as usize]
            .take()
            .expect("one observed destination owner");
        assert_eq!(owner.page, page);
        let counters = &mut state.counters;
        counters.read_credits = counters
            .read_credits
            .checked_sub(1)
            .expect("one terminal read credit");
        counters.destinations = counters
            .destinations
            .checked_sub(1)
            .expect("one terminal destination");
        let event = if outcome.is_ok() {
            counters.publications += 1;
            Event::Publication
        } else {
            counters.failures += 1;
            Event::Failure
        };
        self.record(
            &mut state,
            Record {
                event,
                token: owner.token,
                file: page.file(),
                pass: owner.pass,
                purpose: owner.purpose,
                attempt: owner.attempts,
                offset: u64::from(page.granule_idx()) * u64::from(self.granule),
                bytes: self.granule,
                pages: 1,
                frame: Some(frame),
                result: Some(outcome.map(|()| self.granule)),
            },
        );
    }

    fn record(&self, state: &mut State, record: Record) {
        if self.config.interval_pages > 0 {
            let start = u64::from(self.config.interval_start_page) * u64::from(self.granule);
            let end = start + u64::from(self.config.interval_pages) * u64::from(self.granule);
            if record.offset >= end || record.offset + u64::from(record.bytes) <= start {
                return;
            }
        }
        if state.records.len() == state.records.capacity() {
            state.dropped += 1;
        } else {
            state.records.push(record);
        }
    }

    pub(crate) fn confirmed_window(&self) {
        let mut state = self.lock();
        state.confirmed_window.get_or_insert(self.position().1);
    }

    fn capture_control(&self) -> bool {
        if self.config.interval_pages == 0 {
            return true;
        }
        let useful_byte = u64::from(self.position().1) * 4096;
        let start = u64::from(self.config.interval_start_page) * u64::from(self.granule);
        let end = start + u64::from(self.config.interval_pages) * u64::from(self.granule);
        (start..end).contains(&useful_byte)
    }

    pub(crate) fn control_count(&self) -> u64 {
        self.lock().control_count
    }

    pub(crate) fn control(
        &self,
        capacity: u32,
        cause: &'static str,
        visits: u32,
        affected: u32,
        cleanup: u32,
        recovered: u32,
    ) {
        assert!(affected <= 32);
        assert!(visits <= affected + cleanup);
        let mut state = self.lock();
        state.control_count += 1;
        state.control_entry_visits_total = state
            .control_entry_visits_total
            .checked_add(u64::from(visits))
            .expect("whole-run control entry visits fit u64");
        if !self.capture_control() {
            return;
        }
        if state.control.len() == state.control.capacity() {
            state.dropped += 1;
            return;
        }
        let cqes = state.counters.cqes;
        let pending_requests = state.counters.pending_requests;
        state.control.push(ControlRecord {
            capacity,
            cause,
            visits,
            affected,
            cleanup,
            recovered,
            cqes,
            pending_requests,
        });
    }

    pub(crate) fn explicit(&self, capacity: u32, work: [u64; 6]) {
        if !self.capture_control() {
            return;
        }
        let mut state = self.lock();
        if state.explicit.len() == state.explicit.capacity() {
            state.dropped += 1;
            return;
        }
        state.explicit.push(ExplicitRecord { capacity, work });
    }

    pub(crate) fn committed_run(&self, page: PageId, pages: u32, stream: Option<(u32, u64)>) {
        let mut state = self.lock();
        state.automatic |= stream.is_some();
        if self.config.interval_pages > 0 {
            let start = self.config.interval_start_page;
            let end = u64::from(start) + u64::from(self.config.interval_pages);
            if u64::from(page.granule_idx()) >= end
                || u64::from(page.granule_idx()) + u64::from(pages) <= u64::from(start)
            {
                return;
            }
        }
        if state.runs.len() == state.runs.capacity() {
            state.dropped += 1;
            return;
        }
        state.runs.push(RunRecord {
            page,
            pages,
            pass: self.position().0,
            stream,
        });
    }

    pub(crate) fn reader_turn(
        &self,
        reader: u32,
        after: Option<u32>,
        admitted: u32,
        outcome: &'static str,
        ready: impl Iterator<Item = u32>,
    ) -> Option<usize> {
        if !self.capture_control() {
            return None;
        }
        let mut state = self.lock();
        if state.turns.len() == state.turns.capacity() {
            state.dropped += 1;
            return None;
        }
        let ready_start = state.ready_members.len();
        for member in ready {
            if state.ready_members.len() == state.ready_members.capacity() {
                state.dropped += 1;
                return None;
            }
            state.ready_members.push(member);
        }
        let ready_end = state.ready_members.len();
        let index = state.turns.len();
        state.turns.push(TurnRecord {
            reader,
            after,
            admitted,
            outcome,
            ready_start,
            ready_end,
        });
        Some(index)
    }

    pub(crate) fn reader_turn_after(
        &self,
        index: Option<usize>,
        after: Option<u32>,
        admitted: u32,
        outcome: &'static str,
    ) {
        if let Some(index) = index {
            let mut state = self.lock();
            let turn = &mut state.turns[index];
            turn.after = after;
            turn.admitted = admitted;
            turn.outcome = outcome;
        }
    }

    fn snapshot_steady_intervals(&self, state: &State) -> Vec<Value> {
        if !state.automatic || self.config.interval_pages == 0 {
            return Vec::new();
        }
        (0..=self.position().0).map(|pass| {
            let reads: Vec<_> = state.records.iter().filter(|record| {
                record.pass == pass && matches!(record.event, Event::Attempt)
                    && record.purpose != ReadPurpose::Continuation
            }).map(|record| json!({
                "kind": if record.purpose == ReadPurpose::Demand { "demand" } else { "speculative" },
                "page": record.offset / u64::from(self.granule), "pages": record.pages,
            })).collect();
            let refills = state.runs.iter().filter(|run| run.pass == pass && run.stream.is_some()).count();
            json!({"pass_index": pass, "start_page": self.config.interval_start_page,
                "pages": self.config.interval_pages, "refills": refills, "reads": reads})
        }).collect()
    }

    /// Allocates a report after the measured workload and final drain.
    #[must_use]
    pub fn snapshot(&self) -> Value {
        let state = self.lock();
        let counters = &state.counters;
        let records: Vec<_> = state.records.iter().map(|record| record.value()).collect();
        let control: Vec<_> = state.control.iter().map(|event| json!({
            "capacity": event.capacity, "cause": event.cause, "entry_visits": event.visits,
            "affected_entries": event.affected, "cleanup_entries": event.cleanup,
            "credits_recovered": event.recovered, "after_last_cqe": event.cqes > 0 && event.pending_requests == 0 && event.cqes == counters.cqes,
        })).collect();
        let explicit: Vec<_> = state.explicit.iter().map(|call| json!({
            "capacity": call.capacity, "examined": call.work[0], "protection_lookups": call.work[1],
            "replacement_visits": call.work[2], "admission_visits": call.work[3], "clock_visits": call.work[4],
            "protected_evictions": call.work[5],
        })).collect();
        let turns: Vec<_> = state
            .turns
            .iter()
            .map(|turn| {
                let mut ready = state.ready_members[turn.ready_start..turn.ready_end].to_vec();
                ready.sort_unstable();
                json!({"ready": ready, "stream": turn.reader, "turn_before": turn.reader,
                "turn_after": turn.after, "admitted_pages": turn.admitted, "outcome": turn.outcome})
            })
            .collect();
        let runs: Vec<_> = state.runs.iter().map(|run| json!({
            "pass_index": run.pass, "page": run.page.granule_idx(), "pages": run.pages,
            "file_slot": run.page.file().slot(), "stream": run.stream.map(|stream| stream.0),
            "incarnation": run.stream.map(|stream| stream.1),
        })).collect();
        json!({
            "io": counters.value(), "read_events": records,
            "io_units": state.units,
            "metadata_bytes": self.metadata_bytes,
            "capture_interval_start_page": self.config.interval_start_page,
            "capture_interval_pages": self.config.interval_pages,
            "capture_event_capacity": state.records.capacity(),
            "overflow": state.dropped, "dropped_events": state.dropped,
            "confirmed_window_page": state.confirmed_window,
            "steady_intervals": self.snapshot_steady_intervals(&state),
            "explicit_calls": explicit, "control": control, "shared_reader_turns": turns,
            "control_entry_visits_total": state.control_entry_visits_total,
            "committed_runs": runs, "notification_metadata_bytes": self.pool_metadata.0,
            "index_metadata_bytes": self.pool_metadata.1,
        })
    }
}

impl Record {
    fn value(self) -> Value {
        let event = match self.event {
            Event::Attempt => "attempt",
            Event::Completion => "completion",
            Event::Publication => "publication",
            Event::Failure => "failure",
        };
        let kind = match self.purpose {
            ReadPurpose::Demand => "demand",
            ReadPurpose::Speculative => "speculative",
            ReadPurpose::Continuation => "continuation",
        };
        json!({"event": event, "kind": kind, "token": self.token.user_data(),
            "generation": self.token.user_data() >> 32, "slot": self.token.slot(),
            "file_slot": self.file.slot(), "file_generation": self.file.generation(),
            "pass_index": self.pass, "offset": self.offset, "bytes": self.bytes,
            "pages": self.pages, "attempt": self.attempt, "frame": self.frame.map(ReadFrameIdx::get),
            "result": self.result.map(|result| result.map_or_else(|errno| -i64::from(errno), i64::from))})
    }
}

impl Counters {
    fn attempt(&mut self, record: Record, stop: Option<u64>) {
        self.read_sqes += 1;
        self.requested_bytes += u64::from(record.bytes);
        match record.purpose {
            ReadPurpose::Demand => self.demand_read_sqes += 1,
            ReadPurpose::Speculative => {
                self.speculative_read_sqes += 1;
                self.initial_lengths[record.pages as usize - 1] += 1;
                if self.pending_requests > 0 {
                    self.refills_while_pending += 1;
                }
            }
            ReadPurpose::Continuation => {
                self.continuation_sqes += 1;
                self.continuation_lengths[record.pages as usize - 1] += 1;
            }
        }
        self.pending_requests += 1;
        self.pending_bytes += u64::from(record.bytes);
        self.pending_requests_max = self.pending_requests_max.max(self.pending_requests);
        self.pending_bytes_max = self.pending_bytes_max.max(self.pending_bytes);
        if let Some(stop) = stop {
            let waste =
                (record.offset + u64::from(record.bytes)).saturating_sub(record.offset.max(stop));
            if waste > 0 {
                self.beyond_stop_sqes += 1;
                self.beyond_stop_bytes += waste;
            }
        }
    }

    fn value(&self) -> Value {
        let lengths = |histogram: &[u64; 32]| -> Vec<Value> {
            histogram
                .iter()
                .enumerate()
                .filter(|(_, requests)| **requests > 0)
                .map(|(index, requests)| json!({"pages": index + 1, "requests": requests}))
                .collect()
        };
        json!({
            "read_sqes": self.read_sqes, "demand_read_sqes": self.demand_read_sqes,
            "speculative_read_sqes": self.speculative_read_sqes, "continuation_sqes": self.continuation_sqes,
            "cqes": self.cqes, "read_bytes": self.read_bytes, "requested_bytes": self.requested_bytes,
            "publications": self.publications, "failures": self.failures,
            "terminal_read_credits": self.read_credits, "terminal_destinations": self.destinations,
            "eof_sqes": self.eof_sqes, "eof_bytes": self.eof_bytes,
            "beyond_stop_sqes": self.beyond_stop_sqes, "beyond_stop_bytes": self.beyond_stop_bytes,
            "short_bytes": self.short_bytes, "pending_requests_max": self.pending_requests_max,
            "pending_bytes_max": self.pending_bytes_max, "refills_while_pending": self.refills_while_pending,
            "initial_vector_lengths": lengths(&self.initial_lengths),
            "continuation_vector_lengths": lengths(&self.continuation_lengths),
        })
    }
}
