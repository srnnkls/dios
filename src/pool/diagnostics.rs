//! The one pool module permitted to name `std::sync` directly (ARCH-3).
//!
//! Every other pool concurrency primitive routes through [`crate::sync`] so loom
//! explores it. No proof reads these observations, so modelling them would cost
//! state space for nothing.

#[cfg(loom)]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicU64, Ordering};

/// A monotone observation counter. Relaxed throughout: nothing synchronises with
/// it and no invariant is stated over it.
#[derive(Debug)]
pub(crate) struct DiagnosticCounter(AtomicU64);

impl DiagnosticCounter {
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(crate) fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for DiagnosticCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// A last-write-wins observation slot holding one `u32`, for model scaffolding
/// that must survive across calls without entering the interleaving.
#[cfg(loom)]
#[derive(Debug)]
pub(crate) struct DiagnosticSlot(AtomicU32);

#[cfg(loom)]
impl DiagnosticSlot {
    pub(crate) const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    pub(crate) fn set(&self, value: u32) {
        self.0.store(value, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}
