use std::hint::black_box;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dios::{
    FileId, FrameGuard, Get, PendingToken, Pool, PoolCompletion, PoolCompletionBatch, PoolToken,
    ReaderCtx, ReadyResult, RetainedFrame, SyncMode,
};

use super::catalog::{Arm, GRANULE, Lane, POLLS_MAX, PageNumber, WINDOW};
use super::fixture::{Fixture, display_error, fill_page, fold_bytes, page_id, prefill};
use super::observe::{
    Counters, Event, Observer, allocations_begin, allocations_end, nanoseconds, thread_cpu_ns,
};

const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Measurement {
    pub(super) elapsed_ns: u64,
    pub(super) foreground_ns: u64,
    pub(super) cpu_ns: Option<u64>,
    pub(super) counters: Counters,
    pub(super) events: Vec<Event>,
    pub(super) registration: String,
    pub(super) io_mode: String,
}

struct WorkerResult {
    foreground_ns: u64,
    cpu_ns: Option<u64>,
    counters: Counters,
    events: Vec<Event>,
}

#[derive(Clone, Copy)]
struct Work<'pool> {
    pool: &'pool Pool,
    files: &'pool [FileId; 8],
    output: FileId,
    lane: Lane,
    arm: Arm,
    round: u32,
}

pub(super) fn measure<const TRACE: bool>(
    fixture: &Fixture,
    lane: Lane,
    arm: Arm,
    round: u32,
    direct: dios::DirectIo,
) -> Result<Measurement, String> {
    let (pool, files, output) = fixture.pool(direct)?;
    prefill(&pool, &files, lane.warm_pages())?;
    let work = Work {
        pool: &pool,
        files: &files,
        output,
        lane,
        arm,
        round,
    };
    let mut measurement = measure_workers::<TRACE>(work)?;
    measurement.registration = format!("{:?}", pool.registration_posture());
    measurement.io_mode = format!("{:?}", pool.io_mode(files[0]));
    validate(fixture, work, &measurement)?;
    if lane == Lane::CompactionInterference && arm == Arm::Candidate {
        fixture.verify_output()?;
    }
    Ok(measurement)
}

fn measure_workers<const TRACE: bool>(work: Work<'_>) -> Result<Measurement, String> {
    let worker_count = work.lane.workers(work.arm);
    std::thread::scope(|scope| {
        let (initialized_tx, initialized_rx) = mpsc::sync_channel(worker_count as usize);
        let (result_tx, result_rx) = mpsc::sync_channel(worker_count as usize);
        let mut starts = Vec::with_capacity(worker_count as usize);
        let mut handles = Vec::with_capacity(worker_count as usize);
        for worker in 0..worker_count {
            let (start_tx, start_rx) = mpsc::sync_channel(1);
            starts.push(start_tx);
            let initialized = initialized_tx.clone();
            let result = result_tx.clone();
            handles.push(scope.spawn(move || {
                let reader = match work.pool.register_reader() {
                    Ok(reader) => reader,
                    Err(error) => {
                        let _ = initialized.send(Err(display_error(error)));
                        return;
                    }
                };
                let observer = Observer::<TRACE>::new(worker);
                let output = Output::new(work);
                if initialized.send(Ok(())).is_err() {
                    return;
                }
                let Ok(origin) = start_rx.recv_timeout(WORKER_TIMEOUT) else {
                    return;
                };
                let measured = run_worker(work, &reader, observer, output, origin);
                let _ = result.send(measured);
            }));
        }
        drop(initialized_tx);
        drop(result_tx);
        for _ in 0..worker_count {
            initialized_rx
                .recv_timeout(WORKER_TIMEOUT)
                .map_err(display_error)??;
        }
        let origin = Instant::now();
        for start in &starts {
            start.send(origin).map_err(display_error)?;
        }
        let mut results = [None, None, None, None];
        for entry in results.iter_mut().take(worker_count as usize) {
            *entry = Some(
                result_rx
                    .recv_timeout(WORKER_TIMEOUT)
                    .map_err(display_error)??,
            );
        }
        let elapsed_ns = nanoseconds(origin.elapsed());
        for handle in handles {
            handle
                .join()
                .map_err(|_| "workload worker panicked".to_owned())?;
        }
        Ok(measure_workers_combine(elapsed_ns, results))
    })
}

