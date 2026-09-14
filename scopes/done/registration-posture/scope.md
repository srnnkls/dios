---
created: 2026-09-04
status: done
issue_type: Task
revision: 1
origin: pinned-frame-retention T14 (amendment revision 15, 2026-09-03), split out after that scope closed
implementation: PR #7 feat/arena-registration-posture, merged to main as b1db67d (2026-09-04)
---

# Scope: registration-posture

`PoolBuilder::registration_posture(RegistrationPolicy::{Auto, Registered,
Unregistered})`, `require_locked()`, one uring dispatch table, the
best-effort arena `mlock`, and the `Pool::registration_posture()` /
`Pool::arena_locked()` readbacks.

## Why a scope of its own

pinned-frame-retention closed with 14 of 15 tasks landed (PRs #5, #6).
Its revision-15 amendment had added T14 to a working copy that never
merged into the closed record, so T14 existed only as an untracked
active copy while PR #7 implemented it. This scope is that task's
record; the stale copy is deleted.

## Premise (verified 2026-09-03/04)

- `src/backend/uring.rs` at 3fae1a4 registers BOTH arenas as io_uring
  fixed buffers unconditionally at driver build, so every pool's frame
  bytes plus write-arena bytes charge `RLIMIT_MEMLOCK`.
- The nix host's session limit is 8 MiB soft AND hard on kernel 6.6.64;
  the causal probe (plan-prefetch landscape-audit) put the ceiling near
  2,000 4 KiB frames. No unprivileged `ulimit` raise exists.
- sira-dios-migration's DM001-frozen budget is 984,027,136 bytes; no
  real-driver pool at that size can build on the box. pinned-frame-
  retention T8b's exact-R8 arm (8,035 frames) cannot build either.
- TigerBeetle ships unregistered buffers and treats memlock as an
  optional hardening step (`--development` skips `mlockall`).

## Requirements

1. RP-R1 — postures. `Registered` is today's shape (frames buf_index 0,
   write arena 1, `READ_FIXED`/`WRITE_FIXED`). `Unregistered` registers
   nothing: the same preallocated non-moving slab, plain `READ`/`WRITE`
   by pointer. `Auto` (default) attempts `register_buffers` and degrades
   to `Unregistered` on `ENOMEM` with a printed remediation; any other
   errno is the operating error it is. An explicit posture is honoured
   or refused typed (`PoolConfigError::RegistrationRefused` on uring,
   `RegistrationUnsupported` on eager), never silently downgraded.
2. RP-R2 — one dispatch table. No submit path names a buffer index;
   the SAFETY contract is restated per posture (buffer table under
   `Registered`; fixed arena addresses under `Unregistered`, the ring
   dropping before the `Arc`s it holds).
3. RP-R3 — arena lock, separate readback. After posture selection both
   arenas are `mlock`ed best-effort (`ArenaLockRefused` under
   `require_locked()`); the guard owns `Arc` clones and unlocks before
   either allocation frees. Locking charges the same limit as
   registration, so at the stock baseline neither is available and the
   page-out hazard is recorded, not solved; dios makes no integrity
   claim for resident bytes on an unlocked arena, and `arena_locked()`
   is the readback a consumer keys its verification posture on.
4. RP-R4 — readbacks at the crate root beside `IoMode`:
   `RegistrationPolicy`, `RegistrationPosture`,
   `Pool::registration_posture()`, `Pool::arena_locked()`.
5. RP-R5 — evidence. Six `setrlimit`-lowered cases on nix (Auto
   degrades; explicit Unregistered builds and reads land; explicit
   Registered and `require_locked()` refused typed; ambient limit
   selects Registered and locks). The `arena_registration` paired gate
   (Unregistered over Registered, QD1 4 KiB O_DIRECT pool reads, 40
   pairs, CI95 upper ≤ 1.15) is a no-regression bound for the
   unregistered path at QD1, not a whole-pool value of registration.
   `read_path_product` takes `DIOS_REGISTRATION_POLICY` and prints
   both readbacks.

## Non-goals

- `SparseRegistered` (BUFFERS2 extents, THP quantisation, span-level
  dispatch): arena-modernization AM4, which adds that rung to this
  scope's enum.
- Per-ring registration accounting under dios-v1 AD-4 (per-worker
  rings): AD-4's enactment obligation.
- A typed `DirectIo::Required` refusal variant: seed, not owed here.

## Verification

- PR #7: darwin all-feature suite 309 passed; nix
  `cargo test --features bench,mock --no-fail-fast` 314 passed, 1
  unrelated failure (`jq` missing on the box); memlock-floor cases 6/6;
  strict Clippy, rustdoc, rustfmt.
- `arena_registration` on nix 2026-09-04: geomean 0.9991, CI95 upper
  1.0035, gate PASS at 1.15.
- Before merge (sira review 2026-09-04): one full ring-suite run with
  the posture forced to `Unregistered`, so the plain-`READ` path is not
  covered by three floor tests and one bench alone.
