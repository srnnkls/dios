use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

use serde::Serialize;

use super::catalog::PageNumber;

const EVENTS_MAX: usize = 20_000;

thread_local! {
    static ALLOCATIONS: Cell<(bool, u64)> = const { Cell::new((false, 0)) };
}

struct CountingAllocator;

// SAFETY: every allocation operation forwards its arguments unchanged to System.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: layout is forwarded unchanged from the allocator caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation();
        // SAFETY: the live allocation and requested layout are forwarded unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout identify the same allocation passed by the caller.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn count_allocation() {
    let _ = ALLOCATIONS.try_with(|state| {
        let (enabled, count) = state.get();
        if enabled {
            state.set((enabled, count + 1));
        }
    });
}

pub(super) fn allocations_begin() {
    ALLOCATIONS.with(|state| state.set((true, 0)));
}

pub(super) fn allocations_end() -> u64 {
    ALLOCATIONS.with(|state| {
        let (_, count) = state.replace((false, 0));
        count
    })
}

#[derive(Clone, Copy, Default, Debug, Serialize)]
pub(super) struct Counters {
    pub(super) operations: u64,
    pub(super) useful_bytes: u64,
    pub(super) processed_bytes: u64,
    pub(super) checksum: u64,
    pub(super) hits: u64,
    pub(super) pending: u64,
    pub(super) ready_checks: u64,
    pub(super) busy: u64,
    pub(super) polls: u64,
    pub(super) poll_reclaimed: u64,
    pub(super) poll_backend_completions: u64,
    pub(super) pending_max: u64,
    pub(super) writes: u64,
    pub(super) barriers: u64,
    pub(super) pending_lifetime_ns: u64,
    pub(super) allocations: u64,
}

impl Counters {
    pub(super) fn merge(&mut self, other: Self) {
        self.operations += other.operations;
        self.useful_bytes += other.useful_bytes;
        self.processed_bytes += other.processed_bytes;
        self.checksum = self.checksum.wrapping_add(other.checksum);
        self.hits += other.hits;
        self.pending += other.pending;
        self.ready_checks += other.ready_checks;
        self.busy += other.busy;
        self.polls += other.polls;
        self.poll_reclaimed += other.poll_reclaimed;
        self.poll_backend_completions += other.poll_backend_completions;
        self.pending_max = self.pending_max.max(other.pending_max);
        self.writes += other.writes;
        self.barriers += other.barriers;
        self.pending_lifetime_ns += other.pending_lifetime_ns;
        self.allocations += other.allocations;
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Event {
    pub(super) worker: u32,
    pub(super) operation: u32,
    pub(super) page: u32,
    pub(super) phase: &'static str,
    pub(super) start_ns: u64,
    pub(super) end_ns: u64,
}

pub(super) struct Observer<const TRACE: bool> {
    pub(super) counters: Counters,
    pub(super) events: Vec<Event>,
    pub(super) origin: Instant,
    pub(super) worker: u32,
}

impl<const TRACE: bool> Observer<TRACE> {
    pub(super) fn new(worker: u32) -> Self {
        Self {
            counters: Counters::default(),
            events: Vec::with_capacity(if TRACE { EVENTS_MAX } else { 0 }),
            origin: Instant::now(),
            worker,
        }
    }

    pub(super) fn start(&self) -> Option<Instant> {
        if TRACE {
            let started = Instant::now();
            debug_assert!(started >= self.origin);
            Some(started)
        } else {
            None
        }
    }

    pub(super) fn end(
        &mut self,
        start: Option<Instant>,
        operation: u32,
        page: PageNumber,
        phase: &'static str,
    ) {
        if let Some(start) = start {
            assert!(self.events.len() < EVENTS_MAX, "trace capacity exhausted");
            let end = Instant::now();
            if phase == "pending" {
                self.counters.pending_lifetime_ns += nanoseconds(end.duration_since(start));
            }
            self.events.push(Event {
                worker: self.worker,
                operation,
                page: page.0,
                phase,
                start_ns: nanoseconds(start.duration_since(self.origin)),
                end_ns: nanoseconds(end.duration_since(self.origin)),
            });
        }
    }
}

pub(super) fn nanoseconds(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).expect("bounded workload duration fits u64")
}

#[cfg(target_os = "linux")]
pub(super) fn thread_cpu_ns() -> Result<Option<u64>, String> {
    #[repr(C)]
    struct TimeSpec {
        seconds: std::ffi::c_long,
        nanoseconds: std::ffi::c_long,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: i32, time: *mut TimeSpec) -> i32;
    }
    let mut time = TimeSpec {
        seconds: 0,
        nanoseconds: 0,
    };
    // SAFETY: Linux CLOCK_THREAD_CPUTIME_ID is 3; time points to an initialized timespec.
    if unsafe { clock_gettime(3, &raw mut time) } != 0 {
        return Err(format!(
            "thread CPU clock: {}",
            std::io::Error::last_os_error()
        ));
    }
    let seconds = u64::try_from(time.seconds).map_err(|error| error.to_string())?;
    let nanos = u64::try_from(time.nanoseconds).map_err(|error| error.to_string())?;
    Ok(Some(seconds * 1_000_000_000 + nanos))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn thread_cpu_ns() -> Result<Option<u64>, String> {
    std::thread::available_parallelism().map_err(|error| error.to_string())?;
    Ok(None)
}
