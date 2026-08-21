//! Compile-time capability trait assertions.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use dios::{FrameGuard, PendingToken, ReaderCtx, RetainedFrame};

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

trait AmbiguousIfClone<A> {
    fn resolve() {}
}
impl<T: ?Sized> AmbiguousIfClone<()> for T {}
impl<T: Clone> AmbiguousIfClone<SecondImpl> for T {}

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

macro_rules! assert_not_clone {
    ($t:ty) => {
        let _ = <$t as AmbiguousIfClone<_>>::resolve;
    };
}

fn assert_send<T: Send>() {}

#[test]
fn reader_ctx_is_neither_send_nor_sync() {
    assert_not_send!(ReaderCtx);
    assert_not_sync!(ReaderCtx);
}

#[test]
fn pending_token_is_send_but_affine() {
    assert_send::<PendingToken>();
    assert_not_clone!(PendingToken);
}

#[test]
fn frame_guard_is_neither_send_nor_sync() {
    assert_not_send!(FrameGuard<'static>);
    assert_not_sync!(FrameGuard<'static>);
}

#[test]
fn retained_frame_is_neither_send_sync_nor_clone() {
    assert_not_send!(RetainedFrame<'static>);
    assert_not_sync!(RetainedFrame<'static>);
    assert_not_clone!(RetainedFrame<'static>);
}

fn compile_probe(name: &str, source: &str) -> std::process::Output {
    let probe = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    fs::create_dir_all(probe.join("src")).expect("create the isolated compile-fail probe");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    fs::write(
        probe.join("Cargo.toml"),
        format!(
            "[package]\nname = \"dios-{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\ndios = {{ path = \"{manifest_dir}\" }}\n"
        ),
    )
    .expect("write the compile-fail manifest");
    fs::write(probe.join("src/lib.rs"), source).expect("write the compile-fail source");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Command::new(cargo)
        .args(["check", "--offline", "--quiet"])
        .current_dir(&probe)
        .env(
            "CARGO_TARGET_DIR",
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("compiler-probe-target"),
        )
        .output()
        .expect("run the isolated compiler probe")
}

fn compile_root_import(symbol: &str) -> std::process::Output {
    compile_probe(
        "forbidden-root-import",
        &format!("#![allow(unused_imports)]\n\nuse dios::{symbol};\n"),
    )
}

#[test]
fn advanced_driver_vocabulary_does_not_import_from_the_crate_root() {
    for symbol in [
        "Completion",
        "Driver",
        "FileHandle",
        "SubmitError",
        "WriteArena",
        "WriteSlot",
        "OpToken",
        "OpKind",
        "CompletionBatch",
    ] {
        let output = compile_root_import(symbol);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "dios::{symbol} unexpectedly compiled; advanced vocabulary belongs under dios::driver"
        );
        assert!(
            stderr.contains("E0432") && stderr.contains(&format!("dios::{symbol}")),
            "dios::{symbol} must fail specifically as an unresolved root import (E0432):\n{stderr}"
        );
    }
}

#[test]
fn a_frame_guard_cannot_remain_live_across_reader_drop() {
    let output = compile_probe(
        "guard-reader-borrow",
        r"#![allow(dead_code, elided_lifetimes_in_paths)]

use dios::{PendingToken, Pool, ReaderCtx, ReadyResult};

fn reject_guard_across_reader_drop(
    pool: &Pool,
    reader: ReaderCtx,
    token: PendingToken,
) {
    let guard = match pool.ready(&reader, token) {
        ReadyResult::Ready(guard) => guard,
        ReadyResult::NotYet(_) | ReadyResult::Err(_) => return,
    };
    drop(reader);
    std::hint::black_box(&guard);
}
",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a FrameGuard unexpectedly outlived the ReaderCtx epoch pin"
    );
    assert!(
        stderr.contains("E0505")
            && stderr.contains("cannot move out of `reader` because it is borrowed"),
        "the compiler probe must fail specifically because reader remains borrowed by the live guard:\n{stderr}"
    );
}

#[test]
fn a_frame_guard_cannot_remain_live_across_pool_drop() {
    let output = compile_probe(
        "guard-pool-borrow",
        r"#![allow(dead_code, elided_lifetimes_in_paths)]

use dios::{PendingToken, Pool, ReaderCtx, ReadyResult};

fn reject_guard_across_pool_drop(
    pool: Pool,
    reader: &ReaderCtx,
    token: PendingToken,
) {
    let guard = match pool.ready(reader, token) {
        ReadyResult::Ready(guard) => guard,
        ReadyResult::NotYet(_) | ReadyResult::Err(_) => return,
    };
    drop(pool);
    std::hint::black_box(&guard);
}
",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a FrameGuard unexpectedly outlived its Pool"
    );
    assert!(
        stderr.contains("E0505")
            && stderr.contains("cannot move out of `pool` because it is borrowed"),
        "the compiler probe must fail specifically because pool remains borrowed by the live guard:\n{stderr}"
    );
}

#[test]
fn a_retained_frame_cannot_remain_live_across_reader_drop() {
    let output = compile_probe(
        "retained-reader-borrow",
        r"#![allow(dead_code, elided_lifetimes_in_paths)]

use dios::{PendingToken, Pool, ReaderCtx, ReadyResult};

fn reject_retained_across_reader_drop(
    pool: &Pool,
    reader: ReaderCtx,
    token: PendingToken,
) {
    let guard = match pool.ready(&reader, token) {
        ReadyResult::Ready(guard) => guard,
        ReadyResult::NotYet(_) | ReadyResult::Err(_) => return,
    };
    let retained = match guard.into_retained() {
        Ok(retained) => retained,
        Err(_) => return,
    };
    drop(reader);
    std::hint::black_box(&retained);
}
",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a RetainedFrame unexpectedly outlived the ReaderCtx epoch pin"
    );
    assert!(
        stderr.contains("E0505")
            && stderr.contains("cannot move out of `reader` because it is borrowed"),
        "the compiler probe must fail specifically because reader remains borrowed by the retained frame:\n{stderr}"
    );
}

#[test]
fn a_retained_frame_cannot_remain_live_across_pool_drop() {
    let output = compile_probe(
        "retained-pool-borrow",
        r"#![allow(dead_code, elided_lifetimes_in_paths)]

use dios::{PendingToken, Pool, ReaderCtx, ReadyResult};

fn reject_retained_across_pool_drop(
    pool: Pool,
    reader: &ReaderCtx,
    token: PendingToken,
) {
    let guard = match pool.ready(reader, token) {
        ReadyResult::Ready(guard) => guard,
        ReadyResult::NotYet(_) | ReadyResult::Err(_) => return,
    };
    let retained = match guard.into_retained() {
        Ok(retained) => retained,
        Err(_) => return,
    };
    drop(pool);
    std::hint::black_box(&retained);
}
",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a RetainedFrame unexpectedly outlived its Pool"
    );
    assert!(
        stderr.contains("E0505")
            && stderr.contains("cannot move out of `pool` because it is borrowed"),
        "the compiler probe must fail specifically because pool remains borrowed by the retained frame:\n{stderr}"
    );
}

#[test]
fn a_warm_hit_frame_guard_cannot_remain_live_across_reader_drop() {
    let output = compile_probe(
        "guard-reader-get-borrow",
        r"#![allow(dead_code, elided_lifetimes_in_paths)]

use dios::{Get, PageId, Pool, ReaderCtx};

fn reject_warm_hit_guard_across_reader_drop(
    pool: &Pool,
    reader: ReaderCtx,
    page: PageId,
) {
    let guard = match pool.get(&reader, page) {
        Ok(Get::Hit(guard)) => guard,
        Ok(Get::Pending(_) | Get::Busy) | Err(_) => return,
    };
    drop(reader);
    std::hint::black_box(&guard);
}
",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a warm-hit FrameGuard unexpectedly outlived the ReaderCtx epoch pin"
    );
    assert!(
        stderr.contains("E0505")
            && stderr.contains("cannot move out of `reader` because it is borrowed"),
        "the warm-hit compiler probe must fail specifically because reader remains borrowed by the live guard:\n{stderr}"
    );
}
