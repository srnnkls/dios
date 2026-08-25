#[cfg(pfr_product_retention)]
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
#[cfg(pfr_product_retention)]
use std::sync::Arc;
#[cfg(pfr_product_retention)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(pfr_product_retention)]
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
#[cfg(pfr_product_retention)]
use std::thread;
#[cfg(pfr_product_retention)]
use std::time::Duration;
use std::time::Instant;

use dios::testing::PoolTestingExt as _;
use dios::{DirectIo, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

use crate::common::{
    FaultDelta, GRANULE_BYTES, Lane, Measurement, RetentionEvidence, ThreadEvidence,
    descriptor_offset, display_error, fold_bytes,
};
use crate::platform;

const POLLS_MAX: u32 = 1_000_000;
const TRANSIENT_PAGES: u32 = 128;
#[cfg(pfr_product_retention)]
const POLL_BATCH_FRAMES: u32 = 64;
const ZERO_WARM_HITS: u32 = 96;
const ZERO_MISSES: u32 = 16;
const ZERO_PAGE_COUNT: u32 = 144;
#[cfg(pfr_product_retention)]
const WAKE_PERIOD: u64 = 64;
#[cfg(pfr_product_retention)]
const SAME_FRAME_CPUS: [u32; 8] = [0, 1, 2, 3, 32, 33, 34, 35];
#[cfg(pfr_product_retention)]
const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(pfr_product_retention)]
const SAMPLE_TIMEOUT: Duration = Duration::from_mins(1);
#[cfg(pfr_product_retention)]
const POLL_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(pfr_product_retention)]
const WAKE_ACK_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) fn measure(lane: Lane, arm: &str, input: &Path) -> Result<Measurement, String> {
    lane.validate_arm(arm)?;
    match lane {
        Lane::TransientGuard => transient_guard(input),
        Lane::NonzeroPoll => retention_measure(|| nonzero_poll(arm, input)),
        Lane::ZeroBudgetBypass => zero_budget_bypass(input),
        Lane::PromoteReleaseWake => retention_measure(|| promote_release_wake(arm, input)),
        Lane::SameFramePromotion => retention_measure(|| same_frame_promotion(arm, input)),
    }
}

fn retention_measure(
    operation: impl FnOnce() -> Result<Measurement, String>,
) -> Result<Measurement, String> {
    #[cfg(pfr_product_retention)]
    {
        operation()
    }
    #[cfg(not(pfr_product_retention))]
    {
        let _ = operation;
        Err("this product executable has no retention implementation".to_owned())
    }
}

fn build_pool(
    frames: u32,
    readers: u32,
    peak_guards: u32,
    inflight_reads: u32,
    retention_budget: u32,
) -> Result<Pool, String> {
    let builder = Pool::builder()
        .frame_count(frames)
        .granule(GRANULE_BYTES)
        .max_concurrent_readers(readers)
        .peak_guards_per_reader(peak_guards)
        .max_inflight_reads(inflight_reads)
        .miss_headroom(inflight_reads.saturating_mul(3));
    #[cfg(pfr_product_retention)]
    let builder = builder.max_retained_frames(retention_budget);
    #[cfg(not(pfr_product_retention))]
    if retention_budget != 0 {
        return Err("baseline product cannot configure retention".to_owned());
    }
    builder
        .build()
        .map_err(|error| format!("build shipping pool: {error}"))
}

fn open_input(pool: &Pool, input: &Path) -> Result<dios::FileId, String> {
    pool.open(input, DirectIo::Required)
        .map_err(|error| format!("open direct-I/O input: {error}"))
}

fn resolve_page<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> Result<dios::FrameGuard<'pool>, String> {
    match pool.get(reader, page).map_err(|error| error.to_string())? {
        Get::Hit(guard) => Ok(guard),
        Get::Pending(token) => resolve_pending(pool, reader, token, &mut 0),
        Get::Busy => Err("shipping pool unexpectedly remained Busy".to_owned()),
    }
}

fn resolve_page_counted<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    page: PageId,
    completions: &mut u64,
) -> Result<dios::FrameGuard<'pool>, String> {
    match pool.get(reader, page).map_err(|error| error.to_string())? {
        Get::Hit(guard) => Ok(guard),
        Get::Pending(token) => resolve_pending(pool, reader, token, completions),
        Get::Busy => Err("shipping pool unexpectedly remained Busy".to_owned()),
    }
}

fn resolve_pending<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    mut token: PendingToken,
    completions: &mut u64,
) -> Result<dios::FrameGuard<'pool>, String> {
    let mut batch = dios::PoolCompletionBatch::with_capacity(0);
    for _ in 0..POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return Ok(guard),
            ReadyResult::NotYet(returned) => token = returned,
            ReadyResult::Err(error) => return Err(format!("shipping read failed: {error}")),
        }
        let report = pool.poll_report(&mut batch);
        *completions += u64::from(report.backend_completions());
    }
    Err("shipping read exceeded its fixed poll bound".to_owned())
}

fn warm_pages(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    count: u32,
) -> Result<usize, String> {
    let mut base = 0_usize;
    for page_index in 0..count {
        let guard = resolve_page(pool, reader, PageId::new(file, page_index))?;
        if page_index == 0 {
            base = guard.as_ptr() as usize;
        }
        let _ = fold_bytes(&guard, 0);
    }
    if base == 0 {
        return Err("warmup did not expose the frame-arena base".to_owned());
    }
    Ok(base)
}

fn transient_guard(input: &Path) -> Result<Measurement, String> {
    let pool = build_pool(256, 1, 1, 1, 0)?;
    let file = open_input(&pool, input)?;
    let reader = pool.register_reader().map_err(display_error)?;
    let arena_base = warm_pages(&pool, &reader, file, TRANSIENT_PAGES)?;
    let schedule = shuffled_pages(TRANSIENT_PAGES);
    crate::allocation_window_start();
    let warm_started = Instant::now();
    std::hint::black_box(transient_operations(&pool, &reader, file, &schedule)?);
    std::hint::black_box(elapsed_ns(warm_started)?);
    std::hint::black_box(crate::allocation_window_stop());
    let before = platform::thread_faults()?;
    crate::allocation_window_start();
    let started = Instant::now();
    let checksum = transient_operations(&pool, &reader, file, &schedule)?;
    let elapsed_ns = elapsed_ns(started)?;
    let allocations = crate::allocation_window_stop();
    let faults = platform::fault_delta(before, platform::thread_faults()?)?;
    let measurement = Measurement {
        iterations: Lane::TransientGuard.iterations(),
        useful_operations: Lane::TransientGuard.iterations(),
        useful_bytes: Lane::TransientGuard.iterations() * 64,
        elapsed_ns,
        checksum,
        allocations,
        threads: single_thread(faults, "0"),
        arena: arena_for(arena_base, 256)?,
        pool_capacity: 256,
        retained_pages: 0,
        retention: retention_stats(&pool),
        reclaimed_frames: 0,
        backend_completions: 0,
        evictions: 0,
        wake_cycles: 0,
        parked_wakes: 0,
        wake_acks: 0,
        ring_drains: 0,
        held_transitions: 0,
    };
    validate_measurement(&measurement)?;
    Ok(measurement)
}

