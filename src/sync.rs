//! `std::sync` for the shipping build, `loom::sync` under `--cfg loom`. loom
//! explores interleavings only over its own atomics, so every proof-bearing pool
//! concurrency primitive must resolve to `loom` types under `cfg(loom)` or the T009
//! proofs pass vacuously over `std` atomics loom cannot see.
//!
//! Invariant (ARCH-3, enforced by the `arch3_sync_alias` regression test): every
//! proof-bearing pool concurrency primitive routes through this alias. The sole
//! exception is `pool::diagnostics`, which owns the observation counters no proof
//! depends on — modelling them in loom would cost state space for zero proof
//! value. A new counter of that kind becomes a `DiagnosticCounter` there rather
//! than a fresh carve-out.

pub(crate) use std::sync::atomic::Ordering;

#[cfg(not(loom))]
pub(crate) use std::hint::spin_loop;
#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, fence};
#[cfg(not(loom))]
pub(crate) use std::sync::{Condvar, Mutex, MutexGuard};

#[cfg(loom)]
pub(crate) use loom::hint::spin_loop;
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, fence};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Condvar, Mutex, MutexGuard};

#[cfg(not(loom))]
#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

#[cfg(not(loom))]
impl<T> UnsafeCell<T> {
    #[inline]
    pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        f(self.0.get())
    }

    #[inline]
    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}

#[cfg(loom)]
pub(crate) use loom::cell::UnsafeCell;
