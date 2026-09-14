use std::hint::black_box;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dios::{FileId, Get, PageId, PendingToken, Pool, PoolCompletionBatch, ReaderCtx, ReadyResult};
use serde::Serialize;

use super::catalog::{Arm, Cache, Lane, POLLS_MAX, PageNumber, WINDOW, consume};
use super::fixture::error;
use super::observe::{
    Counters, Event, FlightEvent, Observer, allocations_begin, allocations_end, nanoseconds,
};
use super::os::{self, Mapping, Usage};

const WORKER_TIMEOUT: Duration = Duration::from_mins(3);

#[derive(Clone, Copy)]
pub(super) enum Source<'a> {
    Mmap(&'a Mapping),
    Dios(&'a Pool, FileId),
}

#[derive(Clone, Copy)]
pub(super) struct Work<'a> {
    pub(super) source: Source<'a>,
    pub(super) lane: Lane,
    pub(super) arm: Arm,
    pub(super) seed: u32,
}

#[derive(Serialize)]
pub(super) struct WorkerResult {
    pub(super) worker: u32,
    pub(super) elapsed_ns: u64,
    pub(super) finished_ns: u64,
    pub(super) cpu_ns: u64,
    pub(super) setup_ns: u64,
    pub(super) teardown_ns: u64,
    pub(super) retained_payload_bytes: u64,
    pub(super) setup_storage_bytes: u64,
    #[serde(flatten)]
    pub(super) counters: Counters,
    #[serde(flatten)]
    pub(super) usage: Usage,
    pub(super) events: Vec<Event>,
    pub(super) flights: Vec<FlightEvent>,
}

pub(super) fn measure<const TRACE: bool>(work: Work<'_>) -> Result<Vec<WorkerResult>, String> {
    if work.lane.resident_access() {
        return super::resident::measure::<TRACE>(work).map(|worker| vec![worker]);
    }
    std::thread::scope(|scope| {
        let count = work.lane.workers() as usize;
        let (prepared_tx, prepared_rx) = mpsc::sync_channel(count);
        let mut starts = Vec::with_capacity(count);
        let mut handles = Vec::with_capacity(count);
        for worker in 0..work.lane.workers() {
            let (start_tx, start_rx) = mpsc::sync_channel(1);
            starts.push(start_tx);
            let prepared = prepared_tx.clone();
            handles.push(scope.spawn(move || {
                let setup = prepare_worker::<TRACE>(work, worker);
                let state = match setup {
                    Ok(state) => {
                        prepared.send(Ok(())).map_err(error)?;
                        state
                    }
                    Err(failure) => {
                        let _ = prepared.send(Err(failure.clone()));
                        return Err(failure);
                    }
                };
                let origin = start_rx.recv_timeout(WORKER_TIMEOUT).map_err(error)?;
                run_worker(work, worker, state, origin)
            }));
        }
        drop(prepared_tx);
        for _ in 0..count {
            prepared_rx.recv_timeout(WORKER_TIMEOUT).map_err(error)??;
        }
        let origin = Instant::now();
        for start in starts {
            start.send(origin).map_err(error)?;
        }
        let mut results = Vec::with_capacity(count);
        for handle in handles {
            results.push(handle.join().map_err(|_| "worker panicked".to_owned())??);
        }
        Ok(results)
    })
}

struct Prepared<const TRACE: bool> {
    reader: Option<ReaderCtx>,
    observer: Observer<TRACE>,
    requests: Vec<PageNumber>,
    completions: PoolCompletionBatch,
}

fn prepare_worker<const TRACE: bool>(
    work: Work<'_>,
    worker: u32,
) -> Result<Prepared<TRACE>, String> {
    os::pin(worker).map_err(error)?;
    let reader = match work.source {
        Source::Mmap(_) => None,
        Source::Dios(pool, _) => Some(pool.register_reader().map_err(error)?),
    };
    let operations = work.lane.operations() / work.lane.workers();
    let requests = (0..operations)
        .map(|index| {
            work.lane
                .page(work.seed, index * work.lane.workers() + worker)
        })
        .collect();
    let state = Prepared {
        reader,
        observer: Observer::new(operations),
        requests,
        completions: PoolCompletionBatch::with_capacity(0),
    };
    os::cpu_ns().map_err(error)?;
    os::usage().map_err(error)?;
    allocations_begin();
    assert_eq!(allocations_end(), 0);
    Ok(state)
}

