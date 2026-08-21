# Bench Plan: prefetch_window

| Field | Value |
|-------|-------|
| Metric & direction | wall-time ratio candidate/base of a 32-page cold fragmented drain through the composed `Pool`, lower is better |
| Workload | 256 MiB preallocated file (65,536 × 4 KiB granules, per-granule fill byte), opened `DirectIo::Preferred`; one reader over a 1,984-frame pool; both arms draw from one shared bijective page permutation (`i × 75,193 mod 65,536`), so every page is unique and fragmented and the whole run fits the frame budget — no claim ever waits on epoch-matured reclamation (in-bench Busy assertions pin the regime); 30 reps × 1 iter, one iter = one 32-page drain |
| Baseline | pure demand drain, QD1 by construction: `get` → `poll`-until-`ready` → probe one landed byte → drop guard, page by page — the synchronous consuming cursor with no plan knowledge. Candidate: the same consuming cursor preceded by a 16-page fire-and-forget lookahead (`get` with the `PendingToken` immediately dropped; the read still completes and residents the page; the cursor's demand `get` coalesces via the miss-table singleflight) |
| Reps | 30 (protocol minimum) × 1 iter_per_rep (device-bound: one base iter ≈ 32 × ~60 µs ≈ 2 ms) |
| Threshold | one-sided 95% CI upper bound of the ratio ≤ 0.5 on the pinned Linux host; macOS numbers are advisory — the eager-inline backend executes every enqueued read serially at `poll`, so no overlap exists and the advisory ratio is ~1.0 (measured 0.97) |
| Compare command | `mise run gate target/bench-samples/prefetch_window.csv 0.5` |
| Escalation lever | this gate is the existence falsifier for the plan-prefetch scope (dios-seeds falsifier #1): a FAIL on the pinned host kills the API premise — record the verdict against the seed and create no scope. Before recording a kill: fio QD1-vs-QD32 on the same filesystem (kernel/device ceiling), then a driver-level batched-submit probe (pool bypass), so the fail indicts the mechanism, not a submission bug |

## Results (2026-08-18, pinned host, kernel 6.6.64, NVMe 970 PRO)

Gate run: ratio geomean 0.1826, ci95 upper 0.2107, threshold 0.5 → PASS
(reproduced: 0.1782/0.2074). The 5.5× win flows through the public
`get`/dropped-token/`ready` surface unchanged.

Reference ladder measured the same day, same filesystem:

| Layer | Result |
|---|---|
| fio io_uring 4K randread | QD1 15.9k IOPS (59.8 µs); QD32 274k IOPS — 17× ceiling |
| driver bypass (submit 32 → reap 32 vs submit-reap ×32) | 57 µs/read serial, 7 µs/read batched, ratio 0.127 |
| pool, unpressured (this gate) | 0.18 |
| pool, frame-recycling pressure (first-draft workload) | 0.93 — FAIL, see below |
| macOS eager-inline advisory | 0.97 (control: no overlap exists) |

## Escalation record — the pressured-regime FAIL is a finding, not noise

The first-draft workload (256-frame pool, 64-page windows, 32-deep
lookahead, pages recycled through eviction) failed the gate at ratio
0.93/0.95. Instrumentation attributed it exactly: of ~20.5k lookahead
issues, 10,210 returned `Get::Busy` (and the demand path stalled 10,197
times) — under steady-state recycling, frame claim is bound by
epoch-matured reclamation, a burst claimer drains the claimable stock
instantly, and the fire-and-forget lookahead collapses to ~QD1. The
driver overlaps fine (0.127 bypassing the pool); the pool's claim path
is the serializer.

Consequence carried into the plan-prefetch scope
(`scopes/draft/plan-prefetch/`): a prefetch surface without an
admission/headroom story is worthless under real frame budgets — the
staging class + `staging_headroom` + admission vocabulary is what makes
the measured unpressured win capturable, not decoration on top of it.
The workload above was re-registered to the unpressured regime to
measure the API-reachable ceiling; the threshold itself was never
relaxed.

## Notes

Existence experiment for the plan-prefetch seed
(`~/projects/sira/resources/dios-seeds.md`), API-free by construction:
prefetch = `get` + dropped `PendingToken` (contract: dropping cancels
waiter interest only; the read completes and the page becomes resident),
consumption unchanged. Dropped-miss slots reclaim lazily (admission
reuses zero-interest terminal slots), so the lookahead does not leak the
miss table.

The baseline is QD1 because sira's consumer is the synchronous
borrowed-lease cursor: with no plan knowledge a single cursor cannot
overlap its own misses. Demand-miss coalescing reaches depth only across
concurrent readers — a different workload; this gate scopes the
single-cursor drain the seed targets.

Registered buffers pin memory: the ring backend's frame arena counts
against `RLIMIT_MEMLOCK` (8 MiB on the pinned host → ≤ ~2,048 four-KiB
frames per pool in a bench process). 24,576 frames failed driver build
with ENOMEM. Any staging-capable pool sizing on Linux inherits this
constraint (ulimit or unregistered-buffer fallback are the levers).

Pool sizing: `max_inflight_reads` 34, `miss_headroom` 102 (the 3×
floor), 1,984 frames ≥ the INV-9 watermark and ≥ total pages drawn
(1,920 + warmup). `Get::Busy` on the lookahead path stalls the prefetch
pointer for that cursor step, never the cursor itself.

What this bench cannot show: pollution of a demand-hot set (needs the
staging eviction class), tail latency (seed falsifier #3 — a scope-level
gate), and the adaptive-window sweet spot (falsifier #5).