fn measure_workers_combine(elapsed_ns: u64, results: [Option<WorkerResult>; 4]) -> Measurement {
    let mut measured = Measurement {
        elapsed_ns,
        foreground_ns: 0,
        cpu_ns: Some(0),
        counters: Counters::default(),
        events: Vec::new(),
        registration: String::new(),
        io_mode: String::new(),
    };
    for result in results.into_iter().flatten() {
        measured.foreground_ns = measured.foreground_ns.max(result.foreground_ns);
        measured.cpu_ns = measured
            .cpu_ns
            .zip(result.cpu_ns)
            .map(|(sum, value)| sum + value);
        measured.counters.merge(result.counters);
        measured.events.extend(result.events);
    }
    measured
}

fn run_worker<const TRACE: bool>(
    work: Work<'_>,
    reader: &ReaderCtx,
    mut observer: Observer<TRACE>,
    mut output: Output,
    origin: Instant,
) -> Result<WorkerResult, String> {
    observer.origin = origin;
    let cpu_before = thread_cpu_ns()?;
    allocations_begin();
    let foreground = timed_workload(work, reader, &mut observer, &mut output);
    observer.counters.allocations = allocations_end();
    let foreground_ns = foreground?;
    let cpu_after = thread_cpu_ns()?;
    Ok(WorkerResult {
        foreground_ns,
        cpu_ns: cpu_before
            .zip(cpu_after)
            .map(|(before, after)| after - before),
        counters: observer.counters,
        events: observer.events,
    })
}

#[inline(never)]
fn timed_workload<const TRACE: bool>(
    work: Work<'_>,
    reader: &ReaderCtx,
    observer: &mut Observer<TRACE>,
    output: &mut Output,
) -> Result<u64, String> {
    match work.lane {
        Lane::RetainedSession if work.arm == Arm::Candidate => {
            retained_session(work, reader, observer)?;
        }
        Lane::DependentControl => dependent_chain(work, reader, observer, output)?,
        _ => read_window(work, reader, observer, output)?,
    }
    let foreground_ns = nanoseconds(observer.origin.elapsed());
    for _ in 0..POLLS_MAX {
        output.submit(work, observer)?;
        if output.finished() {
            return Ok(foreground_ns);
        }
        phase_poll(work.pool, output, observer)?;
    }
    Err("output drain exceeded its poll bound".to_owned())
}

struct PendingRead {
    token: PendingToken,
    operation: u32,
    page: PageNumber,
    admitted: Option<Instant>,
}

