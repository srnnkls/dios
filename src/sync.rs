//! `std::sync` for the shipping build, `loom::sync` under `--cfg loom`. loom
//! explores interleavings only over its own atomics, so every proof-bearing pool
//! concurrency primitive must resolve to `loom` types under `cfg(loom)` or the T009
//! proofs pass vacuously over `std` atomics loom cannot see.
//!
//! Invariant (ARCH-3, enforced by the `alias_guard` regression test): every
//! proof-bearing pool concurrency primitive routes through this alias.
//! Diagnostics-only counters no proof depends on are exempt — routing them through
//! loom would cost state space for zero proof value — and are the only `std` sync
//! atomics allowed under `src/pool`:
//!   - `Clock::reference_stores` (Relaxed CLOCK store-elision observation counter)
//!   - `loom_model::PoolModel::held_frame` (`cfg(loom)` model scaffolding, not shipping)

pub(crate) use std::sync::atomic::Ordering;

#[cfg(not(loom))]
pub(crate) use std::hint::spin_loop;
#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, fence};
#[cfg(not(loom))]
pub(crate) use std::sync::{Mutex, MutexGuard};

#[cfg(loom)]
pub(crate) use loom::hint::spin_loop;
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, fence};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Mutex, MutexGuard};
