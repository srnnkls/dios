# Bench plan: sequential scan geometry

Written before implementation, 2026-09-13. The owner authorized matched
lookahead measurements and 64 KiB–1 MiB read experiments after reviewing the
Crotty/Leis/Pavlo benchmark. This is a benchmark-only extension of
`scopes/draft/plan-prefetch/design.md`; production ownership, reclamation,
default policy, and 4 KiB page semantics remain the measured baseline.

| Field | Value |
|---|---|
| Metric and direction | Candidate/base whole-scan elapsed, lower is better; CPU/useful byte, submitted read size/count, observed outstanding reads/bytes, faults, input amplification, wasted speculation, polls and admission calls |
| Workload | Initialized immutable existing 256 MiB + spare-page fixture; sequential full-page u64 fold. Cold 64 MiB traversal with 4 MiB arena; three 256 MiB passes with 64 MiB arena under private 128 MiB memory.max, swap zero. One worker CPU 0; controller CPU 4; nix performance governor/THP never; verified file-local cold state; direct/unregistered Dios |
| Baseline | mmap MADV_SEQUENTIAL; existing default automatic 4 KiB / 32-credit pool; explicit 4 KiB at the same credit count. Each comparison names its two configurations |
| Repetitions | Two qualification pairs plus 30 alternating fresh-process A/B pairs; fixed workloads; exploratory smoke pairs cannot establish a win |
| Threshold | Scan-win claim requires shared one-sided 95% elapsed-ratio upper <= 1.00 against mmap. Material geometry improvement requires upper <= 0.80 against the named 4 KiB control. Neither threshold automatically adopts a production change. Legacy runner bridge upper <= 1.05 in both orientations; observer trace/plain upper <= 1.05 for use of trace latency |
| Compare command | Retained mmap_workloads executable `summarize paired.csv`; shared `mise run gate paired.csv 1.00` (mmap win), `0.80` (material improvement), or `1.05` (bridge/observer). Preserve failed results |
| Escalation lever | Reject incorrect checksums, allocation, incomplete drain, cache/pressure mismatch or contaminated host samples. If the bridge fails, retain the existing runner for baseline and investigate loop differences. If a performance threshold fails, retain that result and profile the corresponding arm; do not change fences or relax bounds. If coarse reads help, a separate design and safety review is required before coalescing independently owned 4 KiB frames |

## Experiment matrix

1. Match explicit and automatic windows at 16, 32 and 64 credits using 4 KiB
   granules. Raise the common read limit to 128 so the 64-credit experiment
   retains demand capacity. Include mmap/default-32 directly paired and a
   bridge to the unchanged legacy automatic scan runner.
2. Sweep 4 KiB, 64 KiB, 256 KiB and 1 MiB granules using the existing public
   pool builder. Hold the maximum accepted-read byte budget at 2 MiB:
   read capacity 512, 32, 8, 2 respectively; speculative credits are capacity
   minus one, reserving one demand request. Use an 8 MiB arena for the cold
   geometry sweep and 64 MiB for pressure. The ordinary cold lookahead lanes
   retain their 4 MiB arena. This accommodates the existing three-read-capacity
   reclamation watermark without changing it. Frame count changes to preserve
   payload-arena bytes. The speculative/demand split consequently changes by
   one granule and is reported. These are coarse
   cache/guard granules: they measure combined read and bookkeeping
   amortization, not transparent coalescing of independent 4 KiB pages.
3. Compare the fastest qualified coarse configuration with mmap in independent
   30-pair confirmation runs on both shapes. Selection samples do not establish
   its confirmation result. Include true-sequential fio read-size controls if
   available; any upstream 1 MB randread reproduction has its own label.

Admission windows stop at the end of each pass. Explicit demand order stays
sequential. Automatic prediction may read beyond a pass or EOF; accepted reads
must drain and their extra traffic/errors remain in the record. Keep the
same full-page fold for every arm; do not replace consumption with a byte touch.

## Measurement and cost evidence

Preparation, pool construction and thread-local initialization are outside
timing. End timing only after demand consumption and accepted I/O drain.
Detailed observation buffers are allocated before timing and bounded by the
declared operation count. Event slots are initialized; flight-vector capacity
materializes as observations are appended, so observer costs include that
first-touch behavior. Primary runs have no per-read clocks.
Record exact checksums, zero timed allocations, credits, total reads in flight,
effective granule, arena bytes, read limit, resolved registration and I/O mode.
Keep raw samples, host snapshots, executable/source/fixture hashes and gate
outputs. Different process and payload memory limits are explicitly reported.

Use matched trace replays and paired observer controls on selected arms. Flight
snapshots measure logical accepted-read occupancy, not device queue depth.
Attempt block issue/complete tracing without privilege or host security
changes; record unavailable permissions as a limit. Request-count/size metadata
and OS read-byte counters cannot substitute for an actual block trace.
Collect loss-qualified CPU profiles from the exact primary executable, with
recorder/output outside the pressure cgroup. Estimate category CPU budgets
from unprofiled CPU demand and sample shares; retain unresolved kernel work.

The fio ceiling control uses true sequential `rw=read`, io_uring, direct reads,
one worker, 768 MiB read over the existing 256 MiB fixture and a 2 MiB maximum
outstanding-byte budget. Pair 4 KiB against 64/256/1024 KiB with two qualification
pairs and 30 measured pairs. Record fio's achieved-depth histogram, byte/I/O
counts and native read runtime (millisecond resolution). It performs no page
fold and is storage-only calibration, so its elapsed time is not an equal-work
Dios or mmap speedup. No production adoption gate is attached to it.

