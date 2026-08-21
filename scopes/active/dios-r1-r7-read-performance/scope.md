---
created: 2026-08-19
status: active
issue_type: Feature
revision: 3
---

# Scope: Dios R1-R7 read-path performance

## Goal

Ship the Dios-owned part of the R1-R7 read-path work as production code:

- remove the AD-4 mutex from live-file validation with a generation-exact
  liveness mirror;
- isolate adjacent `ReaderSlot`s and decide the page hash through a
  pre-registered quality-and-speed gate;
- promote the R7 `ResidentFileLease` and compact `ResidentHint` idea from a
  manifest-bound mock experiment to the generic production `Pool` API;
- make a missing, mismatched, or stale hint execute ordinary `Pool::get` rather
  than becoming a new failure mode; and
- prove retirement and frame reuse safety with the real proof-bearing
  primitives before any performance result can authorize adoption.

The scope is Dios-only. It owns no Sira source, test, bench, fixture, scope, or
artifact file. Its cross-repository consumer is
`sira-aligned-buffers`; that scope owns Sira integration and the
end-to-end REMIX verdict.

## Context and provenance

Both experiment worktrees are based on
`1004a2e6fcae0bcc9552dc3211c2416e388a250d` and remain uncommitted evidence:

| Candidate | Canonical location | Use in this scope |
|---|---|---|
| atomic read protocol | `.worktrees/experiment-read-protocol-tightened` | liveness-mirror and `ReaderSlot`-alignment source; its packed hash is rejected |
| R7 native hint | `.worktrees/experiment-sira-point-proof` | reference only; the live tree contains later R8 retention contamination |

The authoritative R7 record is
`scopes/active/dios-v1/resources/remix-dios-native-experiment.md`. The concise
source and numeric audit for this scope is [resources/evidence.md](resources/evidence.md).
The exact **base-to-R7 source carrier** is tracked with this scope as
[`resources/r7-source.diff`](resources/r7-source.diff), 16,708 bytes, SHA-256
`16748dd827b1958b6889535f7244fb8ffe767dac674d587f9fee18738be4a967`.
The separately tracked exact plan snapshot is
[`resources/sira_native_hint.md`](resources/sira_native_hint.md), 3,907 bytes,
SHA-256
`337ecf1aafdbded58c38cca5efdf62056e139e2521744d305b43b962ebd62ecf`.
Applying the carrier to the base and installing that plan reproduces every R7
SHA-256 recorded in the canonical ledger. Its historical source under an R8
artifact directory grants no authority to any R8 code or result and is not
required for reconstruction.

Directly copying the current point-proof worktree is forbidden. Its `src/lib.rs`
and `src/pool/mod.rs` contain `ResidentSetError`, `ResidentSetLease`, retention
counts, and resident-set methods that are outside this scope. The extraction
task starts from the clean base, applies the carrier, checks all recorded
hashes, and then translates only R7 behavior into production code.

`pinned-frame-retention` remains a behavioral non-goal but is serialized after
DRP010 because both scopes change the frame-state, reclaim, sync, Loom, and
public API files. Its implementation must rebase and re-review HELD against the
selected packed-state branch. Forbidden R8-symbol scans in this scope apply
only to the isolated R7 extraction and this scope's own delta, never to a later
legitimate retention implementation.

Before product work, DRP001 also records the ownership handoff in
`scopes/draft/read-protocol-atomic/` and `scopes/active/dios-v1/`. This scope
supersedes their overlapping liveness/hint primitive implementation work;
`read-protocol-atomic` remains an evidence source, while dios-v1 T018/DIO-G10
retains the migration-level consumer gate and consumes this scope's result.

## Requirements

### Atomic ordinary-get path

- Add one preallocated atomic live-generation word per file slot. A live word
  encodes the complete `FileId` generation plus a live tag; zero is not live.
  Foreign-pool identity is asserted before indexing.
- Registration publishes the control-plane file entry before the mirror's
  Release store. Retirement changes the entry to `Retiring` and clears the
  mirror with Release before mapping removal or close. A warm `get` uses one
  Acquire load and exact generation comparison without taking AD-4.
- A miss takes AD-4 and repeats the authoritative file-state check before it
  inserts or joins singleflight work. The mirror is an admission fast path, not
  a second source of truth.
- `ReaderSlot` is cache-line separated with `#[repr(align(64))]`. The existing
  `begin_pin` publish-before-validate protocol and its `SeqCst` fence are
  unchanged.