fn read_window<const TRACE: bool>(
    work: Work<'_>,
    reader: &ReaderCtx,
    observer: &mut Observer<TRACE>,
    output: &mut Output,
) -> Result<(), String> {
    let count = work.lane.operations() / work.lane.workers(work.arm);
    let first = observer.worker * count;
    let mut cursor = first;
    let mut pending: [Option<PendingRead>; WINDOW] = std::array::from_fn(|_| None);
    let mut outstanding = 0_u64;
    for _ in 0..POLLS_MAX {
        output.submit(work, observer)?;
        for slot in pending.iter_mut().take(work.lane.window(work.arm)) {
            if slot.is_none() {
                while cursor < first + count {
                    let page = work.lane.page(work.round, cursor);
                    let start = observer.start();
                    let get = work
                        .pool
                        .get(reader, page_id(work.files, page))
                        .map_err(display_error)?;
                    observer.end(start, cursor, page, "get");
                    match get {
                        Get::Hit(guard) => {
                            observer.counters.hits += 1;
                            consume_guard(guard, work.lane, cursor, page, observer);
                            cursor += 1;
                        }
                        Get::Pending(token) => {
                            observer.counters.pending += 1;
                            *slot = Some(PendingRead {
                                token,
                                operation: cursor,
                                page,
                                admitted: observer.start(),
                            });
                            outstanding += 1;
                            observer.counters.pending_max =
                                observer.counters.pending_max.max(outstanding);
                            cursor += 1;
                            break;
                        }
                        Get::Busy => {
                            observer.counters.busy += 1;
                            break;
                        }
                    }
                }
            }
        }
        if cursor == first + count && outstanding == 0 {
            return Ok(());
        }
        phase_poll(work.pool, output, observer)?;
        for slot in pending.iter_mut().take(work.lane.window(work.arm)) {
            if let Some(read) = slot.take() {
                *slot = ready_read(work, reader, read, observer)?;
                if slot.is_none() {
                    outstanding -= 1;
                }
            }
        }
    }
    Err("read window exceeded its poll bound".to_owned())
}

fn ready_read<const TRACE: bool>(
    work: Work<'_>,
    reader: &ReaderCtx,
    read: PendingRead,
    observer: &mut Observer<TRACE>,
) -> Result<Option<PendingRead>, String> {
    let start = observer.start();
    observer.counters.ready_checks += 1;
    let ready = work.pool.ready(reader, read.token);
    // Unsuccessful ready checks may be extremely frequent; counters retain them
    // without letting an idle device exhaust the per-request trace buffer.
    match ready {
        ReadyResult::Ready(guard) => {
            observer.end(start, read.operation, read.page, "ready");
            observer.end(read.admitted, read.operation, read.page, "pending");
            consume_guard(guard, work.lane, read.operation, read.page, observer);
            Ok(None)
        }
        ReadyResult::NotYet(token) => Ok(Some(PendingRead { token, ..read })),
        ReadyResult::Err(error) => Err(display_error(error)),
    }
}

#[inline(never)]
fn consume_guard<const TRACE: bool>(
    guard: FrameGuard<'_>,
    lane: Lane,
    operation: u32,
    page: PageNumber,
    observer: &mut Observer<TRACE>,
) {
    consume_bytes(&guard, lane, operation, page, observer);
    let start = observer.start();
    drop(guard);
    observer.end(start, operation, page, "release");
}

#[inline(never)]
fn consume_bytes<const TRACE: bool>(
    bytes: &[u8],
    lane: Lane,
    operation: u32,
    page: PageNumber,
    observer: &mut Observer<TRACE>,
) {
    let start = observer.start();
    let count = lane.useful_bytes() as usize;
    for _ in 0..lane.decode_passes() {
        observer.counters.checksum = observer
            .counters
            .checksum
            .wrapping_add(fold_bytes(black_box(&bytes[..count])));
    }
    observer.counters.operations += 1;
    observer.counters.useful_bytes += u64::from(lane.useful_bytes());
    observer.counters.processed_bytes += u64::from(lane.useful_bytes() * lane.decode_passes());
    observer.end(start, operation, page, "decode");
}