Production source is unchanged, so no new product performance gate is adopted.
The existing pinned regression gates remain applicable. Verify benchmark
configuration rejection, checksum/byte matching and malformed-row rejection,
Linux zero-allocation/path witnesses, strict Clippy and formatting.

## Research interpretation

The [paper](https://vldb.org/cidrdb/papers/2022/p13-crotty.pdf) uses a much
larger working set, more threads and faster/multiple SSDs. Its
[runner](https://github.com/viktorleis/mmapbench/blob/main/run.sh) uses
`--rw=randread --blocksize=1MB` for fio in the comparisons labelled sequential.
Preserve that distinction; do not import its large outstanding-byte budget
into a 128 MiB process cap or claim our experiment reproduces its result.

## Rejected incremental-refill hypothesis

The first matched-window pairs suggested trying a monotonic hint cursor to
avoid repeating the whole explicit window. This was proposed after v1 smoke
and before measuring that proposed control. Contract inspection showed that
the supplied list also protects useful speculative pages from replacement
(`Pool::prefetch` documentation). A singleton tail hint can consequently alter
eviction behavior. The unmeasured prototype was removed; no product change or
performance conclusion follows from it. Preserve the v1 runner and finish
the originally defined geometry/overlap attribution before proposing API work.

## Diagnostic qualification repair

The v1 coarse 256 KiB trace failed its bounded flight-buffer assertion before
any observer result was accepted. Admission, I/O completion and speculative
credit release can produce more than two distinct snapshots per acquisition.
Increase the preallocated trace-only allowance from `2 * operations + 128` to
`4 * operations + 128`; retain the overflow assertion. Plain timing has no
flight allocation. This changes the benchmark executable identity, so retain
v1 selection evidence and collect fresh v2 confirmation, plain/trace pairs and
CPU profiles using one retained v2 executable. Do not join v1 and v2 as if their
hashes matched. Confirm coarse/default arms on both shapes, plus the explicit
32-credit and 1 MiB cold controls used for attribution. No driver code changes.

Also distinguish identifiable setup stacks from orphaned stacks that lack a
workload caller. The pressure-default v1 capture has many such stacks despite
zero lost events. Withhold its absolute CPU category budgets; an increased
DWARF stack bound is a diagnostic retry, not permission to broaden the timed
boundary. Preserve failed captures and report unresolved attribution.
For this diagnostic repair, withhold category CPU budgets when orphaned samples
exceed 1% of the identified timed sample count. Show the excluded and orphaned
counts separately; this qualification rule does not change a performance gate.

The scan's `requests` metadata counts granule acquisitions, not exact backend
submissions. That planned measurement remains unavailable in the scan runner;
the later driver probe counts SQEs and CQEs directly. Fio rates are attained
storage-path references, not certified physical ceilings: the application can
outperform a particular fio engine/queue configuration. Preserve that distinction
when using the calibration in the cost explanation.

<a id="open-owner-decision-coalescing-mechanism"></a>

## Owner decision: coalescing mechanism

**Decided by Sören, 2026-09-14:** adopt ordinary READV for coalesced readahead
over scattered, independently owned 4 KiB frames, at most 32 iovecs per SQE.
Explicit hints and automatic prediction split missing runs at resident/pending
holes. Point reads retain their per-frame path. This closes the mechanism
choice opened for the [driver probe](readv_mechanism.md).

Use plain READV on Linux 6.6 for both Unregistered and Registered arenas;
ordinary READV is valid for registered memory but does not use fixed-buffer
pinning savings. READV_FIXED is a future extension when the kernel floor
reaches 6.15, not part of this implementation. AM2's run allocator is
**deferred**, revisited only for a Registered deployment below that kernel
floor with measured binding pinning cost. Plain READV is no longer rejected.

One coalesced CQE completing multiple independently owned frames needs the
span-slab completion responsibility identified by AM2, byte-exact
short-read progress, per-frame publication and credit accounting. A READV
result changes destination representation, so AM2's contiguous first-frame
representation cannot be copied unchanged. The new
[readahead-coalescing scope](../../scopes/draft/readahead-coalescing/scope.md)
owns the bounded vector/route design, automatic range admission and safety
review. **Product implementation remains blocked on scope review/approval.**
No AM2 implementation is authorized by this decision.

Evidence update, 2026-09-14: the corrected
[pipelined probe](../evidence/readv_pipelined/README.md) passes the Unregistered
depth-8 b/c gate (1.003389, upper 1.004666); depth 16 is 1.002260, upper
1.004455. The corrected serial gate still fails (1.101803, upper 1.105391).
The older serial/pipeline samples used an invalid pointer-construction pattern
and remain retained unqualified; the corrected datasets supersede them.

The separately observer-qualified [clock split](../evidence/readv_pipelined/clock-split.md)
localizes 4.70 us/group of the diagnostic's 7.48 us gap to submission, 0.05 us
to post-submit completion observation and 2.72 us to remaining work. Repeated
per-iovec pinning is source-supported, but no isolated pinning coefficient or
physical/DMA segment count was measured. Registration avoids repeated user-page
pinning, not necessarily DMA/descriptor costs. A virtually contiguous run and
THP `never` do not establish physical contiguity/scatter; IOMMU mapping may
coalesce segments. The 2.72 us residual remains noted probe-local work; the
owner chose coalescing rather than further residual attribution. Exact
physical/DMA segment counts and per-kernel-function budgets remain unmeasured.
The decision selects the mechanism, not an unsupported causal coefficient.
