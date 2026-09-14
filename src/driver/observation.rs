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

#[derive(Debug)]
struct State {
    operations: Box<[Option<Operation>]>,
    owners: Box<[Option<Owner>]>,
    records: Vec<Record>,
    counters: Counters,
    dropped: u64,
    units: Option<&'static str>,
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
}

impl ReadObservation {
    pub(crate) fn try_new(
        config: ReadObservationConfig,
        granule: u32,
        operations: u32,
        frames: u32,
        product_metadata_bytes: u64,
    ) -> Option<Self> {
        let state = State {
            operations: try_boxed_slice_with(operations, || None)?,
            owners: try_boxed_slice_with(frames, || None)?,
            records: try_vec_with_exact_capacity(config.event_capacity)?,
            counters: Counters::default(),
            dropped: 0,
            units: None,
        };
        let capture_bytes = size_of_val(&*state.operations)
            + size_of_val(&*state.owners)
            + state.records.capacity() * size_of::<Record>()
            + size_of::<Self>();
        Some(Self {
            config,
            granule,
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

    /// Allocates a report after the measured workload and final drain.
    #[must_use]
    pub fn snapshot(&self) -> Value {
        let state = self.lock();
        let counters = &state.counters;
        let records: Vec<_> = state.records.iter().map(|record| record.value()).collect();
        json!({
            "io": counters.value(), "read_events": records,
            "io_units": state.units,
            "metadata_bytes": self.metadata_bytes,
            "capture_interval_start_page": self.config.interval_start_page,
            "capture_interval_pages": self.config.interval_pages,
            "capture_event_capacity": state.records.capacity(),
            "overflow": state.dropped, "dropped_events": state.dropped,
            "confirmed_window_page": null, "steady_intervals": null, "explicit_calls": null, "control": null,
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