fn run_worker<const TRACE: bool>(
    work: Work<'_>,
    worker: u32,
    mut state: Prepared<TRACE>,
    origin: Instant,
) -> Result<WorkerResult, String> {
    state.observer.origin = origin;
    let usage_before = os::usage().map_err(error)?;
    let cpu_before = os::cpu_ns().map_err(error)?;
    let started = Instant::now();
    allocations_begin();
    let result = timed_workload(work, &mut state);
    let allocations = allocations_end();
    let elapsed_ns = nanoseconds(started.elapsed());
    let finished_ns = nanoseconds(origin.elapsed());
    let cpu_ns = os::cpu_ns().map_err(error)? - cpu_before;
    let usage = os::usage().map_err(error)?.since(usage_before);
    result?;
    state.observer.counters.allocations = allocations;
    assert_eq!(
        state.observer.counters.operations as usize,
        state.requests.len()
    );
    assert_eq!(
        state.observer.counters.pending,
        state.observer.counters.completed_pending
    );
    Ok(WorkerResult {
        worker,
        elapsed_ns,
        finished_ns,
        cpu_ns,
        setup_ns: 0,
        teardown_ns: 0,
        retained_payload_bytes: 0,
        setup_storage_bytes: 0,
        counters: state.observer.counters,
        usage,
        events: state.observer.events,
        flights: state.observer.flights,
    })
}

#[inline(never)]
fn timed_workload<const TRACE: bool>(
    work: Work<'_>,
    state: &mut Prepared<TRACE>,
) -> Result<(), String> {
    match work.source {
        Source::Mmap(mapping) => {
            read_mmap(mapping, work.lane, state);
            Ok(())
        }
        Source::Dios(pool, file) if work.arm == Arm::DiosWrongHints => {
            read_dios(pool, file, work, state)
        }
        Source::Dios(pool, file) => match work.lane.cache() {
            Cache::Resident | Cache::Minor => read_dios_resident(pool, file, work.lane, state),
            _ => read_dios(pool, file, work, state),
        },
    }
}

fn read_dios_resident<const TRACE: bool>(
    pool: &Pool,
    file: FileId,
    lane: Lane,
    state: &mut Prepared<TRACE>,
) -> Result<(), String> {
    let reader = state.reader.as_ref().expect("Dios reader");
    for (index, page) in state.requests.iter().enumerate() {
        let started = state.observer.start();
        let guard = match pool.get(reader, PageId::new(file, page.0)).map_err(error)? {
            Get::Hit(guard) => guard,
            Get::Pending(_) => return Err("resident trace missed in pool".to_owned()),
            Get::Busy => return Err("resident trace was Busy".to_owned()),
        };
        let sum = black_box(consume(black_box(&guard[..lane.useful_bytes() as usize])));
        state.observer.counters.hits += 1;
        state
            .observer
            .finish(u32::try_from(index).expect("index"), *page, started, sum);
    }
    Ok(())
}

fn read_mmap<const TRACE: bool>(mapping: &Mapping, lane: Lane, state: &mut Prepared<TRACE>) {
    let mut current = state.requests[0];
    for (index, requested) in state.requests.iter().enumerate() {
        let page = if lane.dependent() {
            current
        } else {
            *requested
        };
        let started = state.observer.start();
        let bytes = &mapping.page(black_box(page))[..lane.useful_bytes() as usize];
        let sum = black_box(consume(black_box(bytes)));
        if lane.dependent() {
            current = next_page(bytes);
        }
        state.observer.finish(
            u32::try_from(index).expect("request index"),
            page,
            started,
            sum,
        );
    }
}

struct Waiting {
    token: PendingToken,
    page: PageNumber,
    index: u32,
    started: u64,
}

struct Pump<'a, const TRACE: bool> {
    pool: &'a Pool,
    file: FileId,
    lane: Lane,
    state: &'a mut Prepared<TRACE>,
    waiting: [Option<Waiting>; WINDOW],
    submitted: usize,
    current: PageNumber,
    outstanding: u32,
    arm: Arm,
    hint_next: usize,
}

fn read_dios<const TRACE: bool>(
    pool: &Pool,
    file: FileId,
    work: Work<'_>,
    state: &mut Prepared<TRACE>,
) -> Result<(), String> {
    let current = state.requests[0];
    let operations = state.requests.len();
    let mut pump = Pump {
        pool,
        file,
        lane: work.lane,
        state,
        submitted: 0,
        waiting: std::array::from_fn(|_| None),
        current,
        outstanding: 0,
        arm: work.arm,
        hint_next: 0,
    };
    let mut stalled = 0;
    let turns_max = u64::try_from(operations).expect("operations") * u64::from(POLLS_MAX);
    for _ in 0..turns_max {
        let before = pump.state.observer.counters.operations;
        pump_prefetch(&mut pump);
        pump_admit(&mut pump, work.arm.window())?;
        pump_ready(&mut pump)?;
        if pump.state.observer.counters.operations as usize == operations {
            assert_eq!(pump.outstanding, 0);
            return pump_drain(&mut pump);
        }
        if pump.state.observer.counters.operations != before {
            stalled = 0;
            if pump.outstanding == 0 {
                continue;
            }
        }
        pump_poll(&mut pump);
        if pump.state.observer.counters.operations == before {
            stalled += 1;
        } else {
            stalled = 0;
        }
        if stalled == POLLS_MAX {
            return Err("read progress exceeded fixed poll limit".to_owned());
        }
    }
    Err("workload exceeded fixed turn bound".to_owned())
}

