# RC6 measured results — 2026-09-14

**Adopted with the owner-accepted cold CPU attribution limitation.** The six headline comparisons, inherited prefetch gates,
non-sequential controls and both-backend mechanism/wrong-hint witnesses passed at
512 KiB. The owner-authorized, pre-registered DRP-G4 confirmation passed with
**0.977669 geomean and 0.998034 upper** against the unchanged 1.00 bound. The failed
30-pair campaign remains separate. RC-G6 now permits the recorded frozen-arm
nulls. The owner also accepts the candidate cold CPU nulls for RC6 closure;
those profiles remain unqualified. No gate bound,
default budget, workload or candidate poll cadence changed.

## Headline comparisons

Each row has two qualification pairs and 30 alternating fresh-process pairs on
`nix`, CPU 0 worker / CPU 4 controller, Linux 6.6.64, performance governor, THP
never, ext4/Samsung 970 PRO. Ratios are candidate/base, with the shared one-sided
95% upper. Cold pools share an 8 MiB payload arena; pressure pools use 64 MiB
inside a private 128 MiB cap. The candidate uses the actual default C=128/R=256,
with no prefetch override.

| Comparison | Ratio | Upper | Bound | Base elapsed / CPU ns per page | Candidate elapsed / CPU ns per page |
|---|---:|---:|---:|---:|---:|
| cold-mmap | 0.913994 | 0.914949 | 1.00 | 1611.11 / 1528.81 | 1472.54 / 1463.95 |
| cold-current-old-budget | 0.483953 | 0.486081 | 0.80 | 3043.15 / 3027.49 | 1472.59 / 1464.03 |
| cold-current-new-budget | 0.505174 | 0.507839 | 0.80 | 2830.81 / 2815.95 | 1429.83 / 1421.39 |
| pressure-mmap | 0.669666 | 0.673502 | 1.00 | 2467.10 / 2387.96 | 1652.00 / 1640.40 |
| pressure-current-old-budget | 0.509454 | 0.510413 | 0.80 | 3213.23 / 3193.72 | 1636.98 / 1625.33 |
| pressure-current-new-budget | 0.530004 | 0.531184 | 0.80 | 3078.05 / 3059.84 | 1631.40 / 1619.97 |

All inherited gates passed: fragmented explicit 0.105354/0.125539 upper
(bound 0.50), automatic cold/pressure 0.185904/0.202004 (0.80), fragmented/
dependent 1.000464/1.000267 (1.02, zero automatic admissions), and original
frozen explicit cold/pressure 0.528265/0.562080 (0.80).

The retained coarse 256 KiB reference remains approximately 1,163 ns elapsed /
1,158 ns CPU per useful 4 KiB, as labelled in the [plan](../../plans/readahead_coalescing.md).
It is a separate historical granule configuration, not another matched candidate arm.

## DRP and the owner-directed direct pairing

| Original frozen DRP lane | Ratio | Upper | Bound | Result |
|---|---:|---:|---:|---|
| drp_g2_warm_ordinary | 0.992083 | 0.996016 | 1.01 | passed |
| drp_g2_cycling_reuse | 1.003451 | 1.006314 | 1.01 | passed |
| drp_g4_ordinary_base_8t | 1.002841 | 1.074171 | 1.00 | gate_failed |
| drp_g4_ordinary_scaling | 0.315177 | 0.332980 | 0.50 | passed |

Sören requested the direct `5edb6a7 / aa97c82` comparison before any bisect.
It returned **0.918692 geomean, 0.981067 upper**, passing the 1.00 diagnostic
discriminator with two qualification and 30 measured pairs. Both compiled products
have exactly `bench`, identical compiler/profile/rustflags and non-build-script
dependency fingerprints, and the unchanged frozen runner/build script. The lane
retains 32,768 full-page folds and CPUs `0-3,32-35`. The suspected branch regression
was not reproduced; no commit bisect or product modification followed. This
diagnostic does not replace the failed canonical frozen-base gate.