- The packed hash from the experiment
  (`slot << 32 | generation ^ granule_idx`) never ships. The only eligible
  replacement is the full-width one-round candidate defined in `design.md`.
  It ships only if DRP-G1 passes; otherwise the current four-round hash remains
  and the scope records that decision as a successful outcome.

### R7 product capability

- `ResidentFileLease` is an owned, lifetime-free, non-`Clone` capability for
  one exact pool identity and `FileId` generation. It owns one `Arc` clone of
  a file-slot lease state allocated at pool construction; acquisition itself
  allocates nothing. Acquisition is serialized by AD-4:
  it either increments the bounded per-slot lease count while the file is
  `Live`, or returns a typed stale/exhausted value. Retirement and acquisition
  therefore have one explicit lock linearization order.
- Retirement immediately stops new ordinary admission and new leases. A lease
  acquired before that point delays file-frame retirement, close, and slot
  reuse until its drop. Lease drop follows the existing bounded retirement
  progress path; it is not in the per-value read loop.
- `ResidentHint` is an opaque, pool-minted, volatile `Copy` observation. It
  carries only the stable granule ordinal, a frame index, and a nonzero packed
  residency stamp. `size_of::<ResidentHint>()` and
  `size_of::<Option<ResidentHint>>()` are both pinned to 16 bytes. It is never
  persisted in REMIX or any on-disk format.
- Every frame has one preallocated exact-`PageId` metadata cell. A refill
  writes bytes and that full identity before Release-publishing the new
  Resident stamp. The stable `PageId` is supplied again on every hinted access.
  After epoch publication and exact stamp validation, the reader compares the
  frame metadata with that full `PageId` before reading bytes. Same-granule
  hints from another file or pool cannot authorize unrelated bytes: metadata
  mismatch falls back, while a coincidental exact target-page match can return
  only the correctly requested target bytes.
- A hinted hit performs the existing reader-epoch publication, exact residency
  stamp validation, CLOCK reference, guard commit, and returns the normal
  `FrameGuard`. The bytes are decoded only while that guard is live.
- `None`, a page mismatch, or a stale stamp aborts an uncommitted first pin and
  calls ordinary `Pool::get(reader, page)`. The result remains
  `Hit | Pending | Busy` or `GetError::StaleFile`; hint staleness adds no public
  error variant. A caller may refresh the hint after ordinary acquisition.
- A hint frame index outside the target pool's fixed frame arena is rejected
  before any frame-array indexing and takes the same ordinary-get fallback. A
  foreign-pool hint that happens to name an in-range target frame can return a
  hinted hit only if that target frame independently validates the exact
  requested `PageId` and stamp; it can never expose bytes from the source pool.
- The packed residency word is the frame-state source of truth: low bits encode
  `FrameState`, upper bits are a monotonically increasing residency generation.
  Every transition into `Resident` increments the generation; leaving
  `Resident` publishes a non-resident word before reclamation. One state load or
  store remains one atomic operation.
- The exact-page cell is isolated in `Frames` behind one audited unsafe
  accessor. It is written only while the frame is unpublished and cannot be
  overwritten until mapping removal plus two successful epoch advances permit
  reuse. A reader accesses it only after publishing its epoch and Acquire-
  validating the Resident stamp.

### Safety and boundedness

- Proof-bearing atomics route through `crate::sync`; Loom exercises the actual
  `ReaderSlot`, liveness word, packed frame word, page-table cell, and eviction
  queue. A duplicate protocol model is not evidence.
- Loom must cover: ordinary get and lease acquisition versus retire/full
  retirement/file-slot reuse; and hinted acquisition versus mapping removal,
  eviction, two successful epoch advances, frame reuse, and a new residency
  generation. Every execution yields typed stale/fallback or a guard
  linearized before invalidation whose bytes are not reused until drop.
- Counts, attempts, tables, and Loom state spaces are fixed before execution.
  Lease-count exhaustion is a value, not an assertion or wraparound.
- There is no allocation after pool construction on ordinary hit, hinted hit,
  stale-hint fallback, lease acquire/drop, or retirement progress. The existing
  zero-allocation harness covers both shipping backends.
- When no lease or hint API is used, ordinary warm `get` executes no
  hint-specific branch, load, store, or RMW. The packed frame word replaces the
  existing frame-state atomic rather than adding a second transition atomic;
  per-file lease counts are control-plane state only.

## API contract

Exact naming may change only in review; the behavior and capability boundaries
are normative.

