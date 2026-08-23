# Bench Plan: pinned-frame retention

This plan freezes the T1 performance contract before retention product code or
benchmark code exists. Timing gates use the shared compare harness; benchmark
binaries only emit measurements and validity evidence.

## Shared timed-pair gate protocol

| Field | Frozen value |
|---|---|
| Metric and direction | Wall-time nanoseconds per operation, reported as `candidate / baseline`; lower is better. The same useful-byte checksum must match before a timing ratio is admissible. |
| Workload | Each gate below fixes its own workload. Every arm runs on host `nix` with the shipping cfg-selected backend unless its section explicitly fixes another posture. Warmup completes all allocation, I/O, page population, and descriptor construction before timing. |
| Baseline | Source-comparison gates use clean baseline `4264896e7d2e1a2a5d6d71322a46cb7d8a3de7e7`. Within-candidate gates use the same recorded T7 source commit and executable on both sides, changing only the arm configuration named in that gate. A section may explicitly override this identity for the exact-R8 runnable pair. |
| Candidate | A clean committed T7 retention build unless the exact-R8 section fixes its post-AM4 integration identity. Every run records the source commit, executable SHA-256, `Cargo.lock` SHA-256, Rust version, runner SHA-256, arguments, and arena posture before accepting samples. |
| Repetitions | 40 paired repetitions per gate. Rep 0 runs baseline then candidate, rep 1 runs candidate then baseline, and order continues alternating. Iterations per repetition are fixed before the first candidate run and make each side last at least 100 microseconds. |
| Threshold | Every timing gate asserts a one-sided 95% confidence-interval upper bound of the `candidate / baseline` ratio; lower is better, and no lane threshold below exceeds `1.25`. The shared percentile-bootstrap implementation uses 10,000 deterministic resamples. |
| Compare command | Each section gives the exact `mise run gate` command over its validated two-column paired artifact. No in-benchmark or hand-rolled statistic can close a gate. |
| Escalation lever | A failed timing gate blocks the task. Run the flamegraph workflow on that exact lane, apply only the section's pre-recorded lever, and repeat the same frozen workload; threshold relaxation requires an owner revision to this plan. |

Each timing repetition emits a rich row per side. The row includes gate, pair,
order, arm, source and executable identities, iterations, elapsed nanoseconds,
checksum, allocation count, fault deltas, CPU set, NUMA record, page-size state,
arena posture, segment-layout identity, pool capacity, retained-page count,
memlock limits, and `retained_evictions_held`. A lane-specific validity command
must reject malformed or unmatched rows before emitting exactly
`base_ns,candidate_ns` for `mise run gate`.

## Transient guard path A/B gate

| Field | Frozen value |
|---|---|
| Metric and direction | Warm-hit ns/op ratio `candidate / baseline`; lower is better. |
| Pinned workload and host protocol | On `nix`, CPU 0, performance governor, use the shipping registered backend, one real file, a 256-frame pool of 4 KiB frames, and 128 resident pages. Each iteration performs ordinary `Pool::get`, keeps the transient guard live while folding the same 64 descriptor-selected bytes, then drops it. Both arms replay one fixed xorshift-shuffled page order and identical segment layout. |
| Baseline | Clean executable from `4264896e7d2e1a2a5d6d71322a46cb7d8a3de7e7`, using the ordinary transient guard path. |
| Candidate | The recorded T7 executable with `max_retained_frames = 0`; it uses the same ordinary transient guard path and never calls promotion. |
| Repetitions | 40 paired, order-alternated repetitions, with 8,192 warm hits per repetition after resident and page-fault warmup. |
| Threshold | The one-sided 95% CI upper bound of the `candidate / baseline` ns/op ratio must be `<= 1.01`; lower is better. |
| Compare command | `mise run validate-pfr-pairs pfr_transient_guard target/bench-samples/pfr_transient_guard_process.csv target/bench-samples/pfr_transient_guard.csv target/bench-samples/pfr_transient_guard_provenance.json && mise run gate target/bench-samples/pfr_transient_guard.csv 1.01` |
| Escalation lever | Remove retention metadata loads, branches, or cache-line traffic from `begin_pin`, guard minting, dereference, and `release_guard`; the transient API or epoch fence cannot be weakened. |

