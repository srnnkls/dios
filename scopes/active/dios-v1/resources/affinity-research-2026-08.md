# Affinity research record (2026-08-21)

Four-agent research pass over dios's capability-affinity model: Rust
type-system encoding, comparable-runtime survey, overhead reduction, and
literature positioning. Condensed findings backing the 2026-08-21
amendments to dios-v1 (AD-4 escalation costs, stall-containment
escalation), arena-modernization (AM4 remediations), pinned-frame-retention
(memlock knob mechanism), and sira-dios-migration (integration seeds).

## 1. Registered-buffer memlock accounting under per-worker rings

The AD-4 escalation previously booked "N rings account the arena N times"
as an unavoidable cost. Three recorded outs, none baseline-breaking:

- `CAP_IPC_LOCK` exempts the process from `RLIMIT_MEMLOCK` accounting
  entirely (`io_uring_create` sets `ctx->user` only when
  `!capable(CAP_IPC_LOCK)`). TigerBeetle's shipped systemd posture:
  `AmbientCapabilities=CAP_IPC_LOCK`
  (https://docs.tigerbeetle.com/operating/deploying/systemd/#memory-locking).
- `IORING_REGISTER_CLONE_BUFFERS` (kernel 6.12+, liburing 2.8): register
  the arena once on a source ring, clone the buffer table into each
  worker ring by reference — pinned pages accounted once. The `nix`
  bench host (6.6) predates it; probe at open.
- The `Unregistered` posture (arena-modernization AM4) charges zero and
  composes with any ring count.

Ring SQ/CQ memory itself stopped charging `RLIMIT_MEMLOCK` at kernel
5.12 (memcg-accounted since). The remaining real escalation costs are
per-ring sizing and the issuer progress rule, not locked memory. The
pinned-frame-retention revision-11 owner decision (stock unprivileged
8 MiB baseline) stands: exemption and cloning are opt-in deployment
knobs, never baseline requirements.

## 2. Stall-radius containment (EBR stalled-reader wedge)

Precedent that the wedge is real: LeanStore 2018 is the closest published
match to dios's reclamation shape (shared pool, global epoch, per-worker
epoch slots) and its authors abandoned that design for exactly this
failure ("a single slow thread impedes global-epoch advancement, blocking
page eviction" — The Evolution of LeanStore, BTW 2023,
https://dl.gi.de/bitstreams/edd344ab-d765-4454-9dbe-fcfa25c8059c/download),
exiting to optimistic versioned reads — the branch design.md rejects as
UB through a safe `&[u8]`. dios's fixed slab converts the classic
unbounded-garbage failure into bounded `Busy` (INV-9); the availability
consequence remains pool-global.

Diagnostics floor (cheap, do first): `advance_epoch` already walks the
reader slots under the AD-4 lock, short-circuiting at the first denier;
completing the scan on the deny path and recording each denying slot's
consecutive-denial count in poll-side control state (never a store on
the reader slot line) turns "pool is Busy" into "reader slot N blocked
K advances". Precedent that report-only is the accepted floor:
Seastar's and glommio's stall detectors are diagnostics, never
containment (seastar `reactor.cc:1242-1562`, glommio `stall.rs:103`).
Related recorded prerequisite: the per-reader peak-guard release assert
is sira-dios-migration DM002; `commit_pin` currently checks only
`< u64::MAX`.

Park-boundary assert (recorded 2026-08-21, same scope as the attribution
counters): the voluntary stall pattern that produces an indefinite wedge
is a guard held across an unbounded park. Enforcement at the API
boundary: the deadline-free wait verb the containment scope adds takes
the caller's own `&ReaderCtx` and asserts its live-guard count is zero
before arming. Today's `poll_wait(out, timeout)` carries no reader
identity and no deadline-free form (`src/pool/mod.rs:1088`), so the
assert lands with that new verb; a pool-side scan of other readers'
`guard_count` is not an alternative — the field is Relaxed and
owner-thread-written by construction (`src/pool/epoch.rs:24-25,116`).
The exemption is load-bearing: a
deadline-bounded wait may hold guards — the INV-9 merge-miss case, where
a compaction reader at peak fan-out parks on its own miss with guards
live; that stall is bounded by the deadline and budgeted by
`miss_headroom`, so it is designed-for, not a wedge. Rust has no effect
system, so "stalls impossible under proper use" is unreachable in the
strict sense (nothing forbids unbounded compute inside a guard scope,
and involuntary stalls are invisible to any API); the assert closes the
one voluntary pattern that is both catastrophic and syntactically
identifiable. Rejected stronger forms: closure-scoped access (no guard
value exists, but a slow closure still stalls — and it breaks the
sans-io Ready|Pending|Busy verb and zero-copy cursor borrows); copy-out
at the boundary (impossibility by construction, priced and rejected as
the copy-per-read branch); static hold-time verification (Verus-style —
paper territory, not roadmap).

Containment candidates if attribution shows stalls that are not fixable
bugs — both amend INV-9 and replace the T009 two-advance loom model, so
either is its own scope:

- Hazard-slots per guard: publish the pinned `FrameIdx` per live guard;
  a frame is freeable when no slot names it. INV-9's static
  `peak_guards_per_reader` bound voids the classic unbounded-protected-set
  objection to hazard pointers; a stalled reader then freezes exactly its
  own frames. Cost: the SeqCst-fence-per-first-guard becomes per-guard;
  reclaim scans `max_readers × peak_guards` slots. (Hazard pointers:
  Michael 2004; batch-amortized modern form: seize/Hyaline,
  https://github.com/ibraheemdev/seize, https://arxiv.org/abs/1905.07903.)
- Interval-based reclamation: birth/retire eras per frame; readers
  publish a reservation interval (one extra Relaxed store on the owned
  slot line per pin); a stalled reader blocks only frames born before its
  snapshot, so eviction keeps churning past it (Wen et al., PPoPP 2018,
  https://www.cs.rochester.edu/u/scott/papers/2018_PPoPP_IBR.pdf).
  Cheaper reader side, softer bound than hazard-slots.

Rejected: per-shard/per-domain epoch domains (with one global page table
any reader may pin any frame, so every grace period must consult every
domain — the global scan dios already has); DEBRA+ neutralization
(signal-forced operation restart is incompatible with handing out
`&[u8]` guards); Hyaline reference counts on the guard path (reintroduces
the RMWs the warm path exists to avoid); min-epoch/unbounded-advance
(degrades the failure but the radius still grows toward pool-global under
sustained eviction).

Trigger bench (pre-registered shape): one reader parks holding a guard
while the others cycle a working set larger than the pool; metric is the
non-stalled readers' achieved eviction throughput and Busy rate —
containment sustains reuse where the current rule flatlines.

Fence proxy bench (recorded 2026-08-21; unconditional, pre-trigger,
same post-DRP scope as the attribution counters and park assert):
hazard-slots' reader-side price is one slot store + SeqCst fence per
guard instead of per first-guard, plus one extra page-table probe (the
hazard pin is lookup → publish → fence → revalidate, one probe more
than today's publish → lookup → validate) — priceable without any
hazard-pointer implementation by REPLACING the warm A/B's
per-first-guard epoch publish+fence with one slot store + SeqCst fence
+ one probe per `get` (the warm A/B drops its guard every iteration, so
every hit already fires exactly one first-pin fence; an additive
injection would price two fences, not the one hazard-slots costs — and
for the same reason the unwidened arm is an upper bound, since on
single-guard gets the two schemes coincide reader-side; the
with-widening arm carries the decision). Arms with and without the
epoch-pin
widening (widened EBR amortizes its fence across an iteration;
hazard-slots publishes per protected frame, so widening widens the
gap); the unwidened arm runs on the current path, and the widened arm
either emulates widening with a nested outer pin — `begin_pin` already
elides the publish on a nested pin — or trails the widening spike, so
the proxy's unconditional half is the unwidened arm. Decision rule: if
per-guard fencing holds a DIO-G1-shaped paired ratio (one-sided 95% CI
upper bound ≤ 1.02) on the warm A/B — DIO-G1 itself gates sira's
block-fetch layer, where the fence is amortized, so this is
DIO-G1-shaped, not DIO-G1 — hazard-slots
becomes eligible for owner-decided promotion from containment
escalation to candidate end-state — it deletes the stalled-reader
wedge class (no epoch counter, no unanimity advance; a stalled reader
freezes only its own ≤ peak_guards frames) rather than containing it,
and the containment ladder above collapses to the assert plus
attribution. If it breaks parity, hazard-slots demotes below
interval-based reclamation. Precision worth keeping: hazard-slots
removes the global liveness protocol, not global state — the slot
table, CLOCK, and page table stay shared; cleanliness lives in the
deleted consensus, the cost in the per-guard fence. Numbers freeze in
the plan file when the scope lands. The phased route (instrumentation →
proxy pricing → owner decision → protocol scope), with the Phase-3
design surface and standing risks, is
`scopes/draft/stall-containment/resources/hazard-slots-plan.md` — the
seed resource of the successor scope that executes it.

## 3. Warm-path findings

- The measured warm hit (~41 ns, `benches/plans/mmap_warm_path.md`)
  decomposes as: `get` body incl. AD-4 mutex + liveness ≈ 18 % of
  samples, seqlock probe ≈ 11 %, epoch publish ≈ 7.5 %. The
  read-protocol-atomic / DRP work owns the mutex removal and
  `ReaderSlot` padding; everything below is second-order behind it.
- Epoch-pin widening (best new lever): an explicit epoch-only pin taken
  by the TPC owner at loop-iteration start lets every inner `get` skip
  the epoch store and SeqCst fence. Caveat (2026-08-21 peer review):
  the nested-pin elision recognizes nesting solely through
  `guard_count`, so an epoch-only pin that leaves `guard_count` at zero
  needs a new epoch-state path plus its own loom coverage — the
  existing model covers live guards, not a guardless outer pin;
  alternatively the outer pin consumes `guard_count` and the peak
  formula takes +1 per reader. Fence amortization,
  not weakening (DRP's fence-change rejection does not apply). Saves the
  publish+fence (~5–7 ns) per get plus contended reader-slot-line
  traffic (~94 ns cross-CCX c2c on Zen 2,
  https://github.com/nviennot/core-to-core-latency). Cost: reclaim
  latency stretches to ≤ 2 iteration times; no INV-9 change (an
  epoch-only guard pins no frame). Precedent: crossbeam-epoch pin
  amortization (`Local::pin`), liburcu QSBR as the limit case.
- membarrier/asymmetric fences: killed by data — removing the reader
  fence outright (unsound spike, reverted) moved the pinned-host ratio
  within the run band; the fence costs ~2–4 ns. Not worth a Linux-only
  lane and a second loom model; widening amortizes it anyway.
- PageTable `Cell` shrink: the per-cell 8-byte pool-identity word is
  constant per pool — validating against `self.identity` instead makes
  cells 32 B, two per line, single-line probes.
- x86 idiom: crossbeam uses SeqCst `compare_exchange` over `store+fence`
  (`lock cmpxchg` vs `mfence`); ceiling is the ~2–4 ns fence cost — fold
  into another warm-path bench, never its own change.

## 4. Physical locality

- No surveyed runtime encodes affinity in types more strongly than dios
  already does; Seastar's shard checks are debug-only, glommio/monoio/
  compio use the same `!Send` structural device, DPDK/SPDK are
  convention plus registration. The only kernel-enforced affinity is
  `IORING_SETUP_SINGLE_ISSUER` (wrong-thread submit fails), already
  AD-4's escalation target.
- The portable locality recipe is ordering, not syscalls: pin the owner
  thread before constructing its state, so default local allocation
  first-touches correctly (glommio `executor/mod.rs:1170-1204`, compio
  `compio-runtime/src/lib.rs:453-455`, monoio
  `utils/bind_to_cpu_set.rs`). Placement unit on AMD is the L3 domain:
  `/sys/devices/system/cpu/cpu*/cache/index3/shared_cpu_list`, no hwloc
  needed. Owner-spawn pinning is sira's decision (amends the TPC-R2
  "measurement discipline only" stance) — recorded as a
  sira-dios-migration seed, not dios work.
- Arena-side placement is moot on the 3970X (UMA/NPS1), and Resident
  frame bytes are immutable (INV-7) so they replicate as clean shared
  lines across CCX L3s — cross-CCX cost lives in mutable metadata
  (reader slots, seqlock cells, control mutex). Deployment notes worth
  recording: disable AutoNUMA for a pinned+mlocked arena; on a future
  multi-node host default the shared arena to `MPOL_INTERLEAVE` or
  node-striped first-touch (Seastar `memory.cc:1858`).

## 5. Type-system verdicts (encoding is already at the right level)

- Generative brands (GhostCell/generativity/qcell,
  https://plv.mpi-sws.org/rustbelt/ghostcell/) on Pool identity:
  rejected — a `'brand` contradicts the revision-10 lifetime-free
  capability contract, breaks `thread::spawn`'s `'static` bound for
  owner threads, and infects the ring-transport element types, to
  eliminate one `Arc::ptr_eq` assert. Variance is the classic soundness
  hole in such schemes (RUSTSEC-2022-0007).
- Const-generic guard budgets: cannot close INV-9 (peak guards derive
  from runtime merge fan-out; the watermark inequality has three
  runtime variables). The adopt is the DM002 release assert.
- "This thread specifically" needs nothing beyond `!Send` (they
  coincide in safe Rust; crossbeam-epoch draws the same line). The
  meaningful strengthening is compositional: the shard's `ReaderCtx`
  lives as a field of the `!Sync` `Shard`, so reaching it requires
  `&Shard` — "this shard's owner thread" transitively, zero machinery.
- sira side: a router-minted `RoutedMiss { shard, token }` envelope with
  a receiving-owner `shard == self.id` assert mirrors the pool-identity
  check; no brands.

## 6. Landscape steals (recorded, unscheduled)

- S3-FIFO (SOSP 2023, https://dl.acm.org/doi/10.1145/3600006.3613147)
  as the concrete eviction escalation for CLOCK's scan vulnerability.
- "High-Performance DBMSs with io_uring" (arXiv 2025,
  https://arxiv.org/abs/2512.04859) + LeanStore VLDB 2024 as the
  feature-selection checklist when the AD-4 per-worker-ring escalation
  fires.
- Autonomous Commit (SIGMOD 2025) as the share-nothing write-plane
  protocol matching sira's owner model.
- glommio's `need_preempt` as a single load of a kernel-written CQ
  pointer (`reactor.rs:208`) — syscall-free preemption check for the
  owner loop.
- Publishable claim if ever wanted: "optimistic-read performance without
  optimistic reads" — validate-after-read is UB as safe Rust and the
  sanctioned escape (byte-wise atomic memcpy, RFC 3301, stalled) only
  rescues it by copying the frame; `!Send` capability affinity making
  EBR publication RMW-free is unnamed in the literature; fixed-slab EBR
  yields a closed-form Busy bound. VBR (https://arxiv.org/abs/2107.13843)
  is the reclamation-side twin of the rejected branch. Condition: bench
  the hazard-slot variant before claiming EBR is the right point.