```rust
pub struct ResidentFileLease { /* owned exact pool + FileId generation capability; !Clone */ }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidentHint { /* granule ordinal + frame + nonzero residency stamp */ }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentLeaseError {
    StaleFile { file: FileId },
    Exhausted { file: FileId },
}

impl<D: PoolBackend> Pool<D> {
    pub fn lease_file(
        &self,
        file: FileId,
    ) -> Result<ResidentFileLease, ResidentLeaseError>;

    pub fn resident_hint(
        &self,
        lease: &ResidentFileLease,
        page: PageId,
    ) -> Option<ResidentHint>;

    pub fn get_with_hint<'pool>(
        &'pool self,
        reader: &'pool ReaderCtx,
        lease: &ResidentFileLease,
        page: PageId,
        hint: Option<ResidentHint>,
    ) -> Result<Get<'pool>, GetError>;
}
```

`get_with_hint` owns fallback: consumers do not reimplement the epoch abort or
ordinary-get transition. `resident_hint` is advisory and lock-free; racing
eviction may make its result stale immediately, which is safe by construction.

## Performance plan and gates

All implementation-facing bench plans land before product code. Binding runs
use `nix` (Threadripper 3970X, kernel 6.6.64), performance governor, explicit
CPU affinity, at least 30 order-alternated paired process repetitions, raw CSV,
and the shared paired-log compare harness. Every ratio below is
`candidate / base`; lower is better unless stated otherwise.

### DRP-G1: hash admissibility

The full-width one-round candidate first runs the deterministic 50%-load matrix
from the atomic experiment: 1/16/256 files, sequential and 64-page-interleaved
granules, and 1 Ki/128 Ki/512 Ki slot tables. At 1 Ki, mean probes must be at
most `1.05x` current and p99 at most current `+1` probe. At larger tables, mean
and p99 must be at most `1.05x` current. Every maximum chain must be `<64`.

That historical matrix is calibration because its `+1` small-table p99 rule
was chosen after its integer results were visible. Adoption additionally
requires the pre-frozen independent holdout in `design.md`: new driver IDs,
file populations, generations, table sizes, access permutations, and seeds
that do not occur in the calibration input. The same mean/p99/max bounds apply
to every holdout row. A calibration or holdout failure retains the current
four-round hash.

Only after that passes, a real-`Pool<Driver>` anonymous warm-get A/B compares
the candidate with the current four-round hash on the full-granule workload;
its one-sided 95% CI upper bound must be `<=0.98`. Any failed sub-gate retains
the current hash. The packed candidate is not rerun.

### DRP-G2: unused-capability overhead

Compare the candidate immediately before and after production lease/hint
support, with no lease or hint calls, on real `Pool<Driver>`:

1. warm ordinary `get`, full-granule fold; and
2. a bounded cycling working set larger than the pool, including completion,
   eviction, two-advance reclaim, and frame reuse.

Each CI upper bound must be `<=1.01`, and the allocation counter must remain
zero after warmup. Failure removes or redesigns hint-only state; it cannot be
waived as “not on the warm path.”

### DRP-G3: production hint materiality

On one real file and a fully resident deterministic page set, compare
`get_with_hint` hits with ordinary `get` hits, consuming identical full-frame
bytes. The one-sided 95% CI upper bound must be `<=0.95`. This repeats R7's
materiality question on the shipping backend; the observed mock result
`0.8813 / 0.8980` is prior evidence, not a pass.

If the gate fails, do not ship the public hint/lease surface. Retain the
liveness/alignment work and the DRP-G1 hash decision, and return the Sira
consumer to ordinary get/composition.

### DRP-G4: real contention

Use a shared real `Pool<Driver>`, one `ReaderCtx` per thread, 512 resident
4 KiB pages, 64 disjoint pages per reader, and CCX-0 CPUs `0-3,32-35`. The
full-granule fold is binding; a one-word lane is diagnostic only.

- For ordinary get, candidate 8-thread ns/op over the base protocol's
  8-thread ns/op has CI upper `<=1.00`.
- Within the candidate, normalized 8-thread ns/op over 1-thread ns/op has CI
  upper `<=0.50` (equivalent to at least `2x` aggregate scaling).
- If DRP-G3 accepts the hint surface, repeat the scaling check for hinted hits;
  its normalized CI upper is also `<=0.50`. If DRP-G3 rejects hints, this lane
  is recorded as not applicable rather than blocking the ordinary-get result.