fn pump_admit<const TRACE: bool>(pump: &mut Pump<'_, TRACE>, window: usize) -> Result<(), String> {
    for slot in 0..window {
        if pump.submitted == pump.state.requests.len() {
            break;
        }
        if pump.waiting[slot].is_some() {
            continue;
        }
        let page = if pump.lane.dependent() {
            pump.current
        } else {
            pump.state.requests[pump.submitted]
        };
        let started = pump.state.observer.start();
        let reader = pump.state.reader.as_ref().expect("Dios reader");
        match pump
            .pool
            .get(reader, PageId::new(pump.file, page.0))
            .map_err(error)?
        {
            Get::Hit(guard) => {
                pump.state.observer.counters.hits += 1;
                let bytes = &guard[..pump.lane.useful_bytes() as usize];
                let sum = black_box(consume(black_box(bytes)));
                if pump.lane.dependent() {
                    pump.current = next_page(bytes);
                }
                pump.state.observer.finish(
                    u32::try_from(pump.submitted).expect("index"),
                    page,
                    started,
                    sum,
                );
            }
            Get::Pending(token) => {
                pump.state.observer.counters.pending += 1;
                pump.outstanding += 1;
                pump.state.observer.counters.pending_max = pump
                    .state
                    .observer
                    .counters
                    .pending_max
                    .max(pump.outstanding);
                pump.waiting[slot] = Some(Waiting {
                    token,
                    page,
                    index: u32::try_from(pump.submitted).expect("index"),
                    started,
                });
            }
            Get::Busy => {
                pump.state.observer.counters.busy += 1;
                break;
            }
        }
        pump.submitted += 1;
    }
    Ok(())
}

fn pump_ready<const TRACE: bool>(pump: &mut Pump<'_, TRACE>) -> Result<(), String> {
    let reader = pump.state.reader.as_ref().expect("Dios reader");
    for entry in &mut pump.waiting {
        let Some(waiting) = entry.take() else {
            continue;
        };
        pump.state.observer.counters.ready_checks += 1;
        match pump.pool.ready(reader, waiting.token) {
            ReadyResult::NotYet(token) => *entry = Some(Waiting { token, ..waiting }),
            ReadyResult::Err(failure) => return Err(error(failure)),
            ReadyResult::Ready(guard) => {
                let bytes = &guard[..pump.lane.useful_bytes() as usize];
                let sum = black_box(consume(black_box(bytes)));
                if pump.lane.dependent() {
                    pump.current = next_page(bytes);
                }
                pump.state
                    .observer
                    .finish(waiting.index, waiting.page, waiting.started, sum);
                pump.state.observer.counters.completed_pending += 1;
                pump.outstanding -= 1;
            }
        }
    }
    Ok(())
}

fn next_page(bytes: &[u8]) -> PageNumber {
    let next = u64::from_le_bytes(bytes[..8].try_into().expect("page header"));
    PageNumber(u32::try_from(next).expect("valid next-page header"))
}

fn pump_prefetch<const TRACE: bool>(pump: &mut Pump<'_, TRACE>) {
    if !pump.arm.explicit_hints() {
        return;
    }
    if pump.submitted < pump.hint_next || pump.submitted == pump.state.requests.len() {
        return;
    }
    let count = (pump.state.requests.len() - pump.submitted).min(WINDOW);
    let mut pages = [PageId::new(pump.file, 0); WINDOW];
    for (offset, page) in pages.iter_mut().take(count).enumerate() {
        let index = pump.submitted + offset;
        let number = if pump.arm == Arm::DiosWrongHints {
            4096 + u32::try_from(index).expect("hint index")
        } else {
            pump.state.requests[index].0
        };
        *page = PageId::new(pump.file, number);
    }
    let report = pump.pool.prefetch(&pages[..count]);
    assert_eq!(report.rejected, 0);
    pump.state.observer.counters.prefetch_calls += 1;
    pump.state.observer.counters.prefetch_deferred += report.deferred;
    pump.hint_next = if report.deferred > 0 {
        pump.submitted
    } else if matches!(pump.arm, Arm::DiosPrefetchWhole | Arm::DiosWrongHints) {
        pump.submitted + count
    } else {
        pump.submitted + 1
    };
    pump.state.observer.flight(pump.pool);
    if pump.arm == Arm::DiosWrongHints {
        pump_poll(pump);
    }
}

fn pump_poll<const TRACE: bool>(pump: &mut Pump<'_, TRACE>) {
    let report = pump.pool.poll_report(&mut pump.state.completions);
    pump.state.observer.counters.polls += 1;
    pump.state.observer.counters.reclaimed_lower_bound += u64::from(report.reclaimed_frames());
    assert_eq!(pump.state.completions.iter().count(), 0);
    pump.state.observer.flight(pump.pool);
}

fn pump_drain<const TRACE: bool>(pump: &mut Pump<'_, TRACE>) -> Result<(), String> {
    for _ in 0..POLLS_MAX {
        pump_poll(pump);
        if pump.pool.prefetch_stats().reads_in_flight == 0 {
            return Ok(());
        }
    }
    Err("accepted reads failed to drain within the fixed bound".to_owned())
}