Both sides record `ru_minflt` and `ru_majflt` through `RUSAGE_THREAD` on the
pinned bench thread. This registered pair requires zero minor and major faults,
zero post-warmup allocations, and equal checksums.

## Nonzero-budget poll boundary A/B gate

| Field | Frozen value |
|---|---|
| Metric and direction | Poll-boundary ns per reclaimed frame ratio `candidate / baseline`; lower is better. |
| Pinned workload and host protocol | On `nix`, CPU 0, performance governor, use one T7 shipping-backend executable and identical registered 4 KiB arena layouts. A 256-frame pool begins each timed repetition with 64 unretained frames already tagged for deterministic maturity. Both arms execute the same bounded poll sequence, reclaim all 64 frames, and verify identical `PollReport::reclaimed_frames`, frame states, and useful checksum work. |
| Baseline | The recorded T7 executable configured with `max_retained_frames = 0`, selecting the wholesale poll bypass. |
| Candidate | The same executable configured with `max_retained_frames = 64`; no frame is retained, so the only intended difference is the nonzero-budget retention check at the poll boundary. |
| Repetitions | 40 paired, order-alternated repetitions; each side performs 256 prepared reclaim batches so a repetition lasts at least 100 microseconds. |
| Threshold | The one-sided 95% CI upper bound of the `candidate / baseline` ratio must be `<= 1.01`; lower is better. |
| Compare command | `mise run validate-pfr-pairs pfr_nonzero_poll target/bench-samples/pfr_nonzero_poll_process.csv target/bench-samples/pfr_nonzero_poll.csv target/bench-samples/pfr_nonzero_poll_provenance.json && mise run gate target/bench-samples/pfr_nonzero_poll.csv 1.01` |
| Escalation lever | Hoist the nonzero-budget decision out of per-frame reclaim work or batch the retention-word inspection without changing AD-4 ownership, epoch maturity, or exact reclaim accounting. |

The candidate must finish every repetition with occupied budget zero and
`retained_evictions_held = 0`. Both sides record thread-local minor and major
fault deltas; this registered pair requires both deltas to be zero.

### Owner-authorized final overhead refinement

The 40-pair v14 nonzero-budget poll run remains a valid performance RED:
`candidate / baseline` geomean `1.0280`, one-sided CI95 upper `1.0896`, failing
the unchanged `<= 1.01` gate. The authorized refinement selects a release drain
only when the current consumer ring slot is published or the terminal pending
hint is set; ring tail alone is not readiness because ticket reservation
precedes slot publication. A HELD last-drop combines pending publication and
the parked-check/wake decision under one generation-mutex acquisition. The
pass-start epoch load occurs only after a drain is selected, and reclaim helpers
may be inlined to remove enabled-but-empty call overhead. The occupied-budget
observation after epoch advancement remains safety-critical and cannot be
removed. The nonzero-budget poll gate stays `<= 1.01`, and the promote/release
and mutex-mediated wake gate stays `<= 1.25`.

## Zero-budget bypass parity A/B gate