fn retained_session<const TRACE: bool>(
    work: Work<'_>,
    reader: &ReaderCtx,
    observer: &mut Observer<TRACE>,
) -> Result<(), String> {
    let mut retained: [Option<RetainedFrame<'_>>; 16] = std::array::from_fn(|_| None);
    for (index, slot) in retained.iter_mut().enumerate() {
        let page = PageNumber(u32::try_from(index).map_err(display_error)?);
        let start = observer.start();
        let Get::Hit(guard) = work
            .pool
            .get(reader, page_id(work.files, page))
            .map_err(display_error)?
        else {
            return Err("retention setup must hit its prefetched page".to_owned());
        };
        observer.counters.hits += 1;
        *slot = Some(
            guard
                .into_retained()
                .map_err(|_| "retention promotion refused".to_owned())?,
        );
        observer.end(start, page.0, page, "promote");
    }
    for operation in 0..work.lane.operations() {
        let page = work.lane.page(work.round, operation);
        let frame = retained[page.0 as usize]
            .as_ref()
            .expect("all session frames promoted");
        consume_bytes(frame, work.lane, operation, page, observer);
    }
    for (index, slot) in retained.iter_mut().enumerate() {
        let start = observer.start();
        drop(slot.take());
        let page = PageNumber(u32::try_from(index).map_err(display_error)?);
        observer.end(start, page.0, page, "release");
    }
    Ok(())
}

fn dependent_chain<const TRACE: bool>(
    work: Work<'_>,
    reader: &ReaderCtx,
    observer: &mut Observer<TRACE>,
    output: &mut Output,
) -> Result<(), String> {
    let mut page = work.lane.page(work.round, 0);
    for operation in 0..work.lane.operations() {
        let start = observer.start();
        let Get::Pending(mut token) = work
            .pool
            .get(reader, page_id(work.files, page))
            .map_err(display_error)?
        else {
            return Err("dependent chain must access distinct cold pages".to_owned());
        };
        observer.end(start, operation, page, "get");
        observer.counters.pending += 1;
        observer.counters.pending_max = 1;
        let admitted = observer.start();
        let mut next = None;
        for _ in 0..POLLS_MAX {
            phase_poll(work.pool, output, observer)?;
            observer.counters.ready_checks += 1;
            match work.pool.ready(reader, token) {
                ReadyResult::Ready(guard) => {
                    observer.end(admitted, operation, page, "pending");
                    let decoded =
                        u64::from_le_bytes(guard[..8].try_into().expect("next address word"));
                    next = Some(PageNumber(u32::try_from(decoded).map_err(display_error)?));
                    consume_guard(guard, work.lane, operation, page, observer);
                    break;
                }
                ReadyResult::NotYet(pending) => token = pending,
                ReadyResult::Err(error) => return Err(display_error(error)),
            }
        }
        page = next.ok_or_else(|| "dependent read exceeded poll bound".to_owned())?;
    }
    Ok(())
}

struct Output {
    limit: u32,
    submitted: u32,
    completed: u32,
    tokens: [Option<PoolToken>; WINDOW],
    barrier: Option<PoolToken>,
    barrier_done: bool,
    batch: PoolCompletionBatch,
}

impl Output {
    fn new(work: Work<'_>) -> Self {
        let limit = if work.lane == Lane::CompactionInterference {
            if work.arm == Arm::Candidate { 32 } else { 0 }
        } else {
            0
        };
        Self {
            limit,
            submitted: 0,
            completed: 0,
            tokens: [None; WINDOW],
            barrier: None,
            barrier_done: false,
            batch: PoolCompletionBatch::with_capacity(17),
        }
    }

    fn finished(&self) -> bool {
        self.limit == 0 || (self.completed == self.limit && self.barrier_done)
    }

    fn submit<const TRACE: bool>(
        &mut self,
        work: Work<'_>,
        observer: &mut Observer<TRACE>,
    ) -> Result<(), String> {
        for entry in &mut self.tokens {
            if self.submitted == self.limit {
                break;
            }
            if entry.is_none() {
                let mut slot = work
                    .pool
                    .write_arena()
                    .alloc()
                    .ok_or_else(|| "staging slot missing after completion".to_owned())?;
                let page = PageNumber(self.submitted);
                fill_page(&mut slot, page);
                let start = observer.start();
                *entry = Some(
                    work.pool
                        .submit_write(work.output, slot, u64::from(self.submitted * GRANULE))
                        .map_err(|(error, _)| format!("write submit: {error:?}"))?,
                );
                self.submitted += 1;
                observer.end(start, page.0, page, "write_submit");
            }
        }
        if self.limit > 0 && self.submitted == self.limit && self.barrier.is_none() {
            let start = observer.start();
            self.barrier = Some(
                work.pool
                    .submit_fsync(work.output, SyncMode::Data)
                    .map_err(|error| format!("barrier submit: {error:?}"))?,
            );
            observer.end(start, 0, PageNumber(0), "fsync_submit");
        }
        Ok(())
    }

    fn completions(&mut self, counters: &mut Counters) -> Result<(), String> {
        for completion in self.batch.iter() {
            match completion {
                PoolCompletion::Write { token, result } => {
                    let bytes = result.as_ref().map_err(display_error)?;
                    if *bytes != GRANULE {
                        return Err("short output write".to_owned());
                    }
                    let entry = self
                        .tokens
                        .iter_mut()
                        .find(|entry| **entry == Some(*token))
                        .ok_or_else(|| "unknown or duplicate write completion".to_owned())?;
                    *entry = None;
                    self.completed += 1;
                    counters.writes += 1;
                }
                PoolCompletion::Fsync { token, result } => {
                    result.as_ref().map_err(display_error)?;
                    if Some(*token) != self.barrier || self.barrier_done {
                        return Err("unknown or duplicate barrier completion".to_owned());
                    }
                    self.barrier_done = true;
                    counters.barriers += 1;
                }
            }
        }
        Ok(())
    }
}

#[inline(never)]
fn phase_poll<const TRACE: bool>(
    pool: &Pool,
    output: &mut Output,
    observer: &mut Observer<TRACE>,
) -> Result<(), String> {
    let report = pool.poll_report(&mut output.batch);
    observer.counters.polls += 1;
    observer.counters.poll_reclaimed += u64::from(report.reclaimed_frames());
    observer.counters.poll_backend_completions += u64::from(report.backend_completions());
    output.completions(&mut observer.counters)
}

fn validate(fixture: &Fixture, work: Work<'_>, measurement: &Measurement) -> Result<(), String> {
    let counters = measurement.counters;
    let expected_pending = match work.lane {
        Lane::MultirunReaders | Lane::RetainedSession => 0,
        Lane::PartitionPipeline | Lane::CompactionInterference => 32,
        _ => u64::from(work.lane.operations()),
    };
    let expected_hits = if work.lane == Lane::RetainedSession && work.arm == Arm::Candidate {
        16
    } else {
        u64::from(work.lane.operations()) - expected_pending
    };
    if counters.operations != u64::from(work.lane.operations())
        || counters.useful_bytes
            != u64::from(work.lane.operations()) * u64::from(work.lane.useful_bytes())
        || counters.checksum != fixture.expected(work.lane, work.round)
        || counters.pending != expected_pending
        || counters.hits != expected_hits
    {
        return Err(format!(
            "{} {} workload contract failed: {counters:?}",
            work.lane.name(),
            work.arm.name()
        ));
    }
    if expected_pending > 0 {
        let depth = if work.lane == Lane::CompactionInterference {
            WINDOW
        } else {
            work.lane.window(work.arm)
        };
        if counters.pending_max != depth as u64 {
            return Err(format!(
                "expected pending depth {depth}, got {}",
                counters.pending_max
            ));
        }
    }
    // A pressured get can drain completions internally, so explicit poll reports
    // are a lower bound. Exact completed reads are proven by useful work above.
    if counters.poll_backend_completions > counters.pending + counters.writes + counters.barriers {
        return Err("poll reports exceed admitted work".to_owned());
    }
    if counters.allocations != 0 {
        return Err(format!(
            "{}/{} timed worker allocated {} times",
            work.lane.name(),
            work.arm.name(),
            counters.allocations
        ));
    }
    if work.lane == Lane::CompactionInterference
        && work.arm == Arm::Candidate
        && (counters.writes != 32 || counters.barriers != 1)
    {
        return Err("background work did not drain".to_owned());
    }
    Ok(())
}