The subsequent [owner disposition](owner-dispositions-20260914.md) authorized
one confirmation against the original frozen base. Its [declaration](drp-confirmation-registration.json)
was recorded in the checkpoint and evidence README at 15:59:37 UTC, before any
benchmark process. The campaign completed **2 qualification + 400 measured pairs**
with the same `0b49dc7d…` base, `ae50a099…` candidate, frozen runner, CPU set,
iterations and order alternation. The shared gate passed remotely and locally:
candidate/base **0.9776689295560357**, one-sided 95% upper **0.9980337616928986**,
bound **1.00**. This result is adopted under the pre-registered rule. The failed
30-pair archive and CSV remain unchanged and were never pooled into this sample.
No second confirmation ran.

The frozen converter accepts exactly 30 pairs. As declared before measurement,
the existing raw validator checked every process row; the existing shared
comparison writer, summarizer and gate processed all 400 pairs. The local check
reconstructed the entire paired CSV exactly. The retained preflight correction
restored missing old scratch metadata before any native benchmark process;
it produced no qualification or measurement samples. Executable, raw, CSV,
archive and pre-registration hashes are in the [verification record](owner-disposition-verification.json).
The future lane-resolution decision is recorded separately in the
[DRP plan](../../plans/dios_r1_r7_read_performance.md#owner-follow-up-drp-g4-resolution).

The [mode record](drp-modes.json) retains all sorted elapsed values, source and
executable identities, explicit descriptive cutoffs, mode counts and ratios.
Canonical base/candidate fast/slow counts are 20/10 and 11/19; the within-group
geomean ratios are 0.9045/0.9233. Direct base/candidate counts are 8/22 and 14/16,
with within-group ratios 0.9790/0.9942. Thus the direct overall improvement also
reflects different group frequencies. No sample is excluded from either gate.
The earlier archives name products `812c812` and `ee7f462`, not `aa97c82`;
their compiled `bench` feature selection remains unverified. Their paired log-ratio
standard deviations are 0.249/0.227 versus 0.238 in RC6. Initial SMT placement is
a hypothesis; these elapsed records do not observe the scheduler.

The owner requested the same descriptive accounting for all 400 confirmation
pairs. The cutoffs are unchanged: base fast below 3.45 ms, candidate fast below
3.30 ms; values at or above the cutoff are slow. Every measurement remains in
the adopted shared gate.

| Confirmation arm | Mode | Processes | Share | Mean ms | Geomean ms |
|---|---|---:|---:|---:|---:|
| base | fast | 254 | 63.50% | 2.764329 | 2.757098 |
| base | slow | 146 | 36.50% | 3.773886 | 3.773278 |
| candidate | fast | 155 | 38.75% | 2.395886 | 2.392393 |
| candidate | slow | 245 | 61.25% | 3.505770 | 3.504503 |

Candidate slow-mode share is **24.75 percentage points higher**. Its within-mode
geomean ratios are **0.867721 fast / 0.928769 slow**, while the adopted whole-sample
ratio is **0.977669**. The owner records this as **startup/placement asymmetry on
an approximately 3 ms lane, not higher hot-path cost**. The elapsed rows establish
the mode-share imbalance and lower within-mode costs; they do not directly
observe scheduling. The [mode record](drp-modes.json) retains all 800 elapsed
values and the raw/archive hashes. The [owner protocol follow-up](../../plans/dios_r1_r7_read_performance.md#owner-follow-up-drp-g4-resolution)
carries forward a longer timed region or one worker per physical core, because
placement variation can fail the next tie under the current protocol. This
descriptive addition changes neither the adopted result nor the frozen protocol.

## Mechanism and attribution

All 30 fresh eager and 30 fresh io_uring captures passed the existing standalone
mechanism validator. Each backend preserved caller-byte checksums, real attempt/CQE
conservation, full explicit occupancy, small-capacity progress, shared-reader turns,
idle/post-final-CQE recovery and terminal zero ownership. Every wrong-hint capture
recorded 1,500 demand-hot hits, zero misses/protected evictions and full recovery.

Captured per-event control visits, event counts and cleanup work are retained in
`results.json`, separately from the whole-run visits/page denominator. Metadata
counts cover read descriptors/routes, speculative indexes and notification storage;
they are additional to payload bytes and exclude the observation log.

Each automatic scan reached full 32-page vectors in its first 1,024 useful pages;
each steady interval met the <=33-SQE requirement. The following values are means
over complete scans; the pending-byte maxima include demand reads as well as
speculation. EOF and consumer-stop quantities count aggregate attempted waste,
not an assumed single tail request.

| Candidate comparison | Polls/page | Control visits/page | SQEs/1,024 pages | EOF SQEs / bytes | Stop SQEs / bytes | Metadata bytes |
|---|---:|---:|---:|---:|---:|---:|
| cold-mmap | 0.3217 | 1.9792 | 32.5063 | 0.00 / 0 | 4.00 / 426394 | 5258296 |
| cold-current-old-budget | 0.3203 | 1.9792 | 32.5083 | 0.00 / 0 | 4.00 / 426530 | 5258296 |
| cold-current-new-budget | 0.3456 | 1.9784 | 32.5104 | 0.00 / 0 | 4.00 / 426667 | 5258296 |
| pressure-mmap | 0.2613 | 1.9852 | 32.4356 | 32.30 / 3032405 | 33.93 / 3077734 | 6179448 |
| pressure-current-old-budget | 0.2443 | 1.9855 | 32.4408 | 32.57 / 3078554 | 34.60 / 3125111 | 6179448 |
| pressure-current-new-budget | 0.2606 | 1.9852 | 32.4368 | 32.00 / 2997726 | 33.90 / 3043601 | 6179448 |

All eight exact-arm trace/plain controls passed both elapsed and CPU upper <=1.05.
Qualified logical pending-read means for old-budget frozen / matched-budget frozen /
candidate are 27.05 / 63.28 / 88.81 cold and 27.85 / 64.42 / 86.51 pressure.
These are pool observations at trace checkpoints, not device queue depth or exact
submission-time overlap. Full control bounds and occupancy records are in
[results.json](results.json).

Only the frozen matched-budget cold profile and candidate pressure profile met the
caller-unwinding requirement. Their estimated prefetch/control CPU costs are
349.27 and 522.76 ns/page; unresolved CPU remains 2,007.66 and 706.79 ns/page.
These estimates use measured unprofiled CPU times qualified sample shares; no
historical coefficient is subtracted. The six unqualified profiles have null
category budgets. All four cold-candidate attempts remain unqualified and retained:

| Sampler | Replays | DWARF stack bytes | Timed samples | Orphaned samples |
|---|---:|---:|---:|---:|
| cycles, period 1,000,000 | 16 | 32768 | 1600 | 20 |
| cycles, period 1,000,000 | 64 | 65528 | 4134 | 5859 |
| cycles, period 1,000,000 | 16 | 16384 | 1023 | 1499 |
| CPU-clock, 997 Hz | 16 | 32768 | 249 | 366 |

All had zero lost/throttle/unthrottle events. Each exceeds the unchanged
`orphaned * 100 <= timed` qualification requirement. The last capture used the
existing CPU-clock sampler and the exact retained candidate; every raw replay
validated, but missing callers prevent any category CPU estimate. Its initial
fixture-directory preflight error occurred before capture and is retained.
No further capture or qualification change followed. The owner accepts this
cold CPU limitation for RC6; all candidate mechanism fields and the pressure
CPU attribution remain present and qualified as previously measured.

The resumed raw-data diagnosis supplied all mapped ELF files explicitly through
`perf script --symfs`, including the unchanged candidate and libc build ID
`34df6094fa79d4d842a69928f98fae25ac983cce`. The unchanged collector/classifier
again produced **249 timed / 366 orphaned** samples. Nearly every saved stack
is present, but the decoder fails to follow libc callers; a pre-exec comm-state
diagnostic produced byte-identical script output. The original capture and its
null estimates remain intact, with the [failed replay evidence](cold-unwind-verification.json)
retained separately. No new capture or classifier/threshold change followed.
The [owner disposition](owner-cold-attribution-disposition-20260914.md) accepts
these cold CPU nulls and closes RC6. Its record retains **1,472.5 ns elapsed /
1,464 ns CPU per useful 4 KiB**, the **249 timed / 366 orphaned** sample counts,
incomplete caller unwinding, and the unchanged 1% orphan rule withholding
category budgets. The qualified pressure attribution remains above. RC-G6 has
no numeric adoption bound; every numeric gate remains unchanged.

The gap carries into **readahead-efficiency**, the owner's next scan scope:
qualified cold attribution is a **pre-implementation deliverable**, obtained
from a frame-pointer diagnostic replay with a **separate perturbed executable
and its own identity**. Its bench plan must precede that work. More repetitions
cannot repair structural unwinding loss. This follow-up is also recorded in the
[bench plan](../../plans/readahead_coalescing.md#owner-adoption-cold-cpu-attribution-limitation).

The immutable frozen runner emits elapsed/CPU, polls, prefetch outcomes and trace
occupancy, but no exact SQE/CQE/range, EOF/stop-waste or control-entry-visit records.
Those three groups stay null with their limitation alongside them in `results.json`.
The [owner-amended contract](README.md#product-observation-seam-for-rc3-and-rc4)
requires mechanism reporting from every arm that can emit it. Frozen CPU/elapsed
appears above; each `headlines[].base_cost` also retains polls/page and all native
prefetch fields: admitted, automatic admitted, capacity, deferred, demand promoted,
evicted unused, failed, occupied, reads in flight, reserve free and submission refused.
The mmap controls have zero polls and no pool-prefetch object.

The optional frozen matched-budget cold `strace -f -e trace=io_uring_enter`
replay succeeded: **17,123 submitted SQEs** summed across **1,215 syscall returns**.
This is an exact whole-process submission total, including setup/teardown, with
timing discarded. It supplies no timed-read/CQE/range, waste or control attribution;
those groups remain null. The command, trace and raw hashes are in the
[verification record](owner-disposition-verification.json). No host security setting changed.

DRP CPU-clock profiles (997 Hz, 16 recorder pages) and separate `perf stat`
completed for both exact canonical arms with zero lost/throttled samples. The two
base replays contain 16/16 worker-stack samples; candidate has 14/12. These are
small, whole-process diagnostic captures including setup/barriers, with no qualified
DRP timed filter or thread-CPU budget. Whole-process cycle counts are 172.6/175.5
million base and 163.9/163.2 million candidate; cache misses are 0.936/0.988 versus
0.781/0.810 million, and context switches 69/65 versus 48/55. Counter running-time
coverage is 100%; these values do not establish timed-region cache causality.

The high-rate cycle captures remain retained: 128/64 recorder pages failed driver
initialization before timing; 16 pages ran but lost samples. No memlock limit or
security setting was changed. The supported lower-frequency sampler provided the
bounded diagnostic result above. Current unprivileged tracing cannot open the
io_uring event directory, and bpftrace is absent; exact missing frozen counters
were not reconstructed from sampled stacks.

## Provenance and validation

The reviewed product is `5edb6a7`; the measured scan snapshot `ad434b21` adds only
the RC6 bench adapter, its test and capture documentation. Its exact executable is
`a10ca92b…af06`; the frozen scan executable is `cc1e1ca4…ff57c`. The 41 product
source/build/lock files match the reviewed RC5 product. Exact hashes, both copies
of each raw archive and byte sizes are in the [archive ledger](archive-ledger.json).
Local retained artifacts are under `target/readahead-coalescing`; the pinned-host
copies are under `/home/srnnkls/build/dios-readahead-coalescing-20260914/target/readahead-coalescing`.

RC5 safety passed: 378 local / 400 Linux native tests, 20 Loom, 55 Miri, 271 eager
/ 284 Linux ASAN checks, lifetime/retention and both-backend zero-allocation paths.
RC6 adapter verification passed 11 Python checks on both hosts, 15 Linux benchmark
contract tests, strict all-target bench/mock Clippy and formatting. Evidence hashes
and all six primary/eight observer raw pair reconstructions were verified locally.
Initial review found a missing wrong-hint canary check in the standalone validator.
The repaired checker passed the 11-case Python suite and [revalidated all 60
unchanged captures](validator-revalidation.json), rejecting both reproduced falsifiers.
That record pins the new validator separately; immutable measured source archives
and executables retain their original identities. The single targeted Opus review
passed after independently checking the recorded failures and all 60 captures.
The single targeted Astra performance review at high effort cleared both owner
dispositions. The owner subsequently accepted the cold CPU limitation with the
mandatory follow-up above, completing RC6 adoption. Current full native checks
passed again on both backends, including strict Clippy, formatting and Rustdoc.
The holistic integration review remains pending; no scope-readiness pass is yet
claimed.