fn transient_operations(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    schedule: &[u32],
) -> Result<u64, String> {
    let mut checksum = 0_u64;
    for ordinal in 0..Lane::TransientGuard.iterations() {
        let index = usize::try_from(ordinal % schedule.len() as u64).map_err(display_error)?;
        let page = PageId::new(file, schedule[index]);
        let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
            return Err("transient binding access was not a warm hit".to_owned());
        };
        checksum = checksum.wrapping_add(fold_bytes(&guard, descriptor_offset(ordinal)));
    }
    Ok(checksum)
}

fn shuffled_pages(count: u32) -> Vec<u32> {
    let mut pages = (0..count).collect::<Vec<_>>();
    let mut state = 0x9e37_79b9_u32;
    for index in (1..pages.len()).rev() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let other = usize::try_from(state).expect("u32 fits the host index") % (index + 1);
        pages.swap(index, other);
    }
    pages
}

#[cfg(not(pfr_product_retention))]
fn nonzero_poll(_arm: &str, _input: &Path) -> Result<Measurement, String> {
    Err("nonzero retention polling is unavailable in the baseline product".to_owned())
}

#[cfg(pfr_product_retention)]
fn nonzero_poll(arm: &str, input: &Path) -> Result<Measurement, String> {
    let budget = if arm == "budget64" { 64 } else { 0 };
    let pool = build_pool(256, 1, 1, 1, budget)?;
    let file = open_input(&pool, input)?;
    let reader = pool.register_reader().map_err(display_error)?;
    let arena_base = warm_pages(&pool, &reader, file, 256)?;
    warm_poll_window(&pool, &reader, file)?;
    let mut checksum = 0_u64;
    let mut elapsed = 0_u64;
    let mut faults = FaultDelta::default();
    let mut backend_completions = 0_u64;
    crate::allocation_count_reset();
    for batch in 0..Lane::NonzeroPoll.iterations() {
        prepare_poll_batch(&pool, &reader, file)?;
        let (reclaimed, timing, delta) = measured_poll(&pool)?;
        if reclaimed != u64::from(POLL_BATCH_FRAMES) {
            return Err(format!(
                "poll batch reclaimed {reclaimed} frames, expected 64"
            ));
        }
        elapsed += timing;
        add_faults(&mut faults, delta);
        checksum = checksum.wrapping_add(reload_poll_batch(
            &pool,
            &reader,
            file,
            batch,
            &mut backend_completions,
        )?);
    }
    let measurement = poll_measurement(
        &pool,
        arena_base,
        checksum,
        elapsed,
        faults,
        backend_completions,
    )?;
    validate_measurement(&measurement)?;
    Ok(measurement)
}

#[cfg(pfr_product_retention)]
fn warm_poll_window(pool: &Pool, reader: &ReaderCtx, file: dios::FileId) -> Result<(), String> {
    prepare_poll_batch(pool, reader, file)?;
    let (reclaimed, elapsed, faults) = measured_poll(pool)?;
    if reclaimed != u64::from(POLL_BATCH_FRAMES) {
        return Err(format!(
            "warm poll reclaimed {reclaimed} frames, expected 64"
        ));
    }
    let mut completions = 0;
    std::hint::black_box((elapsed, faults));
    std::hint::black_box(reload_poll_batch(pool, reader, file, 0, &mut completions)?);
    Ok(())
}

#[cfg(pfr_product_retention)]
fn prepare_poll_batch(pool: &Pool, reader: &ReaderCtx, file: dios::FileId) -> Result<(), String> {
    let blocker = match pool
        .get(reader, PageId::new(file, 255))
        .map_err(display_error)?
    {
        Get::Hit(guard) => guard,
        Get::Pending(_) | Get::Busy => return Err("poll blocker was not resident".to_owned()),
    };
    for page in 0..POLL_BATCH_FRAMES {
        let _ = pool.evict_frame(PageId::new(file, page));
    }
    if pool.poll() != 0 {
        return Err("guarded poll preparation reclaimed before grace".to_owned());
    }
    drop(blocker);
    Ok(())
}

#[cfg(pfr_product_retention)]
fn measured_poll(pool: &Pool) -> Result<(u64, u64, FaultDelta), String> {
    let before = platform::thread_faults()?;
    crate::allocation_window_enable();
    let started = Instant::now();
    let reclaimed = u64::try_from(pool.poll()).map_err(display_error)?;
    let elapsed = elapsed_ns(started)?;
    crate::allocation_window_disable();
    let faults = platform::fault_delta(before, platform::thread_faults()?)?;
    Ok((reclaimed, elapsed, faults))
}

#[cfg(pfr_product_retention)]
fn reload_poll_batch(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    batch: u64,
    completions: &mut u64,
) -> Result<u64, String> {
    let mut checksum = 0_u64;
    for page in 0..POLL_BATCH_FRAMES {
        let guard = resolve_page_counted(pool, reader, PageId::new(file, page), completions)?;
        let ordinal = batch * u64::from(POLL_BATCH_FRAMES) + u64::from(page);
        checksum = checksum.wrapping_add(fold_bytes(&guard, descriptor_offset(ordinal)));
    }
    Ok(checksum)
}

#[cfg(pfr_product_retention)]
fn poll_measurement(
    pool: &Pool,
    arena_base: usize,
    checksum: u64,
    elapsed_ns: u64,
    faults: FaultDelta,
    backend_completions: u64,
) -> Result<Measurement, String> {
    let reclaimed = Lane::NonzeroPoll.iterations() * u64::from(POLL_BATCH_FRAMES);
    Ok(Measurement {
        iterations: Lane::NonzeroPoll.iterations(),
        useful_operations: reclaimed,
        useful_bytes: reclaimed * 64,
        elapsed_ns,
        checksum,
        allocations: crate::allocation_count(),
        threads: single_thread(faults, "0"),
        arena: arena_for(arena_base, 256)?,
        pool_capacity: 256,
        retained_pages: 0,
        retention: retention_stats(pool),
        reclaimed_frames: reclaimed,
        backend_completions,
        evictions: reclaimed,
        wake_cycles: 0,
        parked_wakes: 0,
        wake_acks: 0,
        ring_drains: 0,
        held_transitions: 0,
    })
}