| Field | Frozen value |
|---|---|
| Metric and direction | Mixed lifecycle ns/op ratio `candidate / baseline`; lower is better, with exact behavioral parity as a validity condition. |
| Pinned workload and host protocol | On `nix`, CPU 0, performance governor, use the shipping registered backend and identical 128-frame, 4 KiB arena and file-segment layouts. Replay a fixed cycle of 96 warm guarded hits, 16 completed misses, 16 CLOCK evictions, two epoch advances, matured reclaim, and frame reuse. Both arms fold identical 64-byte ranges and must produce the same completion, eviction, reclaim, and checksum records. |
| Baseline | Clean executable from `4264896e7d2e1a2a5d6d71322a46cb7d8a3de7e7`. |
| Candidate | The recorded T7 executable with the default `max_retained_frames = 0`; no retention word, tag array, ring, or file flag is constructed or touched. |
| Repetitions | 40 paired, order-alternated repetitions; each side executes 256 complete fixed cycles after I/O and fault warmup. |
| Threshold | The one-sided 95% CI upper bound of the `candidate / baseline` ratio must be `<= 1.01`; lower is better. |
| Compare command | `mise run validate-pfr-pairs pfr_zero_budget_bypass target/bench-samples/pfr_zero_budget_bypass_process.csv target/bench-samples/pfr_zero_budget_bypass.csv target/bench-samples/pfr_zero_budget_bypass_provenance.json && mise run gate target/bench-samples/pfr_zero_budget_bypass.csv 1.01` |
| Escalation lever | Move the zero-budget decision to pool construction and the outer reclaim boundary until the baseline lifecycle executes no retention allocation, atomic operation, per-frame branch, or wake check. |

The candidate must report all retention counters, including
`retained_evictions_held`, as zero. Both sides record `RUSAGE_THREAD` fault
deltas; the registered pair requires zero minor and major faults.

## Promote/release and mutex-mediated wake_if_parked A/B gate

| Field | Frozen value |
|---|---|
| Metric and direction | Completed ownership-cycle ns/op ratio `candidate / baseline`; lower is better. |
| Pinned workload and host protocol | On `nix`, CPU 0 for the bench thread and CPU 1 for the poller, performance governor, use identical registered 4 KiB arenas and a fixed 64-page resident set. A repetition executes 4,096 ownership cycles and folds the same 64 bytes per cycle. Every 64th cycle drives the selected frame through logical eviction and matured HELD state with the poller parked before release; all other cycles release while Resident. The wake cycles must observe poller acknowledgement, ring drain, frame Free, and one exact `retained_evictions_held` increment. |
| Baseline | The same T7 source identity performs transient guarded ownership and the pre-existing wake/acknowledgement control at the same fixed cycle positions, with identical byte and reclamation work. |
| Candidate | The T7 retention arm promotes the live guard, releases it with one retention-word decrement, and on each prepared HELD last-drop publishes the release ring then traverses mutex-mediated `wake_if_parked`. |
| Repetitions | 40 paired, order-alternated repetitions after the poller, pages, mappings, and allocator are warm. |
| Threshold | The one-sided 95% CI upper bound of the `candidate / baseline` ratio must be `<= 1.25`; lower is better. |
| Compare command | `mise run validate-pfr-pairs pfr_promote_release_wake target/bench-samples/pfr_promote_release_wake_process.csv target/bench-samples/pfr_promote_release_wake.csv target/bench-samples/pfr_promote_release_wake_provenance.json && mise run gate target/bench-samples/pfr_promote_release_wake.csv 1.25` |
| Escalation lever | Profile the exact split between promotion CAS, resident last-drop, HELD ring publication, generation-mutex hold, and eventfd notification; redesign only the dominant retention transition while preserving the mutex-mediated pre-park recheck and wait-free producer. |

In both arms, the CPU-0 bench thread and CPU-1 poller thread each record
separate per-thread `RUSAGE_THREAD` `ru_minflt` and `ru_majflt` deltas. This
registered pair requires every timed thread to have zero minor and zero major
faults. Any poller major fault invalidates the pair. A repetition is invalid if
a required HELD transition, parked wake, ring drain, or exact reclaim count is
absent.

## Same-frame promotion refused_contention refusal-rate gate