A failure profiles the full-fold lane. Liveness-mirror contention, hint-stamp
traffic, and reader-slot placement are the only in-scope levers; fence
weakening is not.

## Acceptance criteria

- [x] Given clean base `1004a2e6...`, the tracked R7 carrier, and the tracked
  hint plan, when the extraction audit runs, then the four product/test source
  files and installed R7 hint plan match all five canonical SHA-256 values and no `ResidentSet`,
  retention count, or R8 API symbol is present.
- [x] Given a warm page whose file races retirement and slot reuse, when the
  real-primitives Loom case explores all bounded schedules, then get/lease is
  stale or linearized before retirement and no guard observes the reused file
  generation.
- [x] Given a previously minted hint, when eviction removes its mapping, two
  epoch advances reclaim the frame, and a different page becomes resident in
  that frame, then the real-primitives Loom case returns ordinary fallback or
  the pre-eviction bytes under a live guard, never the new bytes through the old
  hint.
- [x] Given same-granule hints minted for a different file and a different
  pool, when the target frame/stamp would otherwise collide, then exact
  frame-`PageId` validation either falls back on mismatch or returns only an
  independently matching requested target page, never foreign bytes.
- [x] Given `None`, a wrong-granule hint, and an old residency stamp, when
  `get_with_hint` runs, then each takes the exact ordinary-get result path,
  leaves the reader quiescent when no guard exists, and refresh can mint a new
  hint after readiness.
- [x] Given retirement starts with an existing lease, when new lease/get calls
  arrive and the old lease later drops, then new admission is stale, slot reuse
  waits for the old capability, and bounded retirement completes once all
  ordinary interests are also gone.
- [x] Given a lease outlives its originating `Pool`, dropping it accesses only
  its owned preallocated `Arc` lease/wake state, decrements exactly once, and
  performs no allocation or use-after-free.
- [x] Given default ordinary access, hinted access, and stale-hint fallback on
  both backends, then the zero-allocation harness, full tests, strict clippy,
  fmt, miri pure-memory lane, and Linux asan syscall lane are green.
- [x] DRP-G1 through DRP-G4 are run exactly as pre-registered. A failed hash
  gate retains the current hash; failed DRP-G2 or a safety gate blocks the
  feature; failed DRP-G3 removes the product hint surface.
- [ ] The companion `sira-aligned-buffers` scope consumes a pinned Dios
  commit and records its own end-to-end verdict before this scope closes. This
  repository changes no Sira file.

## Dependency graph

Machine-readable ordering is in [dependencies.yaml](dependencies.yaml).

```text
provenance + bench plans
          |
          v
 conservative callable scaffold
          |
          v
mirror/alignment RED+GREEN -> hash decision
          |
          v
lease RED+GREEN -> hint RED+GREEN + Loom/fallback/retirement
          |                         |
          +----------+--------------+
                     v
       verification + real-backend gates
                     |
                     v
 Sira companion evidence (closeout dependency only)
```

## Non-goals and rejected changes

- R8 retention in every form: no `ResidentSetLease`, retained frame, pinned
  resident set, retention count, retained-victim policy, or descriptor-stream
  fast path.
- Any Sira edit, fixture regeneration, benchmark implementation, scope update,
  or gate decision.
- The always-present dense/vacant hint representation; R7 measured it slower
  and Rust's nonzero niche already keeps `Option<ResidentHint>` compact.
- Weakening or removing `ReaderSlot::begin_pin`'s `SeqCst` fence, substituting a
  weaker memory order, or treating timing as a safety proof.
- Persisting `FrameIdx`, residency generations, pointers, epochs, guards, or
  hints in REMIX/on disk.
- Treating `MockDriver`, an in-process R7 result, or an unbound old binary label
  as product or performance proof.
- Changing CLOCK victim selection, frame retention, pool watermarks, the
  three-way `Get` residency ADT, or the direct-I/O backend topology.

## Open state

- Revision 3 retargets the read-only handoff to the active
  `sira-aligned-buffers:SAB008` endpoint. The reviewed Dios work through
  DRP009 and the DRP010 contract documentation are published; closeout waits
  only on that external endpoint's recorded result.
- The cross-repository scope is
  `/Users/srnnkls/projects/sira/scopes/active/sira-aligned-buffers/`.
  Its existence does not broaden Dios ownership.
- The hash outcome is intentionally data-dependent and is recorded by DRP-G1;
  it is not an unresolved architecture question.