fn zero_budget_bypass(input: &Path) -> Result<Measurement, String> {
    let pool = build_pool(128, 1, 1, 1, 0)?;
    let file = open_input(&pool, input)?;
    let reader = pool.register_reader().map_err(display_error)?;
    let arena_base = warm_pages(&pool, &reader, file, 128)?;
    let lease = pool
        .lease_file(file)
        .map_err(|error| format!("prepare resident lease: {error:?}"))?;
    let mut resident = (0..128).collect::<Vec<_>>();
    let mut absent = (128..ZERO_PAGE_COUNT).collect::<Vec<_>>();
    let mut clock = 0_usize;
    warm_zero_cycle(
        &pool,
        &reader,
        &lease,
        file,
        &mut resident,
        &mut absent,
        &mut clock,
    )?;
    let mut checksum = 0_u64;
    let mut elapsed_ns = 0_u64;
    let mut faults = FaultDelta::default();
    let mut backend = 0_u64;
    crate::allocation_count_reset();
    for cycle in 0..Lane::ZeroBudgetBypass.iterations() {
        normalize_clock(&pool, &reader, file, &resident)?;
        let (cycle_checksum, cycle_elapsed, cycle_faults) = measured_zero_cycle(
            &pool,
            &reader,
            &lease,
            file,
            cycle,
            (&mut resident, &mut absent, &mut clock, &mut backend),
        )?;
        checksum = checksum.wrapping_add(cycle_checksum);
        elapsed_ns += cycle_elapsed;
        add_faults(&mut faults, cycle_faults);
    }
    let allocations = crate::allocation_count();
    let lifecycle = Lane::ZeroBudgetBypass.iterations() * u64::from(ZERO_MISSES);
    let measurement = Measurement {
        iterations: Lane::ZeroBudgetBypass.iterations(),
        useful_operations: Lane::ZeroBudgetBypass.iterations(),
        useful_bytes: Lane::ZeroBudgetBypass.iterations()
            * u64::from(ZERO_WARM_HITS + ZERO_MISSES)
            * 64,
        elapsed_ns,
        checksum,
        allocations,
        threads: single_thread(faults, "0"),
        arena: arena_for(arena_base, 128)?,
        pool_capacity: 128,
        retained_pages: 0,
        retention: retention_stats(&pool),
        reclaimed_frames: lifecycle,
        backend_completions: backend,
        evictions: lifecycle,
        wake_cycles: 0,
        parked_wakes: 0,
        wake_acks: 0,
        ring_drains: 0,
        held_transitions: 0,
    };
    validate_measurement(&measurement)?;
    Ok(measurement)
}

fn warm_zero_cycle(
    pool: &Pool,
    reader: &ReaderCtx,
    lease: &dios::ResidentFileLease,
    file: dios::FileId,
    resident: &mut [u32],
    absent: &mut [u32],
    clock: &mut usize,
) -> Result<(), String> {
    let mut backend = 0_u64;
    normalize_clock(pool, reader, file, resident)?;
    std::hint::black_box(measured_zero_cycle(
        pool,
        reader,
        lease,
        file,
        0,
        (resident, absent, clock, &mut backend),
    )?);
    Ok(())
}

fn normalize_clock(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    resident: &[u32],
) -> Result<(), String> {
    for &page in resident {
        let Get::Hit(guard) = pool
            .get(reader, PageId::new(file, page))
            .map_err(display_error)?
        else {
            return Err("CLOCK normalization found a nonresident page".to_owned());
        };
        drop(guard);
    }
    Ok(())
}

fn measured_zero_cycle(
    pool: &Pool,
    reader: &ReaderCtx,
    lease: &dios::ResidentFileLease,
    file: dios::FileId,
    cycle: u64,
    state: (&mut [u32], &mut [u32], &mut usize, &mut u64),
) -> Result<(u64, u64, FaultDelta), String> {
    let before = platform::thread_faults()?;
    crate::allocation_window_enable();
    let started = Instant::now();
    let checksum = zero_cycle(pool, reader, lease, file, cycle, state)?;
    let elapsed = elapsed_ns(started)?;
    crate::allocation_window_disable();
    let faults = platform::fault_delta(before, platform::thread_faults()?)?;
    Ok((checksum, elapsed, faults))
}

fn zero_cycle(
    pool: &Pool,
    reader: &ReaderCtx,
    lease: &dios::ResidentFileLease,
    file: dios::FileId,
    cycle: u64,
    state: (&mut [u32], &mut [u32], &mut usize, &mut u64),
) -> Result<u64, String> {
    let (resident, absent, clock, backend) = state;
    let mut checksum = zero_warm_hits(pool, reader, file, cycle, resident)?;
    for miss in 0..ZERO_MISSES {
        let absent_index = usize::try_from(miss).map_err(display_error)?;
        let target = absent[absent_index];
        let victim = resident[*clock];
        trigger_clock_eviction(pool, reader, lease, file, target, victim)?;
        reclaim_one(pool)?;
        let guard = resolve_page_counted(pool, reader, PageId::new(file, target), backend)?;
        let ordinal = cycle * u64::from(ZERO_MISSES) + u64::from(miss);
        checksum = checksum.wrapping_add(fold_bytes(&guard, descriptor_offset(ordinal)));
        resident[*clock] = target;
        absent[absent_index] = victim;
        *clock = (*clock + 1) % resident.len();
    }
    Ok(checksum)
}

fn zero_warm_hits(
    pool: &Pool,
    reader: &ReaderCtx,
    file: dios::FileId,
    cycle: u64,
    resident: &[u32],
) -> Result<u64, String> {
    let mut checksum = 0_u64;
    for hit in 0..ZERO_WARM_HITS {
        let index = usize::try_from(hit).map_err(display_error)?;
        let Get::Hit(guard) = pool
            .get(reader, PageId::new(file, resident[index]))
            .map_err(display_error)?
        else {
            return Err("zero-budget warm operation missed".to_owned());
        };
        let ordinal = cycle * u64::from(ZERO_WARM_HITS) + u64::from(hit);
        checksum = checksum.wrapping_add(fold_bytes(&guard, descriptor_offset(ordinal)));
    }
    Ok(checksum)
}

fn trigger_clock_eviction(
    pool: &Pool,
    reader: &ReaderCtx,
    lease: &dios::ResidentFileLease,
    file: dios::FileId,
    target: u32,
    victim: u32,
) -> Result<(), String> {
    match pool
        .get(reader, PageId::new(file, target))
        .map_err(display_error)?
    {
        Get::Busy => {}
        Get::Hit(_) | Get::Pending(_) => {
            return Err("CLOCK trigger did not stop at bounded Busy".to_owned());
        }
    }
    let Some((tagged, global)) = pool.reclamation_epochs_observed() else {
        return Err("CLOCK eviction did not publish an epoch tag".to_owned());
    };
    if global != tagged + 1 {
        return Err(format!(
            "CLOCK trigger left tagged epoch {tagged} at global epoch {global}"
        ));
    }
    if pool
        .resident_hint(lease, PageId::new(file, victim))
        .is_some()
    {
        return Err("CLOCK did not evict the frozen next victim".to_owned());
    }
    Ok(())
}

fn reclaim_one(pool: &Pool) -> Result<(), String> {
    let before = pool.global_epoch_observed();
    let reclaimed = pool.poll();
    let after = pool.global_epoch_observed();
    if after != before + 1 || reclaimed != 1 {
        return Err(format!(
            "lifecycle reclaim advanced epoch {before} to {after} and reclaimed {reclaimed} frames"
        ));
    }
    Ok(())
}

#[cfg(not(pfr_product_retention))]
fn promote_release_wake(_arm: &str, _input: &Path) -> Result<Measurement, String> {
    Err("retained wake measurement is unavailable in the baseline product".to_owned())
}

#[cfg(pfr_product_retention)]
fn promote_release_wake(arm: &str, input: &Path) -> Result<Measurement, String> {
    let pool = Arc::new(build_pool(64, 1, 1, 1, 1)?);
    let file = open_input(&pool, input)?;
    let reader = pool.register_reader().map_err(display_error)?;
    let arena_base = warm_pages(&pool, &reader, file, 64)?;
    let page = PageId::new(file, 0);
    let mut poller = WakePoller::spawn(Arc::clone(&pool))?;
    let samples = wake_samples(&pool, &reader, page, arm, &mut poller);
    let poller_evidence = poller.finish();
    let (checksum, totals, retention_before) = samples?;
    let poller_evidence = poller_evidence?;
    let measurement = wake_measurement(
        &pool,
        arm,
        arena_base,
        checksum,
        &totals,
        &poller_evidence,
        retention_before,
    )?;
    validate_measurement(&measurement)?;
    Ok(measurement)
}