| Field | Frozen value |
|---|---|
| Metric and direction | Binding metric: the `refused_contention` refusal rate, refused promotions divided by attempted promotions; lower is better. Normalized candidate / baseline timing is diagnostic only and cannot hide a refusal-rate failure. |
| Pinned workload and host protocol | On `nix`, pin eight workers to CPUs `0-3,32-35` in one CCX, set the performance governor, and use one shipping-backend pool with one warm resident 4 KiB frame, eight `ReaderCtx` values, `max_concurrent_readers = 8`, and retention budget 1. After a barrier, each worker repeatedly gets, promotes, consumes one fixed 64-byte range, and immediately releases the same frame. Each repetition contains 1,000,000 attempts per worker. |
| Baseline | An order-paired one-worker control from the same T7 executable and fixture; it must record zero `refused_contention` and validates that refusal is caused by same-word contention rather than capacity or retirement. |
| Candidate | Eight simultaneous same-frame promoters using the production retry bound `max_concurrent_readers + 1`. Only the delta of `refused_contention` is counted; budget, ceiling, and retiring refusals invalidate the repetition. |
| Repetitions | 40 paired, order-alternated fresh-process repetitions; control and contended order alternates each pair. The binding CI is computed across the 40 per-process candidate refusal rates with 10,000 deterministic bootstrap resamples. |
| Threshold | The one-sided 95% CI upper bound of the `refused_contention` refusal rate must be `<= 0.5%`. |
| Compare command | `mise run refusal-gate target/bench-samples/pfr_same_frame_promotion_process.csv 0.005 10000` |
| Escalation lever | Preserve safe refusal, then replace the same-frame budget-reservation collision or CAS retry policy with a bounded design and rerun its deterministic exhaustion test and Loom schedules before repeating this unchanged rate gate. |

Both control and contended arms keep one setup-anchor `RetainedFrame` on the
same frame outside sampled attempted promotions and timing. Sampled promotions
therefore start from count > 0 and cannot enter the first-reservation or
budget-refusal path. The anchor is excluded from attempted-promotion and timing
counts and dropped after sampling. Both arms keep retention budget 1 and
require `refused_budget`, `refused_ceiling`, and `refused_retiring` to remain
zero.

Every worker records `ru_minflt` and `ru_majflt` with `RUSAGE_THREAD`; the
process row retains each thread's counters rather than process-wide totals. Any
major fault invalidates the repetition, and this registered arm also requires
zero minor faults. The deterministic retry-exhaustion path remains a unit test,
not a way to populate this benchmark.

## Primary R8 8,035-page retained-session gate

| Field | Frozen value |
|---|---|
| Metric and direction | Successive-read ns/value ratio `candidate / baseline`; lower is better. |
| Pinned workload and host protocol | The shipping backend runs on `nix`, CPU 0, performance governor, with 4 KiB frames. Setup promotes 8,035 distinct pages once and freezes the session before timing. A fixed sequence of 8,192 descriptors visits every page once, then repeats the first 157 pages; each descriptor selects one deterministic 32-byte range. |
| Baseline | The shared post-AM4 executable frozen below selects transient guarded access: each descriptor indexes the frozen page table, calls ordinary `Pool::get`, folds its selected bytes while the guard is live, and drops the guard. |
| Candidate | The same shared post-AM4 executable directly indexes `RetainedFrame` bytes through the frozen owner table. The timed access performs no `get`, epoch pin, promotion, residency validation, CLOCK touch, retention atomic, allocation, or copy. |
| Repetitions | 40 paired, order-alternated repetitions after setup, page population, descriptor validation, and PTE warmup. Each fresh process retains the same descriptor sequence and fixture identity. |
| Threshold | The one-sided 95% CI upper bound of the `candidate / baseline` ratio must be `<= 0.95`; lower is better. |
| Compare command | `mise run validate-pfr-pairs pfr_r8_retained_session target/bench-samples/pfr_r8_retained_session_process.csv target/bench-samples/pfr_r8_retained_session.csv target/bench-samples/pfr_r8_retained_session_provenance.json && mise run gate target/bench-samples/pfr_r8_retained_session.csv 0.95` |
| Escalation lever | Profile the candidate timed loop and remove only residual descriptor, bounds, or dereference work. Do not weaken lifetime ownership, reintroduce persistent frame identities, relax the threshold, or substitute a mock or smaller workload. |

