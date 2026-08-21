---
created: 2026-08-19
status: evidence-only
issue_type: Evidence
revision: 2
---

# Evidence: read-protocol-atomic experiment

This is a non-actionable historical record. It preserves the atomic
read-protocol experiment, measurements, and original adoption questions; it
does not authorize implementation, sequencing, or adoption work. The active
`dios-r1-r7-read-performance` scope owns every current decision and gate.

## Ownership handoff

`dios-r1-r7-read-performance` supersedes `read-protocol-atomic` for the
overlapping liveness mirror, `ReaderSlot` alignment, hash decision, and proof
implementation. `read-protocol-atomic` remains an evidence source for that
active scope; its experiment, measurements, and rejected packed-hash result
retain historical authority but authorize no separate product implementation.

## Candidate and provenance

| what | where |
|---|---|
| candidate source (uncommitted) | `.worktrees/experiment-read-protocol-tightened`, branch `experiment/read-protocol-tightened`, base 1004a2e |
| measured checkout (identical diff) | `/private/tmp/dios-read-protocol-atomic-1004a2e` (+ baseline sibling) |
| the same mirror, independently converged | `spike/prefetch-reserve` worktree |
| evidence record | `scopes/draft/plan-prefetch/resources/warm-protocol-audit.md` (challenger section) |

Measured: warm bracket 2.00–2.09 → 1.75 (Zen 2), 1.69–1.90 → 1.46
(M1); full suite green incl. `pool_retire`; reader scaling positive to
8 threads (unpaired). Shipping point-proof profile names exactly the
residuals this candidate removes (inlined liveness-lock/hash work).

## Historical gate state

1. Loom get-vs-retire case — NOT WRITTEN. The record called for a pool Loom
   model with a reader `get` racing `retire_file` over the
   liveness mirror, covering the audit §3 adversary (LIVE observed →
   full retire + unmap + reclaim + slot reuse → pin publish → lookup)
   and asserting either `StaleFile` or a linearized-before-retire
   guard whose frame the reclaimer cannot reuse. The T009 model covers
   advance/grace only. This gate covers the spike's mirror and the
   candidate's identical twin at once.
2. Hash probe-distribution — SPEC pre-registered in the candidate's
   `benches/plans/table_tightening.md`; simulation RUN 2026-08-19
   (`resources/probe_sim.py`, `resources/probe_sim_results.txt`):
   - the shipped packed hash (`slot << 32 | gen ^ granule`) is
     REJECTED, structurally: at dios-realistic small tables (1 Ki
     slots) with 16-file interleaved keys, mean +19% and p99 2×
     (11 vs 5). Not quantization.
   - the plan's own escalation shape (full-width one-round:
     `driver ^ (gen << 32 | granule) ^ slot × PHI`) eliminates the
     structural failure; its remaining FAILs are single-probe
   quantization at 1 Ki tables (p99 8 vs 7), where the ±5% relative
   bound could not admit any one-round hash. At capture time, the unresolved
   alternatives were an absolute +1-probe small-table tolerance followed by a
   full-width-candidate speed re-bench, or dropping the hash and retaining only
   mirror + alignment (estimated warm 2.04 → ~1.9).
   - caveat: the sim is one interpretation of the spec's patterns and
   generation assignment; the structural verdict is robust to it,
   the marginal ones are not. The active scope, not this record, owns any
   resulting decision.
3. Paired contention run — SPEC pre-registered in the candidate's
   `benches/plans/warm_scaling.md` (baseline = main mutex protocol at
   1004a2e, candidate = mirror, CCX-pinned on the box, 8-thread ≥ 2×
   1-thread, full-fold lane decides). NOT RUN as a pair; only unpaired
   M1 candidate numbers exist (14.1 → 49.2 M ops/s, 1 → 8 threads).

## Related second candidate

The R7 hint surface (`ResidentFileLease` + `ResidentHint` + stamp-
validated hinted pinning, `.worktrees/experiment-sira-point-proof`,
mock-only) supersedes parts of this candidate for locator-carrying
consumers: it bypasses the table lookup entirely, so the hash gate
matters mainly for anonymous gets and the miss path. The historical proposal
combined both adversaries in one Loom extension: get-vs-retire over the
liveness mirror, and hinted-acquisition-vs-eviction/reuse (their §14 names the
same two cases; recorded outcome: typed `StaleFile` or a
linearized-before-retire guard). The active scope owns the current proof plan.

## Historical disposition

The experiment recorded gate 1 as a soundness blocker and gates 2–3 as
partitioning the candidate. Those statements preserve provenance only. No task
may execute from this evidence-only record; all current sequencing,
implementation, validation, and escalation authority resides in
`dios-r1-r7-read-performance`.