#[cfg(pfr_product_retention)]
fn wake_samples(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    arm: &str,
    poller: &mut WakePoller,
) -> Result<(u64, WindowTotals, RetentionEvidence), String> {
    poller.warm()?;
    warm_wake_cycles(pool, reader, page, arm, poller)?;
    let retention_before = retention_stats(pool);
    let mut totals = WindowTotals::default();
    crate::allocation_count_reset();
    let mut checksum = 0_u64;
    for group in 0..64_u64 {
        checksum = checksum.wrapping_add(wake_regular_cycles(
            pool,
            reader,
            page,
            arm,
            group,
            &mut totals,
        )?);
        checksum = checksum.wrapping_add(wake_special_cycle(
            pool,
            reader,
            page,
            arm,
            group,
            poller,
            &mut totals,
        )?);
        let mut completions = 0;
        drop(resolve_page_counted(pool, reader, page, &mut completions)?);
        totals.backend += completions;
    }
    Ok((checksum, totals, retention_before))
}

#[cfg(pfr_product_retention)]
fn warm_wake_cycles(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    arm: &str,
    poller: &mut WakePoller,
) -> Result<(), String> {
    let mut totals = WindowTotals::default();
    let regular = wake_regular_cycles(pool, reader, page, arm, 0, &mut totals)?;
    let special = wake_special_cycle(pool, reader, page, arm, 0, poller, &mut totals)?;
    let mut completions = 0;
    drop(resolve_page_counted(pool, reader, page, &mut completions)?);
    std::hint::black_box((regular, special, totals, completions));
    Ok(())
}

#[cfg(pfr_product_retention)]
#[derive(Default)]
struct WindowTotals {
    elapsed_ns: u64,
    faults: FaultDelta,
    backend: u64,
}

#[cfg(pfr_product_retention)]
fn wake_regular_cycles(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    arm: &str,
    group: u64,
    totals: &mut WindowTotals,
) -> Result<u64, String> {
    let before = platform::thread_faults()?;
    crate::allocation_window_enable();
    let started = Instant::now();
    let mut checksum = 0_u64;
    for cycle in 0..WAKE_PERIOD - 1 {
        let ordinal = group * WAKE_PERIOD + cycle;
        let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
            return Err("wake regular page was not resident".to_owned());
        };
        if arm == "retained" {
            let retained = guard
                .into_retained()
                .map_err(|_| "regular promotion refused")?;
            checksum = checksum.wrapping_add(fold_bytes(&retained, descriptor_offset(ordinal)));
        } else {
            checksum = checksum.wrapping_add(fold_bytes(&guard, descriptor_offset(ordinal)));
        }
    }
    totals.elapsed_ns += elapsed_ns(started)?;
    crate::allocation_window_disable();
    add_faults(
        &mut totals.faults,
        platform::fault_delta(before, platform::thread_faults()?)?,
    );
    Ok(checksum)
}

#[cfg(pfr_product_retention)]
fn wake_special_cycle(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    arm: &str,
    group: u64,
    poller: &mut WakePoller,
    totals: &mut WindowTotals,
) -> Result<u64, String> {
    if arm == "retained" {
        wake_special_retained(pool, reader, page, group, poller, totals)
    } else {
        wake_special_transient(pool, reader, page, group, poller, totals)
    }
}

#[cfg(pfr_product_retention)]
fn wake_special_retained(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    group: u64,
    poller: &mut WakePoller,
    totals: &mut WindowTotals,
) -> Result<u64, String> {
    let before = platform::thread_faults()?;
    crate::allocation_window_enable();
    let started = Instant::now();
    let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
        return Err("special retained page was not resident".to_owned());
    };
    let retained = guard
        .into_retained()
        .map_err(|_| "special promotion refused")?;
    let checksum = fold_bytes(&retained, descriptor_offset(group * WAKE_PERIOD + 63));
    totals.elapsed_ns += elapsed_ns(started)?;
    crate::allocation_window_disable();
    add_faults(
        &mut totals.faults,
        platform::fault_delta(before, platform::thread_faults()?)?,
    );
    let _ = pool.evict_frame(page);
    if pool.poll() != 0 || pool.poll() != 0 {
        return Err("retained frame reclaimed before its HELD release".to_owned());
    }
    poller.begin_park()?;
    totals.elapsed_ns += measured_retained_release(retained, poller, &mut totals.faults)?;
    Ok(checksum)
}

#[cfg(pfr_product_retention)]
fn measured_retained_release(
    retained: dios::RetainedFrame<'_>,
    poller: &mut WakePoller,
    faults: &mut FaultDelta,
) -> Result<u64, String> {
    let before = platform::thread_faults()?;
    crate::allocation_window_enable();
    let started = Instant::now();
    let acknowledgement_deadline = started + WAKE_ACK_TIMEOUT;
    drop(retained);
    poller.acknowledge(acknowledgement_deadline)?;
    let elapsed = elapsed_ns(started)?;
    crate::allocation_window_disable();
    add_faults(
        faults,
        platform::fault_delta(before, platform::thread_faults()?)?,
    );
    Ok(elapsed)
}

#[cfg(pfr_product_retention)]
fn wake_special_transient(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    group: u64,
    poller: &mut WakePoller,
    totals: &mut WindowTotals,
) -> Result<u64, String> {
    let before = platform::thread_faults()?;
    crate::allocation_window_enable();
    let started = Instant::now();
    let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
        return Err("special transient page was not resident".to_owned());
    };
    let checksum = fold_bytes(&guard, descriptor_offset(group * WAKE_PERIOD + 63));
    totals.elapsed_ns += elapsed_ns(started)?;
    crate::allocation_window_disable();
    add_faults(
        &mut totals.faults,
        platform::fault_delta(before, platform::thread_faults()?)?,
    );
    let _ = pool.evict_frame(page);
    if pool.poll() != 0 || pool.poll() != 0 {
        return Err("live transient guard did not hold its grace period".to_owned());
    }
    poller.begin_park()?;
    totals.elapsed_ns += measured_transient_release(pool, guard, poller, &mut totals.faults)?;
    Ok(checksum)
}

#[cfg(pfr_product_retention)]
fn measured_transient_release(
    pool: &Pool,
    guard: dios::FrameGuard<'_>,
    poller: &mut WakePoller,
    faults: &mut FaultDelta,
) -> Result<u64, String> {
    let before = platform::thread_faults()?;
    crate::allocation_window_enable();
    let started = Instant::now();
    let acknowledgement_deadline = started + WAKE_ACK_TIMEOUT;
    drop(guard);
    pool.wake_handle().wake();
    poller.acknowledge(acknowledgement_deadline)?;
    let elapsed = elapsed_ns(started)?;
    crate::allocation_window_disable();
    add_faults(
        faults,
        platform::fault_delta(before, platform::thread_faults()?)?,
    );
    Ok(elapsed)
}

#[cfg(pfr_product_retention)]
struct PollerEvidence {
    faults: FaultDelta,
    reclaimed: u64,
    acknowledgements: u64,
}

