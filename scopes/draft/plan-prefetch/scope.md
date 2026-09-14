---
created: 2026-08-18
status: draft
issue_type: Feature
revision: 12
---

# Scope: plan-prefetch

Execution amendment, 2026-09-13: the owner requested the explicit readahead
surface while investigating the new mmap comparisons, and selected automatic
access-pattern detection **enabled by default, with a way to disable it**.
That supersedes the default-off policy direction below. The implementation
boundary and current registration posture are recorded in [design.md](design.md);
the original evidence ladder remains historical. The new falsifiable gates
live in `benches/plans/prefetch_admission.md`. This work does not adopt a new
ring topology, reclamation protocol, or general cache-policy trait.

Execution evidence, 2026-09-13: the explicit surface and default sequential
policy are implemented within that boundary. All 12 numeric adoption and
historical regression gates pass on `nix`; review, safety checks, observer
controls and eight qualified CPU profiles are recorded in the
[current evidence report](../../../benches/evidence/mmap_workloads/prefetch.md).
The original ladder below remains historical; mmap still wins the separately
measured sequential comparisons.

A plan-driven prefetch surface on `Pool`: a consumer holding an exact
access plan submits page windows ahead of its synchronous consuming
cursor, and the pool overlaps the cold reads while consumption stays on
the existing `get`/`ready` path. Seeded from
`~/projects/sira/resources/dios-seeds.md` (access-plan pushdown); every
load-bearing claim below is measured, not asserted — the experiment
ladder ran before this draft stabilized.

## Evidence ladder (2026-08-18)

All Linux rows on the pinned host (Threadripper 3970X, 970 PRO, kernel
6.6.64); Darwin rows on MacBookPro18,3 (M1 Pro, AP0512R). Ratios are
candidate/base, lower is better. Spike artifacts live on the
`spike/prefetch-reserve` worktree; main-tree artifacts are named.

| # | experiment | key result | artifact |
|---|---|---|---|
| 1 | fio ceiling, 4K randread io_uring | QD1 15.9k IOPS (59.8 µs) → QD32 274k IOPS: 17× available | ad hoc, recorded here |
| 2 | driver bypass, submit-32-reap-32 vs serial | 57 → 7 µs/read, ratio 0.127 — the ring batches correctly | `qd_probe` (recorded, deleted) |
| 3 | existence gate, unpressured (API-free: `get` + dropped token) | 0.18, ci95 0.21, gate ≤ 0.5 PASS | `benches/prefetch_window.rs` + plan (main tree) |
| 4 | existence gate, pressured (256-frame recycling) | 0.93 FAIL — 10,210 lookahead `Busy`, 10,197 demand stalls | recorded in `benches/plans/prefetch_window.md` |
| 5 | reserve sweep on the pressured workload | 8 frames → 0.145; 16 → 0.127; 32 → 0.112; 64 → 0.113 (all PASS ≤ 0.5) | `benches/prefetch_reserve_sweep.rs` (spike) |
| 6 | memlock attribution | ENOMEM boundary moves in lockstep with `RLIMIT_MEMLOCK` at 1/4/8 MiB, `native_workers=true` | `benches/memlock_probe.rs` (spike), `resources/landscape-audit.md` |
| 7 | Darwin bounded-pool probe, cold scatter F_NOCACHE | K=2 0.37, K=4 0.20, K=8 0.107, K=16 0.097; sequential readahead control: buffered 42× over F_NOCACHE (1.3 vs 46 µs/page) | `benches/darwin_pool_probe.rs` (spike) |
| 8 | cold-scatter mmap bracket, both hosts | vs 1-cursor mmap `MADV_RANDOM`: 0.109 Linux / 0.170 Darwin; vs fault-around: 0.086; vs 8 faulting threads: 0.903 Linux (one dios thread) / 0.841 Darwin | `benches/mmap_cold_bracket.rs` (spike) |
| 9 | eager-inline controls (macOS) | existence 0.97 (no overlap possible); reserve sweep 0.56–0.74 (reserve removes CPU-side claim stalls) | same benches, advisory runs |
| 10 | isolated warm path, both archs, both trees | pool warm hit vs bare mmap resident read: 1.73 macOS / 2.05 Linux; spike tree (reserve present, off): 1.80 / 2.09 — warm-neutral within noise; all PASS the 3.0 sanity gate | `benches/plans/mmap_warm_path.md` runs table |
| 11 | warm path under dTLB pressure (256 MiB resident) | macOS 0.64 — the POOL BEATS bare mmap; Linux 1.18 (THP `madvise`: the arena is `MADV_HUGEPAGE`-advised and 2 MiB-aligned in `src/pool/frames.rs`, the file mapping stays on 4 KiB pages). Bench had a latent defect (event-recorder bound), fixed on main. Granule + THP follow-ups in the plan: fixed-cost model validated, small-set 64 KiB flip clears CI, 256 MiB is parity; arena verified PMD-mapped, THP worth 30 points at 4 KiB/256 MiB | `benches/plans/mmap_tlb_pressure.md` runs table |

