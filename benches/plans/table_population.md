# Bench Plan: table_population

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio of constructing the page table of a 240,241-frame pool (524,288 slots, 20 MiB) alone to constructing it and touching every slot (candidate/base), lower is better; the ratio is the share of the eager table cost a build still pays |
| Workload | `PageTable::with_frame_count(240_241)` per iteration on both arms; the base arm adds `populate()` (one store per slot, the fault per page the fill construction used to pay); one iteration per rep so each rep maps and unmaps a fresh table; 40 reps, interleaved A/B |
| Baseline | the eager arm itself (interleaved A/B): a construction that still writes every slot sits near 1.0; a zero-page mapping pays one `mmap` and its page tables |
| Reps | 40 (protocol minimum 30) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 0.10 |
| Compare command | `mise run gate target/bench-samples/table_population.csv 0.10` |
| Escalation lever | a failing gate means construction writes the table again (an element constructor loop, an allocator that clears reused heap memory, or a populating `madvise`); profile the candidate arm with the flamegraph skill and attribute the faults to their call before touching the table |

## Notes

Bench: `cargo bench --features bench --bench table_population` writes
`target/bench-samples/table_population.csv`; the gate is asserted only
by `mise run gate`, never in-bench. Both arms run in-process with no
kernel I/O; on Linux with `THP=never` the base arm takes one minor fault
per 4 KiB page (5,120 faults for 40 B slots), so the absolute base time
varies by host while the ratio bound holds.

Origin: the perf profile of sira's DM009 cold arms on the pinned host
(dios 2f7ffe6, `tr-cold-perf.log`) attributed ~16 ms of each ~26 ms dios
open to write faults populating the pool's frame-scaled tables through an
element-constructor loop (page table 7.4 ms, miss table 5.7 ms, frame
state, clock, retention and in-flight words ~4 ms). mmap's cold open on
the same store measured 15 ms, so the tables alone held dios above the
DM-R6 bar after the arena fill was removed (#9).

Every table whose vacant element is the all-zero pattern (atomics, the
uninitialised page cells) now lives in a private anonymous mapping and
is never written at construction; the kernel materialises a page the
first time a slot in it is touched. Tables whose vacant element is an
`Option::None` (the miss table's entries, the pending index, the frame
page index) keep the constructor loop: `None` has no guaranteed bit
pattern for those payloads.