#[cfg(pfr_product_retention)]
struct PollerSample {
    faults: FaultDelta,
    reclaimed: u64,
    completed_at: Instant,
}

#[cfg(pfr_product_retention)]
enum PollerCommand {
    Poll,
    Stop,
}

#[cfg(pfr_product_retention)]
enum PollerEvent {
    Ready(u32),
    Complete(PollerSample),
    Failed(String),
    Stopped,
}

#[cfg(pfr_product_retention)]
struct WakePoller {
    pool: Arc<Pool>,
    commands: SyncSender<PollerCommand>,
    events: Receiver<PollerEvent>,
    handle: Option<thread::JoinHandle<()>>,
    thread_id: u32,
    completed: u64,
    pending: bool,
    terminal: bool,
    evidence: PollerEvidence,
}

#[cfg(pfr_product_retention)]
impl WakePoller {
    fn spawn(pool: Arc<Pool>) -> Result<Self, String> {
        let (commands, command_receiver) = mpsc::sync_channel(1);
        let (event_sender, events) = mpsc::sync_channel(2);
        let handle = spawn_poller(Arc::clone(&pool), command_receiver, event_sender);
        let mut poller = Self {
            pool,
            commands,
            events,
            handle: Some(handle),
            thread_id: 0,
            completed: 0,
            pending: false,
            terminal: false,
            evidence: PollerEvidence {
                faults: FaultDelta::default(),
                reclaimed: 0,
                acknowledgements: 0,
            },
        };
        if let Err(error) = poller.wait_ready() {
            let _ = poller.finish();
            return Err(error);
        }
        Ok(poller)
    }

    fn wait_ready(&mut self) -> Result<(), String> {
        match self.events.recv_timeout(RENDEZVOUS_TIMEOUT) {
            Ok(PollerEvent::Ready(thread_id)) => {
                self.thread_id = thread_id;
                Ok(())
            }
            Ok(PollerEvent::Failed(error)) => {
                self.terminal = true;
                Err(format!("wake poller setup failed: {error}"))
            }
            Ok(PollerEvent::Complete(_) | PollerEvent::Stopped) => {
                Err("wake poller sent an invalid setup result".to_owned())
            }
            Err(error) => Err(format!("wake poller setup rendezvous failed: {error}")),
        }
    }

    fn warm(&mut self) -> Result<(), String> {
        self.start_poll()?;
        self.pool.wake_handle().wake();
        self.receive_complete(RENDEZVOUS_TIMEOUT, false)
    }

    fn begin_park(&mut self) -> Result<(), String> {
        self.start_poll()?;
        if let Err(error) = platform::wait_until_parked(self.thread_id) {
            self.pool.wake_handle().wake();
            let _ = self.receive_complete(POLL_WAIT_TIMEOUT + RENDEZVOUS_TIMEOUT, false);
            return Err(error);
        }
        Ok(())
    }

    fn acknowledge(&mut self, deadline: Instant) -> Result<(), String> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(timeout) {
            Ok(PollerEvent::Complete(sample)) if sample.completed_at <= deadline => {
                self.accept_event(PollerEvent::Complete(sample), true)
            }
            Ok(PollerEvent::Complete(sample)) => {
                self.pool.wake_handle().wake();
                self.accept_event(PollerEvent::Complete(sample), false)?;
                Err("wake poller missed its acknowledgement deadline".to_owned())
            }
            Ok(event) => self.accept_event(event, false),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pool.wake_handle().wake();
                self.receive_complete(POLL_WAIT_TIMEOUT + RENDEZVOUS_TIMEOUT, false)?;
                Err("wake poller missed its acknowledgement deadline".to_owned())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.terminal = true;
                Err("wake poller disconnected before acknowledgement".to_owned())
            }
        }
    }

    fn start_poll(&mut self) -> Result<(), String> {
        if self.pending || self.terminal {
            return Err("wake poller is not ready for another cycle".to_owned());
        }
        self.commands
            .try_send(PollerCommand::Poll)
            .map_err(|error| format!("start wake poller cycle: {error}"))?;
        self.pending = true;
        Ok(())
    }

    fn receive_complete(&mut self, timeout: Duration, acknowledge: bool) -> Result<(), String> {
        let event = self
            .events
            .recv_timeout(timeout)
            .map_err(|error| format!("wake poller cycle rendezvous failed: {error}"))?;
        self.accept_event(event, acknowledge)
    }

    fn accept_event(&mut self, event: PollerEvent, acknowledge: bool) -> Result<(), String> {
        match event {
            PollerEvent::Complete(sample) => {
                self.pending = false;
                let measured = self.completed >= 2;
                if measured {
                    add_faults(&mut self.evidence.faults, sample.faults);
                    self.evidence.reclaimed += sample.reclaimed;
                    self.evidence.acknowledgements += u64::from(acknowledge);
                }
                self.completed += 1;
                Ok(())
            }
            PollerEvent::Failed(error) => {
                self.terminal = true;
                Err(format!("wake poller failed: {error}"))
            }
            PollerEvent::Ready(_) | PollerEvent::Stopped => {
                Err("wake poller sent an invalid cycle result".to_owned())
            }
        }
    }

    fn finish(mut self) -> Result<PollerEvidence, String> {
        self.pool.wake_handle().wake();
        self.request_stop();
        let terminal = self.wait_terminal();
        let joined = self.join();
        terminal?;
        joined?;
        Ok(self.evidence)
    }

    fn request_stop(&self) {
        match self.commands.try_send(PollerCommand::Stop) {
            Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
        }
    }

    fn wait_terminal(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + POLL_WAIT_TIMEOUT + RENDEZVOUS_TIMEOUT;
        while !self.terminal {
            let event = receive_before(&self.events, deadline, "wake poller cleanup")?;
            match event {
                PollerEvent::Complete(sample) => {
                    self.pending = false;
                    self.completed += 1;
                    std::hint::black_box(sample);
                    self.request_stop();
                }
                PollerEvent::Failed(error) => {
                    self.terminal = true;
                    return Err(format!("wake poller failed: {error}"));
                }
                PollerEvent::Stopped => self.terminal = true,
                PollerEvent::Ready(thread_id) => {
                    self.thread_id = thread_id;
                    self.request_stop();
                }
            }
        }
        Ok(())
    }

    fn join(&mut self) -> Result<(), String> {
        let handle = self.handle.take().expect("wake poller joins once");
        handle
            .join()
            .map_err(|_| "wake poller supervisor panicked".to_owned())
    }
}

#[cfg(pfr_product_retention)]
fn spawn_poller(
    pool: Arc<Pool>,
    commands: Receiver<PollerCommand>,
    events: SyncSender<PollerEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            poller_worker(&pool, &commands, &events)
        }));
        let terminal = match outcome {
            Ok(Ok(())) => PollerEvent::Stopped,
            Ok(Err(error)) => PollerEvent::Failed(error),
            Err(_) => PollerEvent::Failed("wake poller panicked".to_owned()),
        };
        let _ = events.send(terminal);
    })
}

