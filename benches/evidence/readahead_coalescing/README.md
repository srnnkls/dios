# RC1 harness preparation and producer contract

The [RC6 results](results.md) retain the measured headline passes, original DRP
failure, direct diagnostic and passing owner-authorized confirmation. Adoption
remains blocked by candidate cold CPU attribution. This capture contract includes
the recorded owner clarification for unavailable frozen-arm mechanism fields.

This directory records harness preparation, not a coalesced product gate pass.
The owner-selected default remains 524,288 bytes. The retained 1 MiB budget
probe remains separate evidence. RC-G1 failure invokes pool-layer profiling;
this harness does not increase the budget or change a bound.

## Exact executable arms

`scan_collect.py --experiment coalescing` produces these six comparisons.
`geometry` is the existing frozen executable's 8 MiB cold-arena selector;
`pressure` uses the existing 64 MiB arena and private 128 MiB memory limit.
All rows retain independent 4 KiB pages, ordinary `get`/`ready` consumption,
the same poll/consume loop, and the full-page u64 fold.

| Case suffix, for cold and pressure | Frozen base | Candidate | Bound |
|---|---|---|---:|
| `mmap` | `SHAPE:mmap:4:0:0` | `SHAPE:automatic:4:128:256` | 1.00 |
| `current-old-budget` | `SHAPE:automatic:4:32:64` | `SHAPE:automatic:4:128:256` | 0.80 |
| `current-new-budget` | `SHAPE:automatic:4:128:256` | `SHAPE:automatic:4:128:256` | 0.80 |

The frozen arm runs `scan-sample` on the immutable executable. The candidate
runs `scan-default-sample`, which does not call `prefetch_headroom`. The
configuration's credit count is an expected observed capacity, not an
override. Thus the current prerequisite product's 32-page default cannot
accidentally qualify as the 128-page candidate. Each case and raw reference
records its executable digest, runner digest, configuration, and
`credit_selection`; equal configuration strings do not merge identities.

Read limits are 64 for the old control and 256 for both new-budget arms;
miss headroom is three times the read limit. The new candidate's minimum
frame count is 897; both cold pool controls and the candidate allocate
2,048 payload frames. Frozen DRP remains in `benches/read_path_product.rs`
at `max_inflight_reads(1)` and does not use the scan builder.

The explicit scan arm now advances by `min(C, 32, max(1, 128 KiB / granule))`
pages after an accepted hint call. Each call repeats the still-useful prefix
through its new frontier. Deferred calls retry that frontier. This gives
32-page extensions for `geometry:explicit:4:128:256`, while singleton and
large-granule controls preserve a one-page extension. No automatic-arm
poll/consume cadence changes accompany it.

## Frozen source and collection

The pre-dispatch authority is
`target/readahead-coalescing/preflight/preflight.json`. Its retained source
archive, starting-tree archive and frozen executable are checked before use.
Byte-identical copies of the three local preflight inputs are retained in
[`baseline/`](baseline/); the collector defaults to that durable copy so
the ordinary remote sync's `target/` exclusion cannot drop its inputs.
The archived product files must match the approved starting product, and the
executable's compiled runner digest must match its retained runner sources.
Neither frozen comparison is rebuilt from the concurrently changing tree.

The collector copies the frozen executable, both frozen source snapshots,
and the preflight record into `CAMPAIGN/frozen/`. It separately retains the
candidate executable and a source archive, checking source stability across
the candidate build and collection. A supplied candidate `--binary` also
requires `--candidate-manifest` proving the same source and executable hashes.
The default invocation builds the candidate inside this capture boundary.

Run on the pinned host, from the repository root, after RC3/RC4 supply the
observation producer and the product safety checks pass:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 benches/mmap_workloads/scan_collect.py \
  /dev/shm/dios-rc-primary --input /path/to/immutable-fixture \
  --experiment coalescing --mode run
