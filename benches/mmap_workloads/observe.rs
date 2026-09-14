use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

use serde::Serialize;

use super::catalog::PageNumber;

thread_local! {
    static ALLOCATIONS: Cell<(bool, u64)> = const { Cell::new((false, 0)) };
}

struct CountingAllocator;

// SAFETY: every allocation operation forwards its arguments unchanged to System.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's valid layout is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count();
        // SAFETY: allocation pointer and layout are passed unchanged to System.
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: allocation pointer and layout are passed unchanged to System.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn count() {
    let _ = ALLOCATIONS.try_with(|state| {
        let (enabled, count) = state.get();
        if enabled {
            state.set((true, count + 1));
        }
    });
}

pub(super) fn allocations_begin() {
    ALLOCATIONS.with(|state| state.set((true, 0)));
}

pub(super) fn allocations_end() -> u64 {
    ALLOCATIONS.with(|state| state.replace((false, 0)).1)
}

#[derive(Clone, Copy, Default, Serialize)]
pub(super) struct Counters {
    pub(super) operations: u32,
    pub(super) checksum: u64,
    pub(super) hits: u32,
    pub(super) pending: u32,
    pub(super) completed_pending: u32,
    pub(super) pending_max: u32,
    pub(super) polls: u64,
    pub(super) ready_checks: u64,
    pub(super) busy: u64,
    pub(super) reclaimed_lower_bound: u64,
    pub(super) allocations: u64,
    pub(super) retained_reads: u32,
    pub(super) guard_acquisitions: u32,
    pub(super) prefetch_calls: u64,
    pub(super) prefetch_deferred: u64,
}

#[derive(Clone, Default, Serialize)]
pub(super) struct Event {
    pub(super) operation: u32,
    pub(super) page: u32,
    pub(super) start_ns: u64,
    pub(super) end_ns: u64,
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct FlightEvent {
    pub(super) at_ns: u64,
    pub(super) reads: u32,
    pub(super) speculative: u32,
}

pub(super) struct Observer<const TRACE: bool> {
    pub(super) counters: Counters,
    pub(super) events: Vec<Event>,
    pub(super) flights: Vec<FlightEvent>,
    pub(super) origin: Instant,
}

impl<const TRACE: bool> Observer<TRACE> {
    pub(super) fn new(operations: u32) -> Self {
        Self {
            counters: Counters::default(),
            events: vec![Event::default(); if TRACE { operations as usize } else { 0 }],
            flights: Vec::with_capacity(if TRACE {
                operations as usize * 4 + 128
            } else {
                0
            }),
            origin: Instant::now(),
        }
    }

    pub(super) fn start(&self) -> u64 {
        if TRACE {
            nanoseconds(self.origin.elapsed())
        } else {
            0
        }
    }

    pub(super) fn finish(&mut self, operation: u32, page: PageNumber, start_ns: u64, sum: u64) {
        let counters = &mut self.counters;
        counters.checksum = counters.checksum.wrapping_add(sum);
        if TRACE {
            let entry = &mut self.events[counters.operations as usize];
            *entry = Event {
                operation,
                page: page.0,
                start_ns,
                end_ns: nanoseconds(self.origin.elapsed()),
            };
            assert!(entry.end_ns >= entry.start_ns);
        }
        counters.operations += 1;
    }

    pub(super) fn flight(&mut self, pool: &dios::Pool) {
        if !TRACE {
            return;
        }
        let stats = pool.prefetch_stats();
        if self.flights.last().is_some_and(|last| {
            last.reads == stats.reads_in_flight && last.speculative == stats.occupied
        }) {
            return;
        }
        assert!(
            self.flights.len() < self.flights.capacity(),
            "bounded flight trace exhausted"
        );
        self.flights.push(FlightEvent {
            at_ns: self.start(),
            reads: stats.reads_in_flight,
            speculative: stats.occupied,
        });
    }
}

pub(super) fn nanoseconds(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).expect("bounded benchmark duration")
}