#[cfg(pfr_product_retention)]
fn poller_worker(
    pool: &Pool,
    commands: &Receiver<PollerCommand>,
    events: &SyncSender<PollerEvent>,
) -> Result<(), String> {
    platform::pin_current(1)?;
    let thread_id = platform::current_thread_id()?;
    events
        .send(PollerEvent::Ready(thread_id))
        .map_err(display_error)?;
    let mut batch = dios::PoolCompletionBatch::with_capacity(0);
    loop {
        match commands.recv().map_err(display_error)? {
            PollerCommand::Poll => {
                let before = platform::thread_faults()?;
                let report = pool.poll_wait(&mut batch, POLL_WAIT_TIMEOUT);
                let faults = platform::fault_delta(before, platform::thread_faults()?)?;
                events
                    .send(PollerEvent::Complete(PollerSample {
                        faults,
                        reclaimed: u64::from(report.reclaimed_frames()),
                        completed_at: Instant::now(),
                    }))
                    .map_err(display_error)?;
            }
            PollerCommand::Stop => return Ok(()),
        }
    }
}

#[cfg(pfr_product_retention)]
fn receive_before<T>(
    receiver: &Receiver<T>,
    deadline: Instant,
    context: &str,
) -> Result<T, String> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| format!("{context} timed out"))?;
    receiver
        .recv_timeout(timeout)
        .map_err(|error| format!("{context} failed: {error}"))
}

#[cfg(pfr_product_retention)]
fn wake_measurement(
    pool: &Pool,
    arm: &str,
    arena_base: usize,
    checksum: u64,
    totals: &WindowTotals,
    poller: &PollerEvidence,
    retention_before: RetentionEvidence,
) -> Result<Measurement, String> {
    let retained = u64::from(arm == "retained");
    let wake_count = Lane::PromoteReleaseWake.iterations() / WAKE_PERIOD;
    Ok(Measurement {
        iterations: Lane::PromoteReleaseWake.iterations(),
        useful_operations: Lane::PromoteReleaseWake.iterations(),
        useful_bytes: Lane::PromoteReleaseWake.iterations() * 64,
        elapsed_ns: totals.elapsed_ns,
        checksum,
        allocations: crate::allocation_count(),
        threads: ThreadEvidence {
            affinities: "0;1".to_owned(),
            faults: vec![totals.faults, poller.faults],
        },
        arena: arena_for(arena_base, 64)?,
        pool_capacity: 64,
        retained_pages: u32::try_from(retained).expect("retained arm flag fits u32"),
        retention: wake_retention_delta(retention_before, retention_stats(pool))?,
        reclaimed_frames: poller.reclaimed,
        backend_completions: totals.backend,
        evictions: wake_count,
        wake_cycles: wake_count,
        parked_wakes: wake_count,
        wake_acks: poller.acknowledgements,
        ring_drains: wake_count * retained,
        held_transitions: wake_count * retained,
    })
}

#[cfg(pfr_product_retention)]
fn wake_retention_delta(
    before: RetentionEvidence,
    after: RetentionEvidence,
) -> Result<RetentionEvidence, String> {
    if before.occupied_budget != 0 || after.occupied_budget != 0 {
        return Err("wake warmup or samples leaked retention budget".to_owned());
    }
    Ok(RetentionEvidence {
        occupied_budget: after.occupied_budget,
        refused_budget: after.refused_budget - before.refused_budget,
        refused_ceiling: after.refused_ceiling - before.refused_ceiling,
        refused_contention: after.refused_contention - before.refused_contention,
        refused_retiring: after.refused_retiring - before.refused_retiring,
        retained_evictions_held: after.retained_evictions_held - before.retained_evictions_held,
    })
}

#[cfg(not(pfr_product_retention))]
fn same_frame_promotion(_arm: &str, _input: &Path) -> Result<Measurement, String> {
    Err("same-frame promotion is unavailable in the baseline product".to_owned())
}

#[cfg(pfr_product_retention)]
fn same_frame_promotion(arm: &str, input: &Path) -> Result<Measurement, String> {
    let pool = Arc::new(build_pool(20, 8, 2, 1, 1)?);
    let file = open_input(&pool, input)?;
    let setup_reader = pool.register_reader().map_err(display_error)?;
    let arena_base = warm_pages(&pool, &setup_reader, file, 1)?;
    drop(setup_reader);
    let page = PageId::new(file, 0);
    let worker_count = if arm == "eight_workers" { 8 } else { 1 };
    let results = Arc::new(WorkerResults::new(worker_count)?);
    let mut workers = PromotionWorkers::spawn(&pool, page, &results, worker_count)?;
    let sample = promotion_sample(&pool, &mut workers);
    let stopped = workers.stop_and_join();
    let (elapsed_ns, allocations, stats) = sample?;
    stopped?;
    let measurement =
        promotion_measurement(arm, arena_base, elapsed_ns, allocations, &results, stats)?;
    validate_measurement(&measurement)?;
    Ok(measurement)
}

#[cfg(pfr_product_retention)]
fn promotion_sample(
    pool: &Pool,
    workers: &mut PromotionWorkers,
) -> Result<(u64, u64, [RetentionEvidence; 3]), String> {
    workers.phase(
        PromotionCommand::Warm,
        PromotionPhase::Warm,
        RENDEZVOUS_TIMEOUT,
    )?;
    let before = retention_stats(pool);
    crate::allocation_window_start();
    let started = Instant::now();
    let sample = workers.phase(
        PromotionCommand::Sample,
        PromotionPhase::Sample,
        SAMPLE_TIMEOUT,
    );
    let elapsed = elapsed_ns(started);
    let allocations = crate::allocation_window_stop();
    sample?;
    let elapsed = elapsed?;
    let sampled = retention_stats(pool);
    workers.phase(
        PromotionCommand::Release,
        PromotionPhase::Release,
        RENDEZVOUS_TIMEOUT,
    )?;
    let final_stats = retention_stats(pool);
    Ok((elapsed, allocations, [before, sampled, final_stats]))
}

#[cfg(pfr_product_retention)]
struct WorkerResults {
    checksums: Box<[AtomicU64]>,
    refusals: Box<[AtomicU64]>,
    minor_faults: Box<[AtomicU64]>,
    major_faults: Box<[AtomicU64]>,
}

#[cfg(pfr_product_retention)]
impl WorkerResults {
    fn new(count: usize) -> Result<Self, String> {
        if count == 0 || count > SAME_FRAME_CPUS.len() {
            return Err("same-frame worker count must be 1..=8".to_owned());
        }
        Ok(Self {
            checksums: (0..count).map(|_| AtomicU64::new(0)).collect(),
            refusals: (0..count).map(|_| AtomicU64::new(0)).collect(),
            minor_faults: (0..count).map(|_| AtomicU64::new(0)).collect(),
            major_faults: (0..count).map(|_| AtomicU64::new(0)).collect(),
        })
    }
}

#[cfg(pfr_product_retention)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromotionPhase {
    Setup,
    Warm,
    Sample,
    Release,
}

#[cfg(pfr_product_retention)]
#[derive(Clone, Copy)]
enum PromotionCommand {
    Warm,
    Sample,
    Release,
    Stop,
}

#[cfg(pfr_product_retention)]
enum PromotionEvent {
    Complete {
        worker: usize,
        phase: PromotionPhase,
    },
    Failed {
        worker: usize,
        error: String,
    },
    Stopped {
        worker: usize,
    },
}