### Insights the design stands on

1. The win exists and flows through the completion model: 5.5×
   unpressured through today's public API (row 3), against a 17×
   device ceiling (row 1) and a 0.127 driver-level ceiling (row 2).
2. The pool claim path, not ring submission, is the serializer: under
   frame recycling, claim-on-miss is bound by epoch-matured reclamation
   and a burst claimer collapses to QD1 (row 4). Any prefetch admission
   that rides the demand claim path is dead on arrival.
3. A tiny reserved credit inventory suffices: 8 frames flip the
   pressured FAIL to 0.145; the curve plateaus at reserve = lookahead
   (row 5). The wholesale reclamation redesign (`landscape.md`
   §5/§16/§17) is unnecessary — killed by data, deferred out of scope.
4. Reserve admission is the right path outright, not a pressure
   workaround: the pressured drain with a 32-frame reserve (0.112)
   beats the unpressured get+drop form (0.18). Prefetch pretending to
   be demand pays demand-path costs (per-page lock, wake, claim);
   a dedicated admission path is faster even when frames are plentiful.
   The macOS control (row 9) shows the same mechanism removes CPU-side
   claim stalls where no I/O overlap exists at all.
5. mmap is beaten on cold scatter in every form (row 8): 6–12× against
   a single faulting cursor (how engines actually use mmap — faults
   serialize at QD1), and still ahead against eight dedicated faulting
   threads — on Linux with ONE dios thread, with the page-cache
   asymmetry favoring mmap. Default fault-around is 32% slower than
   `MADV_RANDOM` on scatter: the kernel's own prefetch actively hurts
   the shape this scope targets. Warm parity is NOT covered here — it
   stays owned by DIO-G1 and falsifier 2.
6. Darwin: the bounded-pool premise is real (near-linear pread scaling
   to K=8, ~10× plateau), and dios's F_NOCACHE plane forfeits kernel
   readahead entirely — cold sequential is 42× slower than buffered
   (row 7). Within dios-as-is the pool's value is NOT confined to
   scatter. Fork owned by the Darwin follow-up: buffered fds for
   sequential shapes (readahead free, double-caching returns) vs
   F_NOCACHE + pool for everything vs per-shape choice.
7. Registered buffers are memlock-charged on the pinned box — causally
   attributed (row 6), overturning the
   `io_uring_registered_buffers(7)` native-workers/cgroup claim for
   this host: ~2,000 4-KiB frames per process at default limits
   (8 MiB soft and hard). Headroom sizing inherits this.
