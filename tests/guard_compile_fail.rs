//! T007 compile-fail pins, split per the task's dependency budget (NO trybuild):
//!
//!  1. RUNTIME/compile marker assertions (this file) — that `ReaderCtx` is
//!     `!Send + !Sync` and `FrameGuard` is `!Send`, expressed with a hand-written
//!     negative-trait-bound trick (the `static_assertions::assert_not_impl_all`
//!     mechanism, inlined so no dependency is added). Each `assert_not_send!` /
//!     `assert_not_sync!` invocation compiles ONLY WHILE the property holds; if a
//!     T007 rework makes `ReaderCtx` `Send`, this file stops compiling. These pin
//!     the marker invariants (INV-6, EBR per-thread slot) against regression.
//!
//!  2. `compile_fail` DOCTESTS — the three lifetime/thread escapes below. Rustdoc
//!     doctests are harvested only from the LIBRARY crate (`cargo test --doc`), not
//!     from a `tests/` integration file, so the implementer must attach these three
//!     blocks to `src/pool/epoch.rs` (or a `#[doc]` carrier there) once the `pin`
//!     seam exists. They are reproduced verbatim in this header as the spec. They
//!     reference the T007 `Pool::pin` / `register_reader` seams, so TODAY they
//!     "fail to resolve" (the symbols do not exist) — a `compile_fail` block passes
//!     on ANY compile error, so they pass trivially/vacuously now and only become
//!     MEANINGFUL once the seam compiles and the ONLY remaining error is the
//!     lifetime/`Send` violation. This state is recorded honestly in the tester
//!     report; the implementer must re-verify each fails for the RIGHT reason.
//!
//! Doctest A — a guard's borrow must not outlive the pool that minted it (INV-6):
//!
//! ```compile_fail
//! use dios::{FrameGuard, Get, PageId, Pool, ReaderCtx};
//! fn escapes<'pool>(
//!     pool: &'pool Pool,
//!     reader: &'pool ReaderCtx<'pool>,
//!     page: PageId,
//! ) -> FrameGuard<'static> {
//!     match pool.get(reader, page) {
//!         Get::Hit(guard) => guard, // borrows `pool`; cannot escape as 'static
//!         Get::Pending(_) | Get::Busy => panic!("the lifetime is the contract"),
//!     }
//! }
//! ```
//!
//! Doctest B — a `ReaderCtx` cannot cross a thread boundary (EBR per-thread slot):
//!
//! ```compile_fail
//! use dios::Pool;
//! let pool = Pool::builder()
//!     .frame_count(16).granule(4096)
//!     .max_concurrent_readers(1).peak_guards_per_reader(1)
//!     .max_inflight_reads(1).miss_headroom(3)
//!     .build().unwrap();
//! let reader = pool.register_reader().unwrap();
//! std::thread::spawn(move || {
//!     drop(reader); // ReaderCtx is !Send — a consuming use forces the move, which must not compile
//! });
//! ```
//!
//! Doctest C — a `ReaderCtx` cannot outlive the pool it was registered against:
//!
//! ```compile_fail
//! use dios::{Pool, ReaderCtx};
//! fn outlives() -> ReaderCtx<'static> {
//!     let pool = Pool::builder()
//!         .frame_count(16).granule(4096)
//!         .max_concurrent_readers(1).peak_guards_per_reader(1)
//!         .max_inflight_reads(1).miss_headroom(3)
//!         .build().unwrap();
//!     pool.register_reader().unwrap() // borrows `pool`; cannot escape as 'static
//! }
//! ```

use dios::{FrameGuard, ReaderCtx};

struct SecondImpl;

trait AmbiguousIfSend<A> {
    fn resolve() {}
}
impl<T: ?Sized> AmbiguousIfSend<()> for T {}
impl<T: ?Sized + Send> AmbiguousIfSend<SecondImpl> for T {}

trait AmbiguousIfSync<A> {
    fn resolve() {}
}
impl<T: ?Sized> AmbiguousIfSync<()> for T {}
impl<T: ?Sized + Sync> AmbiguousIfSync<SecondImpl> for T {}

/// Resolves `<$t as AmbiguousIfSend<_>>::resolve` — the method is ambiguous (two
/// applicable impls) exactly when `$t: Send`, so this compiles ONLY WHILE `$t` is
/// `!Send`.
macro_rules! assert_not_send {
    ($t:ty) => {
        let _ = <$t as AmbiguousIfSend<_>>::resolve;
    };
}

macro_rules! assert_not_sync {
    ($t:ty) => {
        let _ = <$t as AmbiguousIfSync<_>>::resolve;
    };
}

#[test]
fn reader_ctx_is_neither_send_nor_sync() {
    assert_not_send!(ReaderCtx<'static>);
    assert_not_sync!(ReaderCtx<'static>);
}

#[test]
fn frame_guard_is_not_send() {
    assert_not_send!(FrameGuard<'static>);
}