#[cfg(pfr_product_retention)]
struct PromotionWorkers {
    commands: Vec<SyncSender<PromotionCommand>>,
    events: Receiver<PromotionEvent>,
    handles: Vec<thread::JoinHandle<()>>,
    terminal: u16,
}

#[cfg(pfr_product_retention)]
impl PromotionWorkers {
    fn spawn(
        pool: &Arc<Pool>,
        page: PageId,
        results: &Arc<WorkerResults>,
        count: usize,
    ) -> Result<Self, String> {
        let capacity = count
            .checked_mul(2)
            .ok_or_else(|| "promotion event capacity overflowed".to_owned())?;
        let (event_sender, events) = mpsc::sync_channel(capacity);
        let mut workers = Self {
            commands: Vec::with_capacity(count),
            events,
            handles: Vec::with_capacity(count),
            terminal: 0,
        };
        for worker in 0..count {
            let (command_sender, command_receiver) = mpsc::sync_channel(1);
            let handle = spawn_promotion_worker(
                Arc::clone(pool),
                page,
                worker,
                command_receiver,
                Arc::clone(results),
                event_sender.clone(),
            );
            workers.commands.push(command_sender);
            match handle {
                Ok(handle) => workers.handles.push(handle),
                Err(error) => {
                    let _ = workers.stop_and_join();
                    return Err(error);
                }
            }
        }
        drop(event_sender);
        if let Err(error) = workers.await_phase(PromotionPhase::Setup, RENDEZVOUS_TIMEOUT) {
            let _ = workers.stop_and_join();
            return Err(error);
        }
        Ok(workers)
    }

    fn phase(
        &mut self,
        command: PromotionCommand,
        phase: PromotionPhase,
        timeout: Duration,
    ) -> Result<(), String> {
        for (worker, sender) in self.commands.iter().enumerate() {
            sender
                .try_send(command)
                .map_err(|error| format!("start promotion worker {worker} {phase:?}: {error}"))?;
        }
        self.await_phase(phase, timeout)
    }

    fn await_phase(&mut self, phase: PromotionPhase, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let count = u32::try_from(self.commands.len()).expect("at most eight workers fit u32");
        let mut seen = 0_u16;
        while seen.count_ones() < count {
            match receive_before(&self.events, deadline, "promotion worker rendezvous")? {
                PromotionEvent::Complete {
                    worker,
                    phase: observed,
                } if observed == phase => mark_worker(&mut seen, worker, self.commands.len())?,
                PromotionEvent::Complete {
                    worker,
                    phase: observed,
                } => {
                    return Err(format!(
                        "promotion worker {worker} completed {observed:?} during {phase:?} rendezvous"
                    ));
                }
                PromotionEvent::Failed { worker, error } => {
                    mark_worker(&mut self.terminal, worker, self.commands.len())?;
                    return Err(format!("promotion worker {worker} failed: {error}"));
                }
                PromotionEvent::Stopped { worker } => {
                    mark_worker(&mut self.terminal, worker, self.commands.len())?;
                    return Err(format!(
                        "promotion worker {worker} stopped during {phase:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn stop_and_join(mut self) -> Result<(), String> {
        self.request_stops();
        let terminal = self.await_terminal();
        let joined = self.join_all();
        terminal?;
        joined
    }

    fn request_stops(&self) {
        for (worker, sender) in self.commands.iter().enumerate() {
            if worker_bit(self.terminal, worker) {
                continue;
            }
            match sender.try_send(PromotionCommand::Stop) {
                Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
            }
        }
    }

    fn await_terminal(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + SAMPLE_TIMEOUT + RENDEZVOUS_TIMEOUT;
        let count = u32::try_from(self.commands.len()).expect("at most eight workers fit u32");
        let mut first_error = None;
        while self.terminal.count_ones() < count {
            match receive_before(&self.events, deadline, "promotion worker cleanup")? {
                PromotionEvent::Complete { worker, .. } => {
                    let _ = self.commands[worker].try_send(PromotionCommand::Stop);
                }
                PromotionEvent::Failed { worker, error } => {
                    mark_worker(&mut self.terminal, worker, self.commands.len())?;
                    first_error.get_or_insert_with(|| {
                        format!("promotion worker {worker} failed: {error}")
                    });
                }
                PromotionEvent::Stopped { worker } => {
                    mark_worker(&mut self.terminal, worker, self.commands.len())?;
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn join_all(&mut self) -> Result<(), String> {
        for handle in self.handles.drain(..) {
            handle
                .join()
                .map_err(|_| "promotion worker supervisor panicked".to_owned())?;
        }
        Ok(())
    }
}

#[cfg(pfr_product_retention)]
fn mark_worker(bits: &mut u16, worker: usize, count: usize) -> Result<(), String> {
    if worker >= count {
        return Err(format!("promotion event names unknown worker {worker}"));
    }
    let bit = 1_u16 << worker;
    if *bits & bit != 0 {
        return Err(format!("promotion worker {worker} reported twice"));
    }
    *bits |= bit;
    Ok(())
}

#[cfg(pfr_product_retention)]
fn worker_bit(bits: u16, worker: usize) -> bool {
    bits & (1_u16 << worker) != 0
}

#[cfg(pfr_product_retention)]
fn spawn_promotion_worker(
    pool: Arc<Pool>,
    page: PageId,
    worker: usize,
    commands: Receiver<PromotionCommand>,
    results: Arc<WorkerResults>,
    events: SyncSender<PromotionEvent>,
) -> Result<thread::JoinHandle<()>, String> {
    thread::Builder::new()
        .name(format!("pfr-promotion-{worker}"))
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                promotion_worker(&pool, page, worker, &commands, &results, &events)
            }));
            let terminal = match outcome {
                Ok(Ok(())) => PromotionEvent::Stopped { worker },
                Ok(Err(error)) => PromotionEvent::Failed { worker, error },
                Err(_) => PromotionEvent::Failed {
                    worker,
                    error: "worker panicked".to_owned(),
                },
            };
            let _ = events.send(terminal);
        })
        .map_err(display_error)
}

#[cfg(pfr_product_retention)]
fn promotion_worker(
    pool: &Pool,
    page: PageId,
    worker: usize,
    commands: &Receiver<PromotionCommand>,
    results: &WorkerResults,
    events: &SyncSender<PromotionEvent>,
) -> Result<(), String> {
    platform::pin_current(SAME_FRAME_CPUS[worker])?;
    let reader = pool.register_reader().map_err(display_error)?;
    let mut anchor = if worker == 0 {
        Some(retain_hit(pool, &reader, page)?)
    } else {
        None
    };
    send_promotion_complete(events, worker, PromotionPhase::Setup)?;
    loop {
        match commands.recv().map_err(display_error)? {
            PromotionCommand::Warm => {
                warm_promotion(pool, &reader, page)?;
                send_promotion_complete(events, worker, PromotionPhase::Warm)?;
            }
            PromotionCommand::Sample => {
                let before = platform::thread_faults()?;
                promotion_attempts(pool, &reader, page, worker, results)?;
                let faults = platform::fault_delta(before, platform::thread_faults()?)?;
                results.minor_faults[worker].store(faults.minor, Ordering::Relaxed);
                results.major_faults[worker].store(faults.major, Ordering::Relaxed);
                send_promotion_complete(events, worker, PromotionPhase::Sample)?;
            }
            PromotionCommand::Release => {
                drop(anchor.take());
                send_promotion_complete(events, worker, PromotionPhase::Release)?;
            }
            PromotionCommand::Stop => return Ok(()),
        }
    }
}

#[cfg(pfr_product_retention)]
fn send_promotion_complete(
    events: &SyncSender<PromotionEvent>,
    worker: usize,
    phase: PromotionPhase,
) -> Result<(), String> {
    events
        .send(PromotionEvent::Complete { worker, phase })
        .map_err(display_error)
}

#[cfg(pfr_product_retention)]
fn retain_hit<'pool>(
    pool: &'pool Pool,
    reader: &'pool ReaderCtx,
    page: PageId,
) -> Result<dios::RetainedFrame<'pool>, String> {
    let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
        return Err("same-frame anchor page was not resident".to_owned());
    };
    guard
        .into_retained()
        .map_err(|_| "same-frame setup anchor was refused".to_owned())
}

#[cfg(pfr_product_retention)]
fn warm_promotion(pool: &Pool, reader: &ReaderCtx, page: PageId) -> Result<(), String> {
    let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
        return Err("same-frame warmup page was not resident".to_owned());
    };
    match guard.into_retained() {
        Ok(retained) => drop(retained),
        Err(refused) => drop(refused.guard),
    }
    Ok(())
}

