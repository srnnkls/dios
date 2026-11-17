use std::hint::black_box;
use std::time::Instant;

use dios::{
    FileId, FrameGuard, Get, PageId, Pool, ReaderCtx, ResidentFileLease, ResidentHint,
    RetainedFrame,
};

use super::catalog::{Arm, Lane, PageNumber, consume};
use super::engine::{Source, Work, WorkerResult};
use super::fixture::error;
use super::observe::{Observer, allocations_begin, allocations_end, nanoseconds};
use super::os;

#[derive(Default)]
struct Session<'a> {
    lease: Option<ResidentFileLease>,
    hints: Vec<ResidentHint>,
    retained: Vec<RetainedFrame<'a>>,
}

impl Session<'_> {
    fn storage_bytes(&self) -> u64 {
        u64::try_from(
            self.hints.capacity() * size_of::<ResidentHint>()
                + self.retained.capacity() * size_of::<RetainedFrame<'_>>(),
        )
        .expect("bounded setup storage")
    }
}

pub(super) fn measure<const TRACE: bool>(work: Work<'_>) -> Result<WorkerResult, String> {
    os::pin(0).map_err(error)?;
    assert_eq!(work.lane.workers(), 1);
    let reader = match work.source {
        Source::Mmap(_) => None,
        Source::Dios(pool, _) => Some(pool.register_reader().map_err(error)?),
    };
    let requests: Vec<_> = (0..work.lane.operations())
        .map(|index| work.lane.page(work.seed, index))
        .collect();
    let mut observer = Observer::<TRACE>::new(work.lane.operations());
    let setup_started = Instant::now();
    let session = prepare(work, reader.as_ref())?;
    let setup_ns = nanoseconds(setup_started.elapsed());
    let setup_storage_bytes = session.storage_bytes();
    let retained_payload_bytes =
        u64::try_from(session.retained.len()).expect("retained bound") * 4096;
    let usage_before = os::usage().map_err(error)?;
    let cpu_before = os::cpu_ns().map_err(error)?;
    let origin = Instant::now();
    observer.origin = origin;
    allocations_begin();
    let result = timed_workload(work, reader.as_ref(), &session, &requests, &mut observer);
    let allocations = allocations_end();
    let elapsed_ns = nanoseconds(origin.elapsed());
    let cpu_ns = os::cpu_ns().map_err(error)? - cpu_before;
    let usage = os::usage().map_err(error)?.since(usage_before);
    result?;
    observer.counters.allocations = allocations;
    assert_eq!(observer.counters.operations, work.lane.operations());
    let teardown_started = Instant::now();
    drop(session);
    let teardown_ns = nanoseconds(teardown_started.elapsed());
    Ok(WorkerResult {
        worker: 0,
        elapsed_ns,
        finished_ns: elapsed_ns,
        cpu_ns,
        setup_ns,
        teardown_ns,
        retained_payload_bytes,
        setup_storage_bytes,
        counters: observer.counters,
        usage,
        events: observer.events,
        flights: observer.flights,
    })
}

fn prepare<'a>(work: Work<'a>, reader: Option<&'a ReaderCtx>) -> Result<Session<'a>, String> {
    let mut session = Session::default();
    let Source::Dios(pool, file) = work.source else {
        return Ok(session);
    };
    match work.arm {
        Arm::DiosHinted => {
            let lease = pool
                .lease_file(file)
                .map_err(|failure| format!("resident lease failed: {failure:?}"))?;
            session.hints = Vec::with_capacity(work.lane.warm_pages() as usize);
            for page in 0..work.lane.warm_pages() {
                session.hints.push(
                    pool.resident_hint(&lease, PageId::new(file, page))
                        .ok_or("resident hint setup missed")?,
                );
            }
            session.lease = Some(lease);
        }
        Arm::DiosRetained => {
            let reader = reader.expect("Dios reader");
            session.retained = Vec::with_capacity(work.lane.warm_pages() as usize);
            for page in 0..work.lane.warm_pages() {
                let guard = hit(pool, reader, file, PageNumber(page))?;
                session
                    .retained
                    .push(guard.into_retained().map_err(|refusal| {
                        format!("resident promotion refused: {:?}", refusal.reason)
                    })?);
            }
        }
        _ => {}
    }
    assert!(session.retained.len() <= work.lane.warm_pages() as usize);
    Ok(session)
}