Promotion remains outside the timed region. Fixed-capacity integer descriptor
composition remains outside the timed region. Candidate and baseline use
identical descriptor/access order. Candidate and baseline perform identical
useful-byte work and must produce the same checksum. Descriptor bounds are
validated during setup; timed code performs no descriptor construction.

The retained owner table has capacity 8,035 and is built all-or-nothing. The
pool has 8,059 frames, leaving 24 spare. Each process records the pool capacity,
8,035 retained pages, 4 KiB frame size, memlock limits, arena registration
posture, page-size state, segment-layout identity, and the before/after value of
`retained_evictions_held`.

## Exact-R8 T8b executable identity

The exact-R8 T8b runnable pair uses one clean post-AM4 integration commit; its
full Git SHA and executable SHA-256 are fixed before repetition 0 and must match
every row. Baseline and candidate use one shared executable,
`pfr_r8_retained_session`, with only the arm selector changing the timed access
path. The T7 retention commit is recorded separately only as the retention-code
base/provenance; it is not the runnable T8b identity.

## Exact-R8 Unregistered arena-modernization:AM4 posture

The exact-R8 8,035-page arm uses the Unregistered posture. Its shipping identity
is 8,035 retained pages in an 8,059-frame pool with 24 spare and 4 KiB frames.
The exact-R8 8,035-page run is blocked until `arena-modernization:AM4` supplies
the probed Unregistered arena and shipping read fallback.

The baseline host posture is the stock 8 MiB soft and hard unprivileged memlock
limit supplied by systemd `DefaultLimitMEMLOCK`; the kernel's 64 KiB default is
not the supported baseline and the process does not raise either limit. The run
records both observed limits, Unregistered arena identity, pool and retained
counts, and `retained_evictions_held` before accepting samples.

The registered raised-limit alternative is separately labelled in its own
section. It cannot substitute for the exact-R8 Unregistered gate. The scaled
capacity alternative is non-equivalent and separately labelled in its own
section; it cannot be cited as exact R8 evidence.

This exact scale names the R8-shaped Dios access workload. It is not a
reproduction of the frozen Sira end-to-end comparison unless the Sira source,
corpus, verification mode, and segment layout also match the recorded R8
provenance.

## Opt-in registered raised-limit exact-scale characterization

This is a separately labelled opt-in characterization, not the shipping
baseline and not authority to close the Unregistered gate. It keeps 8,035
retained pages, 8,059 total 4 KiB frames, 24 spare, the primary descriptor
sequence, and both pair arms unchanged, but uses a fully registered arena after
setting and recording 64 MiB soft and hard memlock limits. Forty paired,
order-alternated repetitions follow the shared fault and layout controls. The
registered arm requires zero minor and major faults and records the complete
arena charge, `VmPin`, write-arena size, registration result, and
`retained_evictions_held`.

## Separately labelled scaled capacity characterization

The scaled arm uses 1,024 retained pages, 1,048 total 4 KiB frames, and 24 spare
under the stock unprivileged 8 MiB memlock limits. It is non-equivalent to exact
R8 because it changes the retained-set size and translation regime. If run, it
uses 40 paired, order-alternated repetitions, identical descriptors and useful
bytes within each pair, the shipping registration posture recorded by the
manifest, and all shared fault controls. Its result is a capacity-curve point;
it cannot substitute for or extrapolate to the 8,035-page gate.

## Fault, warm-state, topology, and layout validity

Each side of every timed pair records both `ru_minflt` and `ru_majflt` using
`RUSAGE_THREAD` on the pinned bench thread. Multi-worker rows record the same
thread-local counters for every pinned worker. Any nonzero major fault
invalidates every pair under every arena posture. Registered and exact-R8 arms
must require zero minor faults on both sides; other Unregistered arms retain the
minor-fault count as load-bearing warmth evidence rather than silently treating
page-cache residency as proof of PTE warmth.