#[cfg(pfr_product_retention)]
fn promotion_attempts(
    pool: &Pool,
    reader: &ReaderCtx,
    page: PageId,
    worker: usize,
    results: &WorkerResults,
) -> Result<(), String> {
    let mut checksum = 0_u64;
    let mut refusals = 0_u64;
    for attempt in 0..Lane::SameFramePromotion.iterations() {
        let Get::Hit(guard) = pool.get(reader, page).map_err(display_error)? else {
            return Err("same-frame sampled access was not a hit".to_owned());
        };
        match guard.into_retained() {
            Ok(retained) => checksum = checksum.wrapping_add(fold_bytes(&retained, 0)),
            Err(refused) => {
                checksum = checksum.wrapping_add(fold_bytes(&refused.guard, 0));
                refusals += 1;
            }
        }
        std::hint::black_box(attempt);
    }
    results.checksums[worker].store(checksum, Ordering::Relaxed);
    results.refusals[worker].store(refusals, Ordering::Relaxed);
    Ok(())
}

#[cfg(pfr_product_retention)]
fn promotion_measurement(
    arm: &str,
    arena_base: usize,
    elapsed_ns: u64,
    allocations: u64,
    results: &WorkerResults,
    stats: [RetentionEvidence; 3],
) -> Result<Measurement, String> {
    let workers = if arm == "eight_workers" { 8_u64 } else { 1 };
    let checksum = results.checksums.iter().fold(0_u64, |sum, value| {
        sum.wrapping_add(value.load(Ordering::Relaxed))
    });
    let refusals = results
        .refusals
        .iter()
        .map(|value| value.load(Ordering::Relaxed))
        .sum::<u64>();
    let retention = retention_delta(stats[0], stats[1], stats[2])?;
    if retention.refused_contention != refusals {
        return Err("worker refusals differ from refused_contention attribution".to_owned());
    }
    Ok(Measurement {
        iterations: Lane::SameFramePromotion.iterations(),
        useful_operations: Lane::SameFramePromotion.iterations() * workers,
        useful_bytes: Lane::SameFramePromotion.iterations() * workers * 64,
        elapsed_ns,
        checksum,
        allocations,
        threads: promotion_thread_evidence(results),
        arena: arena_for(arena_base, 20)?,
        pool_capacity: 20,
        retained_pages: 1,
        retention,
        reclaimed_frames: 0,
        backend_completions: 0,
        evictions: 0,
        wake_cycles: 0,
        parked_wakes: 0,
        wake_acks: 0,
        ring_drains: 0,
        held_transitions: 0,
    })
}

#[cfg(pfr_product_retention)]
fn promotion_thread_evidence(results: &WorkerResults) -> ThreadEvidence {
    let count = results.checksums.len();
    let affinities = SAME_FRAME_CPUS[..count]
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(";");
    let faults = (0..count)
        .map(|index| FaultDelta {
            minor: results.minor_faults[index].load(Ordering::Relaxed),
            major: results.major_faults[index].load(Ordering::Relaxed),
        })
        .collect();
    ThreadEvidence { affinities, faults }
}

#[cfg(pfr_product_retention)]
fn retention_delta(
    before: RetentionEvidence,
    sampled: RetentionEvidence,
    final_stats: RetentionEvidence,
) -> Result<RetentionEvidence, String> {
    if final_stats.occupied_budget != 0 || sampled.occupied_budget != 1 {
        return Err("same-frame anchor did not own exactly one sampled budget unit".to_owned());
    }
    Ok(RetentionEvidence {
        occupied_budget: final_stats.occupied_budget,
        refused_budget: sampled.refused_budget - before.refused_budget,
        refused_ceiling: sampled.refused_ceiling - before.refused_ceiling,
        refused_contention: sampled.refused_contention - before.refused_contention,
        refused_retiring: sampled.refused_retiring - before.refused_retiring,
        retained_evictions_held: sampled.retained_evictions_held - before.retained_evictions_held,
    })
}

#[cfg(pfr_product_retention)]
fn retention_stats(pool: &Pool) -> RetentionEvidence {
    let stats = pool.retention_stats();
    RetentionEvidence {
        occupied_budget: stats.occupied_budget,
        refused_budget: stats.refused_budget,
        refused_ceiling: stats.refused_ceiling,
        refused_contention: stats.refused_contention,
        refused_retiring: stats.refused_retiring,
        retained_evictions_held: stats.retained_evictions_held,
    }
}

#[cfg(not(pfr_product_retention))]
fn retention_stats(_pool: &Pool) -> RetentionEvidence {
    RetentionEvidence::default()
}

fn arena_for(base: usize, frames: u32) -> Result<crate::common::ArenaEvidence, String> {
    let span = u64::from(frames) * u64::from(GRANULE_BYTES);
    platform::arena_evidence(base, span)
}

fn single_thread(faults: FaultDelta, affinity: &str) -> ThreadEvidence {
    ThreadEvidence {
        affinities: affinity.to_owned(),
        faults: vec![faults],
    }
}

fn add_faults(total: &mut FaultDelta, delta: FaultDelta) {
    total.minor += delta.minor;
    total.major += delta.major;
}

fn elapsed_ns(started: Instant) -> Result<u64, String> {
    u64::try_from(started.elapsed().as_nanos()).map_err(display_error)
}

fn validate_measurement(measurement: &Measurement) -> Result<(), String> {
    if measurement.elapsed_ns == 0 || measurement.useful_operations == 0 {
        return Err("binding measurement has a zero timing or useful-work bound".to_owned());
    }
    if measurement.allocations != 0 {
        return Err(format!(
            "timed region allocated {} times",
            measurement.allocations
        ));
    }
    if measurement
        .threads
        .faults
        .iter()
        .any(|faults| faults.minor != 0 || faults.major != 0)
    {
        return Err(format!(
            "registered timed region observed thread-local faults: {:?}",
            measurement.threads.faults
        ));
    }
    Ok(())
}