#[inline(never)]
fn timed_workload<const TRACE: bool>(
    work: Work<'_>,
    reader: Option<&ReaderCtx>,
    session: &Session<'_>,
    requests: &[PageNumber],
    observer: &mut Observer<TRACE>,
) -> Result<(), String> {
    match work.source {
        Source::Mmap(mapping) => {
            for (index, page) in requests.iter().enumerate() {
                let started = observer.start();
                finish(
                    observer,
                    work.lane,
                    index,
                    *page,
                    mapping.page(black_box(*page)),
                    started,
                );
            }
        }
        Source::Dios(pool, file) => {
            let reader = reader.expect("Dios reader");
            if matches!(work.arm, Arm::DiosEpochBatch | Arm::DiosPageBatch) {
                batches(work, reader, requests, observer)?;
            } else {
                for (index, page) in requests.iter().enumerate() {
                    let started = observer.start();
                    if work.arm == Arm::DiosRetained {
                        observer.counters.retained_reads += 1;
                        finish(
                            observer,
                            work.lane,
                            index,
                            *page,
                            &session.retained[page.0 as usize],
                            started,
                        );
                    } else {
                        let guard = acquire(pool, reader, file, *page, session)?;
                        observer.counters.hits += 1;
                        observer.counters.guard_acquisitions += 1;
                        finish(observer, work.lane, index, *page, &guard, started);
                    }
                }
            }
        }
    }
    Ok(())
}

fn hit<'a>(
    pool: &'a Pool,
    reader: &'a ReaderCtx,
    file: FileId,
    page: PageNumber,
) -> Result<FrameGuard<'a>, String> {
    resident_guard(pool.get(reader, PageId::new(file, page.0)).map_err(error)?)
}

fn acquire<'a>(
    pool: &'a Pool,
    reader: &'a ReaderCtx,
    file: FileId,
    page: PageNumber,
    session: &Session<'_>,
) -> Result<FrameGuard<'a>, String> {
    if let Some(lease) = &session.lease {
        resident_guard(
            pool.get_with_hint(
                reader,
                lease,
                PageId::new(file, page.0),
                Some(session.hints[page.0 as usize]),
            )
            .map_err(error)?,
        )
    } else {
        hit(pool, reader, file, page)
    }
}

fn resident_guard(result: Get<'_>) -> Result<FrameGuard<'_>, String> {
    match result {
        Get::Hit(guard) => Ok(guard),
        Get::Pending(_) => Err("resident control incurred a miss".to_owned()),
        Get::Busy => Err("resident control was Busy".to_owned()),
    }
}

fn batches<const TRACE: bool>(
    work: Work<'_>,
    reader: &ReaderCtx,
    requests: &[PageNumber],
    observer: &mut Observer<TRACE>,
) -> Result<(), String> {
    let Source::Dios(pool, file) = work.source else {
        unreachable!("Dios batch");
    };
    assert!(requests.len().is_multiple_of(16));
    for (batch, pages) in requests.chunks_exact(16).enumerate() {
        let started = observer.start();
        let first = hit(pool, reader, file, pages[0])?;
        observer.counters.guard_acquisitions += 1;
        observer.counters.hits += 1;
        finish(observer, work.lane, batch * 16, pages[0], &first, started);
        for (offset, page) in pages.iter().enumerate().skip(1) {
            let started = observer.start();
            observer.counters.hits += 1;
            if work.arm == Arm::DiosPageBatch {
                assert_eq!(*page, pages[0]);
                finish(
                    observer,
                    work.lane,
                    batch * 16 + offset,
                    *page,
                    &first,
                    started,
                );
            } else {
                let guard = hit(pool, reader, file, *page)?;
                observer.counters.guard_acquisitions += 1;
                finish(
                    observer,
                    work.lane,
                    batch * 16 + offset,
                    *page,
                    &guard,
                    started,
                );
            }
        }
        drop(first);
    }
    Ok(())
}

fn finish<const TRACE: bool>(
    observer: &mut Observer<TRACE>,
    lane: Lane,
    index: usize,
    page: PageNumber,
    bytes: &[u8],
    started: u64,
) {
    let index = u32::try_from(index).expect("bounded request index");
    let offset = lane.word_offset(index) as usize * 8;
    let sum = black_box(consume(black_box(&bytes[offset..offset + 64])));
    observer.finish(index, page, started, sum);
}