Both arms use an identical segment layout within a pair. The manifest records
the layout name, corpus SHA-256, file offsets, page-size state, and arena VMA
range. Sira's SAP1 aligned-frame superblock layout is a new baseline and never
an R8 reproduction because it postdates the frozen provenance source bases.

Every run records the NUMA node count and memory-node placement,
`/proc/sys/kernel/numa_balancing`, CPU topology, CPU affinity, governor, kernel,
storage identity, arena base and span, and each side's page-size state. The
runner verifies the requested affinity and governor before timing; it does not
infer topology from the Threadripper model name.

## Sira-side retained-vs-locator companion obligation

Dios records but does not author or execute this cross-repository companion
obligation. The Sira-side `retained-vs-locator` tie gate `<= 1.02` uses a
one-sided 95% CI upper bound of retained / locator ns/value; lower is better,
with at least 30 paired, order-alternated fresh-process repetitions on `nix`.
Its Sira-side compare command remains
`mise run gate target/bench-samples/sira_pfr_retained_vs_locator.csv 1.02`, and
its escalation lever is limited to verifying equal-regime controls or changing
the recorded memory-page-size regime rather than adding access-path work.

The locator mmap is prefaulted with `MADV_POPULATE_READ` and verified resident
before timing. Both arms use identical `BlockVerification`, corpus, segment
layout, descriptor/access order, useful-byte work, and page-size state. Both
sides record thread-local minor and major faults and require zero of each.

At an equal regime of useful bytes, segment layout, and page-size state, a
sub-1.0 result is a measurement defect: retained access cannot undercut the
same warm virtual-memory load merely by deleting software. A difference in the
recorded memory-page-size state is the only interpretable lever for a sub-1.0
ratio.

## R8 formal evidence retained

`resources/r8-resident-set.md` preserves and remains authoritative for the R8
formal FAIL: resident-set / locator-range measured paired-log geomean `1.0223`
and one-sided CI95 upper `1.0242`, failing the frozen `<= 1.02` gate. This plan
does not relax, supersede, or relabel that verdict. The same resource records
the mock-only prototype, 30-pair provenance, 8,035 of 8,059 retained frames,
and why that mechanism is rejected.

## DRP010 HELD/count state seam

Baseline identity `4264896e7d2e1a2a5d6d71322a46cb7d8a3de7e7` is DRP010-complete.
Retention adds a separate `AtomicU32` HELD/count word orthogonal to the DRP010
packed frame-state/generation `AtomicU64`. HELD never becomes a `FrameState` bit
and the existing `Free`, `InFlight`, `Resident`, and `Evicting` cycle remains
unchanged. The plan's `retained_evictions_held` field counts only matured
Evicting frames whose physical reuse the separate retention word defers.

## Exploratory THP MADV_HUGEPAGE arm

This arm is exploratory and non-gating. It cannot close or substitute for the
exact-R8 shipping gate. It repeats an otherwise identical pair after applying
`MADV_HUGEPAGE` to a 2 MiB-aligned anonymous arena, populating every measured
page, and verifying the expected `AnonHugePages` and page-size state before any
registration. A registered variant must populate and verify hugepage backing
first because pinning freezes page size. The manifest records policy, defrag
mode, arena alignment, population result, and both pair sides' page-size state.

## Exploratory sparse-registration arm

This arm is exploratory and non-gating. It cannot close or substitute for the
exact-R8 Unregistered gate. It fixes the registration table at ring creation,
registers a hot sub-arena in 2 MiB granules within the stock 8 MiB budget, and
records each populated slot, update result, pinned bytes, `VmPin`, fault counts,
and mixed page-size state. It remains separately labelled because a mixed
pinned/unpinned arena is not comparable to either fully registered or
Unregistered posture.