```

Each comparison has two qualification pairs and 30 alternating fresh-process
pairs. `paired.csv` and `cpu/paired.csv` are reconstructed exactly from hashed
raw records. The shared `summarize` command produces both statistics; shared
`mise run gate CASE/paired.csv BOUND` asserts RC-G1/RC-G2. Observation controls
add both elapsed and CPU gates at 1.05. Failures and raw samples remain on
disk. A selected subset via `--cases` is a partial campaign, not full adoption.

For a paired default observer control, use `--experiment observe
--credit-selection default --configs geometry:automatic:4:128:256` with the
same retained candidate executable. Use ordinary `override` for explicit
controls. The old frozen binary remains usable for external, qualified
observations; it cannot be rebuilt merely to add telemetry.

`scan_profile.py PRIMARY OUTPUT CONFIG --executable-role frozen|candidate`
selects the exact measured executable, including when both roles share a
configuration string. Candidate default profiles use `scan-default-profile`.
Completed pairs from a failed gate remain eligible for profiling. Profiles
and analysis preserve executable identity in addition to configuration.
CPU attribution with unresolved stacks remains unqualified; no missing
category cost is obtained by subtraction from an assumed coefficient.

## Product observation seam for RC3 and RC4

RC1 deliberately emits `coalescing: null`. Existing `PrefetchStats` cannot
observe actual SQEs, vector lengths, control visits or EOF bytes. The collector
rejects an RC gate row without the measurements below. It never fills these
fields from configured credits, vector width, request count, or prefetch
admission totals. This is an intentional dependency on the later product
tasks, not a product result.

RC3 needs bench-only, preallocated capture at these producer sites:

- `src/pool/mod.rs`: accepted demand submission, span dispatch/publication,
  continuation and failure/EOF terminal paths, with operation token,
  generation, file/page range and pass attribution.
- `src/pool/prefetch.rs`: committed explicit/automatic runs and each explicit
  call's examined prefix, protected-page resolutions, replacement visits,
  admission work and CLOCK work.
- The RC3 span route/slab module: descriptor/route allocation bytes,
  continuation lengths, complete-page publications, failures, and terminal
  destination/read-credit ownership. Count accepted backend submissions and
  observed CQEs, including transient retries and demand READs; do not count a
  reservation or refused submission as an SQE.

RC4 needs capture at these producer sites:

- `src/pool/prefetch/pattern.rs` and `progress_prefetch`/`prefetch_next`:
  confirmed width reaching 32, committed run/refill events, and the
  round-robin turn before and after admission or credit deferral.
- `src/pool/prefetch/state.rs` reconciliation/feedback and obsolete cleanup:
  affected-entry and cleanup visits, including empty polls.
- `src/pool/clock.rs` first speculative-consumption notification and its
  drain: consumption after the last CQE and measured credit recovery.
- Construction of notification/index storage: its actual allocated byte
  count, added to descriptor and route metadata rather than payload bytes.

The bench adapter belongs in `benches/mmap_workloads/scan.rs` and, if needed,
`observe.rs`/`mod.rs`; it must replace the null field with a captured record
after final drain. These paths are the RC3/RC4 integration additions to RC1's
prepared harness. Instrumentation must not add a candidate-only poll, impose
a new consumer cadence, allocate in the timed region, or put request clocks
in primary samples. Detailed replay records have fixed startup capacities,
explicit overflow/loss counters, and paired observer qualification. The
2026-09-14 [owner clarification](owner-dispositions-20260914.md) applies RC-G6
mechanism reporting to every arm that can emit it. Frozen executables predate
this seam: report their CPU/elapsed per useful 4 KiB, polls/page and native
prefetch stats; actual SQEs/CQEs/ranges, EOF/consumer-stop waste and control
entry visits stay null with that limitation beside them. The candidate must
report all fields with qualified attribution. Optional counter-only strace
replay may supply exact frozen SQE totals; discard its timing. No numeric
bound, executable, host-security setting or budget changes.

## Per-scan `coalescing` record

`coalescing.py` is the executable validation contract. Field names below
refer to measured records; they are not a recipe for generating observations.

- `confirmed_window_page`: zero-based useful-page ordinal where the
  automatic window first reaches 32, strictly below 1,024.
- `steady_intervals`: exactly one per pass, with `pass_index`, interior
  `start_page`, `pages: 1024`, positive measured `refills`, and `reads`.
  Each read has `kind: demand|speculative`, page offset and page length.
  Include every actual initial read SQE intersecting the interval, including
  boundary overlaps and demand READs. Demand length is one, speculative
  length is 32, intervals have full read coverage, and total SQEs are at most
  33. Continuations belong in the separately counted I/O summary and fault
  witnesses; this no-hole steady interval does not permit them.
- `explicit_calls`: per-call `examined`, `protection_lookups`,
  `replacement_visits`, `admission_visits`, `clock_visits`. Protection
  resolutions cannot exceed examined pages; replacement visits cannot exceed
  capacity. Admission and CLOCK work are reported separately. A non-automatic
  arm has no automatic steady intervals.
- `io`: nonnegative `read_sqes`, `demand_read_sqes`,
  `speculative_read_sqes`, `continuation_sqes`, `cqes`, `read_bytes`,
  `requested_bytes`, `publications`, `failures`, `terminal_read_credits`,
  `terminal_destinations`, `eof_sqes`, `eof_bytes`, `beyond_stop_sqes`,
  `beyond_stop_bytes`, `short_bytes`, `pending_requests_max`,
  `pending_bytes_max`, and `refills_while_pending`. Initial speculative and
  continuation length histograms are `initial_vector_lengths` and
  `continuation_vector_lengths`, each containing distinct
  `{pages, requests}` rows. Initial demand reads are counted independently.
  `read_sqes` partitions into demand, initial speculative and continuation
  SQEs; CQEs and terminal ownership must fully drain.
- `eof_bytes` counts attempted bytes beyond the actual EOF outcome;
  `beyond_stop_bytes` counts requested bytes beyond the consumer's stop.
  Count every accepted wasted call, including several already accepted
  vectors. `short_bytes` sums positive CQE bytes smaller than their requested
  remainder. `read_bytes` counts positive bytes returned, while
  `requested_bytes` includes repeated continuation/retry request ranges.
- `control`: events with `capacity`, `cause:
  idle|completion|consumption|invalidation`, `entry_visits`,
  `affected_entries` (at most 32) and separately charged `cleanup_entries`.
  Idle visits are zero; changed-run visits cannot exceed affected plus
  explicit cleanup entries. The producer must report each affected run, not
  aggregate unrelated work into an invented 32-entry event. These bounded
  detailed records cover only the configured measurement interval per pass.
- `control_entry_visits_total`: nonnegative integer summing actual entry
  visits over the whole measured scan and final drain, across every pass.
  Accumulate it before filtering detailed `control` records by interval or
  capacity. It must cover at least the visits in those retained records;
  missing or invalid totals reject a full measurement. Event counts and
  configured capacity or width cannot substitute for observed entry visits.
- `metadata_bytes`: actual descriptor, route, lookup and notification
  storage allocated at construction, excluding payload. `overflow` and
  `dropped_events` must both be zero.

`scan_analyze.py` reports SQEs per 1,024 useful pages, both length
histograms, whole-scan `control_entry_visits_total` divided by all useful
pages in the scan, metadata bytes, all waste/overlap
counters, and existing polls/page, elapsed/page and thread-CPU/page. An absent
I/O observation produces null, never a zero or an estimated SQE count.

## Separate mechanism capture

The existing `mmap_workloads` executable accepts the portable command
`mechanism-capture INPUT_FILE NEW_JSON`. Build it with
`cargo build --profile profiling --features bench --bench mmap_workloads`,
or obtain its exact executable path through `collect.build_binary`. Supply
an existing file containing at least 4,096 complete 4 KiB pages. The command
uses real `Pool<Driver>` reads with direct I/O and Unregistered buffers on
eager and Linux. It creates a new JSON file and refuses to overwrite one.
Fixture preparation, report snapshots and serialization are outside the
bounded scenario actions; these captures make no timing claim.

Run `PYTHONDONTWRITEBYTECODE=1 python3 benches/mmap_workloads/coalescing.py
CAPTURE.json` to validate the capture. The document has `schema: 1`, summed
actual `overflow`/`dropped_events` counts, and:

- `small_capacity`: exactly C/R=1/64, 8/64, 32/64, 32/33 and 128/33 at
  granule 4096. Each records `credits`, `read_limit`, `granule`, positive
  `admitted_pages`, matching `point_reads`, zero `vector_reads`, and zero
  `deferred_with_available_credit` attributable to a full-vector barrier.
  Other resource refusals remain ordinary bounded operation errors.
- `shared_reader_turns`: consecutive attempts with stable ready streams
  `[0, 1]`, `stream`, `turn_before`, `turn_after`, `admitted_pages`, and
  `outcome: admitted|credit_deferred`. At least one credit deferral retains
  the turn; both streams subsequently admit, and each commit advances it.
- `control`: the events above, including idle polls at C=32/128/256 and
  a consumption event with `after_last_cqe: true` and positive
  `credits_recovered` without another completion.
- `explicit_calls`: the per-call fields above plus `capacity`, zero
  `protected_evictions`, and `scenario: duplicates|multiple_runs|
  newly_admitted_protected`, with all three scenarios present at full credits.

`raw_scenarios` retains all thirteen bounded scenarios, their actual pool
geometry, native I/O mode, registration posture, consumed page indices,
consumed bytes and wrapping u64 checksum over every little-endian word of
each acquired page. Every stage retains cumulative `PrefetchStats`, the
actual poll count and the complete interval-zero `ReadObservation` snapshot.
`PrefetchStats` reconciles consumption feedback: these named boundaries are
explicit control actions, not passive samples. Each observation has a fixed
65,536-event capacity; truncation remains visible through the loss counters.

The five small-capacity probes explicitly fill C-1 unrelated pages before
the third consecutive cold demand confirms a fresh stream. The
`before_opportunity` stage asserts one free speculative credit, available
read capacity and a reserve frame. Its difference from `after_opportunity`
supplies automatic admissions and actual point/vector attempts, excluding
setup hints. `deferred_with_available_credit` records whether that measured
opportunity admitted no page. The shared-reader scenario confirms both
streams before polling at full C=4 credits, records their credit deferral,
consumes the explicit setup pages and polls again. Its entire consecutive
turn sequence is copied unchanged into `shared_reader_turns`.

Only the dedicated disabled-automatic C=32/128/256 scenarios contribute the
aggregate `control` rows. They drain a 32-page explicit run, consume it and
poll twice with no additional read, retaining equal before/after CQE counts.
Each aggregate explicit call is the last actual call in its named raw
scenario. `before_call` records full occupied credits; `after_call` precedes
drain and consumption. The raw `hint_pages` and partitioned `hint_report`
preserve duplicates, separated missing runs and a newly admitted prefix
repeated around capacity-deferred work.

`wrong_hint_canary` is separate from those exact aggregate label sets. It
uses the existing bounded 64-frame, 60-hot-page, C=4/R=16 protocol: warm the
hot set, issue 25 disjoint four-page wrong windows and check all 60 hot pages
after each window. It retains actual demand-hot hits/misses, per-hint and
poll credit samples, and protected evictions summed from the actual explicit
call records. The `wrong_hints_abandoned` stage precedes file retirement;
`file_retired` retains full speculative/read recovery and actual terminal
ownership. Run thirty fresh processes on each backend for RC-G5 and require
1,500 hot hits, zero hot misses/protected evictions, bounded flight samples,
nonzero abandoned occupancy, zero terminal ownership and full recovery in
every capture. Preserve these mechanism witnesses separately from Linux
wrong-hint latency and I/O-amplification characterization.

Mechanism captures must be retained with their source/executable and raw
hashes by RC5/RC6. This standalone validator checks the observed mechanism
contract; it does not run the scenario, authenticate a synthetic document,
or claim a performance gate passed.

## Pre-registered DRP-G4 confirmation, 2026-09-14

Registered at `2026-09-14T15:59:37.773491+00:00`, before any confirmation process runs.
The [declaration](drp-confirmation-registration.json) has SHA-256
`6c080048d9016cbc68cd858a40dcdcb0e850a066eb689598b4a72a0485156af9` and is copied into the RC6 checkpoint.
The owner withdrew the earlier no-large-campaign instruction after the direct
pairing dismissed the suspected branch regression.

- Lane: `drp_g4_ordinary_base_8t`; 2 qualification pairs followed by exactly
  400 alternating fresh-process measured pairs, one campaign only.
- Frozen base executable: `0b49dc7dbe86d89c5390f94d7462818308104b6ce0d2f4ac8d001f9a5b250cfd`.
- Candidate executable: `ae50a099a18f7a508d17d605118801f06ebdf22e9f89a5833b02772ffd5f0529`.
- Same frozen runner/build, 32,768 full-4-KiB folds, worker CPUs `0-3,32-35`,
  controller CPU 4, order alternation, fixture, host and resident-prefill protocol.
- Candidate/base, lower is better; unchanged bound **1.00**, asserted with
  `mise run gate measurement/paired.csv 1.00` on these 400 pairs alone.
- Owner planning expectation: geomean approximately **0.92**; at sd(log ratio)
  **0.238**, one-sided 95% log half-width approximately **0.020**.
- **Adopt the result whichever way it goes. No second attempt if it fails.**
  Retain the failed 30-pair campaign separately; never pool it with this sample.

The frozen DRP converter hardcodes 30 pairs. Preserve all 400 raw pairs and
apply the existing direct-campaign raw identity/work validation, then the
existing shared comparison writer, summarizer and gate. No runner/converter
source or measured executable is rebuilt to collect this campaign.

## Confirmation outcome and owner reporting disposition

The one campaign completed 2 qualification and 400 measured pairs. The shared
gate passed remotely and locally: candidate/base geomean **0.9776689295560357**,
one-sided 95% upper **0.9980337616928986**, unchanged bound **1.00**. Adopt this
result as declared. The failed 30-pair archive remains unchanged and separate;
no samples were pooled. Raw/CSV reconstruction, source/executable identities,
pre-registration copies and the zero-sample preflight correction are retained in
[owner-disposition-verification.json](owner-disposition-verification.json).

The owner requested the same-cutoff mode accounting for the confirmation.
Candidate landed in the slow group in **245/400 processes (61.25%)**, versus
base **146/400 (36.50%)**. Fast/slow within-group geomean ratios are
**0.867721 / 0.928769**. Counts, arithmetic and geometric means, all 800 elapsed
values, unchanged cutoffs and raw identities are retained in [drp-modes.json](drp-modes.json)
and [results.md](results.md). The owner records startup/placement asymmetry on
the approximately 3 ms lane, not higher hot-path cost; the rows do not directly
observe scheduling. The separate [protocol follow-up](../../plans/dios_r1_r7_read_performance.md#owner-follow-up-drp-g4-resolution)
carries a longer timed region or one worker per physical core into future scope
planning. No gate sample or adopted result changes.

Frozen arms retain native CPU/elapsed, polls and prefetch statistics; the three
unavailable mechanism groups remain null with their owner-approved limitation.
The optional strace replay yielded **17,123 whole-process submitted SQEs** across
1,215 syscall returns. Its timing is discarded and it supplies no timed-region
CQE/range, waste or control attribution.

Candidate cold CPU attribution remains unqualified: the latest single CPU-clock
capture has **249 timed / 366 orphaned samples**, with zero loss/throttling. All
four cold-profile attempts are retained in the [results](results.md); their CPU
category estimates stay null under the unchanged qualification rule. Candidate
pressure attribution and all measured mechanism fields remain qualified. The
single targeted Astra review at high
effort cleared B5-P1 and B5-P2; it does not clear the cold attribution gap.

An exact-symbol replay of the retained CPU-clock data reproduced the same
249 timed / 366 orphaned result. All sampled ELF mappings were made available in
a task-owned symbol tree, including the SHA-pinned candidate and matching libc
build ID. The same classifier and threshold still withhold CPU category costs.
The [replay record](cold-unwind-verification.json) retains the commands, symbols,
raw hash and failed qualification; no new workload capture ran.
The [owner accepts these cold CPU nulls for RC6 closure](owner-cold-attribution-disposition-20260914.md),
retaining the measured cold totals **1,472.5 ns elapsed / 1,464 ns CPU per useful
4 KiB** and qualified pressure attribution. RC-G6 has no numeric adoption bound;
no failed profile becomes qualified and no gate or default budget changes.

The next **readahead-efficiency** scope must obtain qualified cold attribution
before product implementation, using a frame-pointer diagnostic replay recorded
as a **separate perturbed executable with its own identity**. Its bench plan
precedes the replay. This deliverable carries the structural unwinding gap
forward; more repetitions are not its remedy. RC6 adoption is complete under
this disposition. The final [Astra and Opus integration reviews](integration-review.yaml)
passed all five gates at high effort with no findings; scope readiness passes.