8. The warm picture is measured in isolation (rows 10–11): the pool's
   residency machinery costs 1.7–2.1× a bare mmap load only at tiny
   working sets; under real dTLB pressure the gap collapses to 1.18 on
   Linux and INVERTS to 0.64 on Apple Silicon — the "mmap warm hit is
   a bare load" advantage is a small-working-set artifact, exactly
   where the seed's PARTIALLY REFUTED verdict said the corner was
   unmeasured. The reserve machinery is warm-neutral when off (2%
   ratio-of-ratios, both archs). The small-set gap is now attributed
   (spike profile, `benches/plans/mmap_warm_path.md` escalation run):
   fold floor 41.5 ns/op vs ~41 ns machinery — get body, seqlock probe,
   loom-guarded epoch publish. A lock-free file-liveness mirror removes
   the last mutex from the warm path (semantically clean, ~4 ns
   single-threaded; its real value is multi-reader contention), inline
   hints measurably regress, and closing further is protocol work whose
   worth the TLB result bounds. The granule sweep then dissolved the
   question (`benches/plans/mmap_tlb_pressure.md` sweep + THP tables):
   the warm tax is an approximately FIXED ABSOLUTE cost per protected
   access — the fixed-cost model R = 1 + (R_4K − 1)/scale is validated
   at three points to within 0.01. At 64 KiB granules the small-set win
   clears its confidence interval (0.98, ci95 0.992); the 256 MiB
   result is parity within uncertainty (ci95 1.002). The remainder is
   THP, now attributed by smaps (arena 100% PMD-mapped, file mapping
   4 KiB) and by MADV_NOHUGEPAGE differential: the 64 KiB second effect
   vanishes to the model's prediction without THP, and the shipped
   `advise_hugepage` is worth 30 ratio points at 4 KiB/256 MiB (1.48
   without it). The granule is sira's settled M001 decision with cold W > 1
   amplification on the other side; these numbers feed the scheduled
   T011/T014 re-validation, and sequential composition raises bytes
   consumed per pin without touching the format. Still owned elsewhere:
   the binding
   1.02 block-fetch warm parity (DIO-G1, sira-dios-migration, against
   sira's own mmap block reader with CRC/decode amortization).
9. Not yet measured: total CPU per op (all brackets are wall-clock;
   the darwin-hybrid reopening bar and falsifier 3's tail-latency axis
   both eventually need the CPU ledger), tail latency (falsifier 3),
   the binding block-fetch warm parity (insight 8), and any device
   slower than local NVMe (falsifier 1's latency-class split).

## Requirements direction

- One new I/O-owning surface: `Pool::prefetch(pages: &[PageId]) ->
  PrefetchReport` (or an equivalently thin window form). Fire-and-forget:
  no ticket, no redemption. Consumption arrives exclusively through
  `get`/`ready`; abandonment is handled by staged-unconsumed frames
  being first in line for eviction. If a real cancellation consumer
  emerges, a ticket can be added later; it cannot be cheaply removed.
- Prefetch owns its admission path end to end (insight 4): reserve-
  sourced frames, window-batched control-lock and wake, never the
  demand claim path (insight 2) and never a spin on `Busy` — under
  exhausted credits the answer is `deferred` in the report.
- `PrefetchReport` is per-window feedback, not ownership: counts of
  `{requested, resident, pending, admitted, deferred}` mapped onto the
  admission vocabulary (the darwin-hybrid vocabulary survives its no-go;
  the AIO lane does not). Whether a distinct `rejected` (permanent)
  beside `deferred` (retriable) carries its weight is a design.md
  decision. The consumer expresses opportunity (lookahead distance);
  the pool alone enforces outstanding speculative I/O — the two depths
  are never conflated. Sizing note from insight 3: headroom beyond the
  consumer's lookahead buys nothing — the plateau is at equality.
- A non-touching batch residency probe (internal-first): trim-on-resident
  needs residency observation without the recency touch and pin a `get`
  implies. Insight 5's fault-around result is the same lesson at the
  kernel level: untargeted prefetch of a scatter shape wastes the
  device — trimming needs cheap residency truth.
- Naming: the read-side surface is `prefetch`, never `stage`/`staging` —
  that vocabulary is settled for the write plane (`PoolWriteArena`/
  `PoolWriteSlot`, dios-v1 design.md submit-validation contracts).
- No `ReaderCtx` parameter: prefetch requests residency, not a pin;
  per-reader accounting stays on the demand path.
- Admission is a credit, not a frame class
  (`resources/landscape.md`): a fixed `prefetch_headroom` budget of
  speculative obligations, where one obligation spans in-flight through
  resident-unconsumed. Hard invariant: `prefetch_inflight +
  prefetch_resident_unconsumed <= prefetch_headroom` (set at
  `PoolBuilder`, default 0 = feature off). Consumption promotes the
  frame to demand-class bookkeeping and returns the credit — speculation
  is bounded by accounting, the cache is never partitioned. A demand
  `get` joining a still-in-flight prefetch promotes at join time, not at
  CQE. Credits must compose with the INV-9 watermark (a new invariant,
  stated before code). Unconsumed speculative frames form the
  evict-first class (Leap `PrefetchFifoLruList` precedent); per the
  pinned-frame-retention precedent this is orthogonal metadata, never a
  new `FrameState` variant. Frame binding happens at submission
  (O_DIRECT needs the aligned destination at submit; late binding
  rejected).
- Reserve replenishment is a bounded top-up step inside the existing
  `poll` pass — matured frames first, victim production capped at the
  uncovered deficit; no background threads (the spike's exact shape,
  proven by row 5; consistent with LeanStore's move to cooperative
  eviction, `resources/landscape-audit.md`).
- Accuracy feedback lives in the consumer-owned policy, not the pool:
  extend the eviction/rejection event channel with staged-consumed vs
  staged-evicted-unconsumed (feeds the cache-semantics-injection
  policy seam). Adaptive lookahead distance is consumer-side.
- General-purpose rule: no plan semantics cross the seam. The surface
  speaks `PageId`s in submission order; window construction, per-run
  sequential ordering, trim-on-resident, and depth adaptation are the
  consumer's job. No consumer type or term appears in the public
  surface.

## Constraints

- Registered-buffer pools are memlock-capped (insight 7): ~2,000 4-KiB
  frames per process at the pinned box's default limits. An
  unregistered-buffer fallback (`resources/landscape.md` §12) would
  lift the cap, but registered `READ_FIXED` is settled dios-v1
  architecture with a bench-recorded +11% — making registration a
  layer is an owner decision against that scope, flagged here, not
  adopted.
- On Darwin the backend is eager-inline: prefetch degrades to a no-op
  or bounded readahead via a blocking pool at most. The darwin-hybrid
  no-go stands; its reopening bar now has measured comparators
  (insights 6 and row 8: any AIO case must beat eager-inline AND a
  ~10× pread pool at equal total CPU). The buffered-vs-F_NOCACHE
  sequential fork (insight 6) is recorded here, resolved by the Darwin
  follow-up, not this scope.

## Dependencies

- `cache-semantics-injection` (draft): owns the policy seam this scope's
  eviction class and accuracy events plug into. Sequence per the seed:
  sira-dios-migration lands first (cold-read entry evidence), then this
  scope, then sira-side plan emission.
- `sira-aligned-buffers` (ACTIVE at
  `~/projects/sira/scopes/active/sira-aligned-buffers`; R7 results in
  dios `scopes/active/dios-v1/resources/remix-dios-native-experiment.md`,
  T018/SAB006 in progress): the
  consumer-side realization arrived — SAP1 4 KiB aligned prefix frames,
  REMIX storing file coordinates never pool identities (this scope's
  seam rule enacted as SAB-D3), bounded-set lease amortization
  (SAB-D5). First real-surface verdict, split exactly on the model's
  fault line: n=10 windows BEAT current mmap (0.9609, ci95 0.9615,
  PASS 1.02 — mean 1.39 frames/window, the span prediction confirmed);
  n=1 points FAIL — and R7 sharpened why: the improved REMIX with
  dios-shaped native locators (run, frame ordinal, entry offset —
  alignment as enabler, locator as driver) halved the mmap baseline
  itself (519 → 245 ns/read; plan-layer metadata accrues to every
  byte source), so dios's fixed work
  (~71 ns/query, best-hint 52 ns) exploded relatively (best arm 1.20
  vs 1.02). The affine split (18.5 ns saved per returned value) makes
  n=10 win and n=1 lose from one equation. The n=1 pass budget is
  ~24 ns fixed/query: the atomic protocol (F ≈ 31, preserved at
  `.worktrees/experiment-read-protocol-tightened`, adoption gates in
  `scopes/draft/read-protocol-atomic/`) is necessary-not-sufficient;
  per the floor audit, an n=1 PASS needs cross-query retention of
  winner frames (pinned-frame-retention's surface) or further
  displacement — T018/SAB006's live question. Their records cite this
  scope's resources as protocol rationale; evidence chains are mutual.

## Falsifier registry (inherited from the seed, statuses per evidence)

1. Cold fragmented-overlap margin — MEASURED on local NVMe, three ways
   (rows 3, 5, 8): 5.5× API-free, 8.9× reserve-backed under pressure,
   6–12× vs mmap. Remaining: the latency-class split (slower devices)
   and the implementation gate re-run through the real `prefetch`
   surface.
2. Warm no-regression — PARTIALLY MEASURED (rows 10–11): the isolated
   residency-overhead component is bounded (1.7–2.1× small-set,
   1.18×/0.64× under dTLB pressure) and the reserve machinery is
   warm-neutral when off. Remaining: the real `prefetch` surface's
   warm cost with headroom > 0, and the binding block-fetch parity
   (DIO-G1, sira-dios-migration side).
3. Tail latency — OPEN: p99/p999 beside the throughput margin; the
   synchronous cursor is the consumer a fat submission tail hurts.
   Nothing measured yet (insight 8).
4. Pollution bound — OPEN, mechanism now specified: staged-evicted-
   unconsumed attributable via the policy event channel and bounded by
   the hard credit cap; kill if demand-hot eviction by staging is
   observable at the point canary.
5. Window sweet spot — PARTIALLY MEASURED (row 5): the reserve curve
   plateaus at lookahead, fixing the headroom knob's shape. The
   consumer-side adaptive-distance optimum remains open.
6. Sync-op exclusion — OPEN by construction: any op that cannot
   complete async stays off the staged hot path (fsync, fallback
   lanes).

## Verification posture

- The existence bench (`benches/prefetch_window.rs`) stays green as the
  API-free baseline. The scope adds the `prefetch`-surface twin and the
  pressured-regime bench; that gate flipping FAIL → PASS through the
  real surface is the scope's headline gate (the spike already proved
  the mechanism flips it — row 5).
- The spike prototype (`spike/prefetch-reserve` worktree:
  `SpeculativeReserve` in `Control`, reserve-skipping
  `first_free_frame`, `top_up_reserve` in `poll`, single-page
  `Pool::prefetch`) is throwaway evidence, not the implementation —
  the real surface is TDD from scratch. Credit accounting is exact,
  never heuristic (`resources/landscape-audit.md`): no repair logic
  like the spike's reset-on-full; credits change only at explicit
  transitions (reserve acquired → speculative in-flight →
  promoted-on-consume | evicted-unused | failed-submission-returned),
  each terminal path returning its credit exactly once —
  assertion-paired across submission and completion.
- Property-based operation sequences against `MockDriver` (prefetch
  racing demand `get`s, drop-vs-consume orderings, fault injection on
  staged reads) with the seed-to-regression convention — the tree has
  loom for interleavings but nothing generating operation sequences;
  `io_events_in_order` pins the one-submission-batch-per-window claim.

## Not in scope

- Plan emission (sira-side), REMIX/winner-span vocabulary of any kind.
- A `StageTicket`/redemption surface (rejected above; revisit only with
  a named cancellation consumer).
- Kernel-selected provided-buffer rings (a page's read must land in the
  frame that represents that page) and late frame binding.
- A Darwin AIO lane (no-go stands) and the Darwin buffered-vs-F_NOCACHE
  fork (owned by the Darwin follow-up).
- The wholesale reclamation redesign — watermark inventory ahead of
  demand claims, epochs off the admission path (killed by row 5;
  reopen only if the real implementation's pressured gate fails).
- An unregistered-buffer registration layer (owner decision against
  dios-v1, flagged in Constraints).
- Crate split (settled: general-purpose in API, single-crate in
  packaging; the `DriverCore<E>` boundary preserves the option).
