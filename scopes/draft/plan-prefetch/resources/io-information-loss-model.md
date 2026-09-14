# The I/O information-loss model (external essay, 2026-08-18) — audited against the evidence ladder

External theoretical frame for why explicit plan-driven I/O beats mmap:
total cost C(r) = C_D (device) + C_L (late discovery) + C_H (heuristic
mismatch) + C_Q (concurrency exposure) + C_A (admission semantics).
mmap discovers intent at fault time, exposes no future misses
(schedulability S ≈ 0), predicts locality from fault history instead of
the plan, and its admission is the OS's. The frame is adopted; the
essay's experiment-to-term mapping needed corrections, below.

## Measured term estimates (evidence-ladder rows in scope.md)

| term | measured estimate | source rows |
|---|---|---|
| C_Q (S = 0 → S > 0) | the dominant term: 6–12× on cold scatter vs single-cursor mmap; 5.5–8.9× within explicit I/O (demand QD1 → plan-driven) | 3, 5, 8 |
| A = C_L + C_H at equal concurrency | 16% (Darwin, 8 faulters vs 8 pread workers, 0.841); ≥10% (Linux, 0.903, understated — one dios thread vs eight faulters) | 8 |
| C_H alone | +32% from default fault-around vs `MADV_RANDOM` on scatter (82.4 vs 62.2 µs/page); W > 1 amplification plausible, bytes-fetched not measured | 8 |
| C_L alone, cold QD1 | ≈ 0 ON THIS DEVICE: single-cursor mmap `MADV_RANDOM` 62.2 µs/page vs single-cursor pread ~62 µs/page — fault machinery hides inside the 60 µs NVMe wait | 3 (base), 8 |
| C_A | ~35%: pressured reserve drain 7.0 µs/page vs unpressured demand-path get+drop ~11.2 µs/page (cross-workload estimate, not a gate) | 3, 5 |

## Corrections to the essay's own mapping

1. "The macOS result isolates C_L + C_H because async isn't involved" —
   wrong pair: the 5.9× macOS pair is one faulting cursor vs eight pread
   workers, i.e. almost pure C_Q. The equal-concurrency pairs isolate A,
   and A is an order of magnitude smaller than C_Q.
2. The cold-path C_L illustration (~20% VM overhead at 50 µs device
   latency) is refuted at this latency class: measured C_L ≈ 0 cold at
   QD1. The machinery-cost story is real but shows up WARM (1.7–2.1× on
   a bare resident load at small sets — rows 10–11), the inverse of the
   essay's emphasis. The C/L_io scaling argument survives as a
   prediction for faster devices: falsifier 1's latency-class split
   doubles as the C_L test.
3. The warm dTLB inversion (pool 0.64 vs bare mmap on Apple Silicon,
   zero faults in either arm — row 11) is a memory-layout/TLB
   phenomenon outside this model; the model takes no credit for it.

## What the scope adopts

- The progress-guarantee split as the theoretical statement of the
  dedicated admission path: demand admission implements MUST-SUCCEED
  (bounded reclaim ritual: linear free-scan, completion drain, epoch
  advance, victim eviction, rescan — `claim_frame_bounded`), speculation
  implements MAY-FAIL (credit pop or `deferred`). Different progress
  guarantees imply different optimal admission algorithms; conflating
  them taxes speculative I/O with demand's stronger semantics. This is
  scope.md insight 4's mechanism, named.
- Schedulability S as the one-word answer to "why does the win exist
  before async": moving S from 0 to > 0 is worth more than every
  refinement after it (row 8's single-cursor pairs), and async then
  raises S rather than creating the win.
- The request-semantics tuple (address, speculative, drop-allowed,
  distance) as the design-review question for every admission surface:
  which dimensions does this path discard?

## Refinements (same session, second pass)

### Layering: sequential composition is the semantic principle, this model its physical execution

The foreknowledge I_p is not free — it is manufactured by the consumer's
sequential composition (narrow → predict span → admit by semantics →
issue ahead → consume sequentially). The chain terminates at the dios
seam as a `PageId` window plus a speculative/demand admission class:
dios never sees "sequential composition," only its product. That keeps
the general-purpose rule intact — the model explains why the seam's
shape is right without importing the consumer's semantics through it.
Async is an implementation consequence of composition exposing the
future, not the source of the win (the S = 0 → S > 0 step dominates).

### The warm model needs a working-set term

Warm mmap is NOT approximately a bare load; it is
load + translation(W, layout). At W = 64 pages translation ≈ 0 and the
pool's fixed machinery shows fully (1.7–2.1×, row 10). At W = 256 MiB
translation and memory-hierarchy effects dominate both arms and the
machinery amortizes: Linux 2.05 → 1.18 (row 11) is the textbook
prediction of the refined model. The macOS 0.64 inversion implies an
additional term favoring the pool whose mechanism is deliberately
unnamed until profiled — the recorded claim is the measurement, never
"pool ownership improves TLB behavior."

Regime table as currently supported (isolated benches, whole-granule
fold as the unit of useful work):

| regime | measured state |
|---|---|
| tiny resident set | mmap keeps a constant-factor edge (1.7–2.1×) |
| large resident set | edge shrinks to 1.18 isolated (Linux) / inverts (macOS); PARITY IS NOT YET THE CLAIM — parity under real block-fetch work (CRC + decode amortization) is DIO-G1's hypothesis, unmeasured |
| cold planned | explicit ownership wins 6–12× (row 8) |
| speculative cold | semantic admission preserves the win under pressure (row 5) |

### Reserve neutrality is the "pay where it matters" property

The speculative semantics could have taxed every ordinary hit; the
1.02–1.04 ratio-of-ratios (row 10) says the demand warm hit stays on
the ordinary fast path while only planned speculative misses pay the
reserve admission. Richer I/O semantics priced at their use site, not
as a global abstraction tax.

### The integrated falsifier already exists in the tree

The model's crisp closing question — "does the consumer pay > 2% on
real warm block fetch?" — is exactly the pre-registered DIO-G1 1.02
warm-parity bar (sira-dios-migration, block-fetch layer, vs sira's own
mmap reader). The theory converges on a gate that was pinned before the
theory existed; nothing new needs registering, the binding run needs
running.

## Warm-path model, final form (third pass, verified + parameter cross-check)

R(U, H) = alpha(H) + F / (c_m * U), where F is the fixed cost of one
protected residency transaction, U the useful bytes consumed under it,
c_m the mmap-side resident cost per byte, alpha the arena/mmap
per-byte cost ratio under translation regime H.

Parameter consistency across independent regimes (the check that makes
this a model, not a fit):

- F ≈ 41 ns from the small-set fold floor (direct measurement), AND
  F = 0.48 × ~85 ns ≈ 41 ns recovered from the 4 KiB/256 MiB no-THP
  point (R 1.48) — same constant from different footprints and
  translation regimes. M1 independently: 39.9 ns.
- alpha(THP, Linux): ≈ 0.92 at the 4 MiB set, ≈ 0.70 at 256 MiB — the
  hugepage advantage grows with dTLB pressure, as it must.
- Crossover U* = F / (c_m − c_a) ≈ 48 KB on this host/workload —
  matching the observed ~64 KiB crossing. NOT a universal constant
  (macOS crosses at 4 KiB/256 MiB).

Structural consequences, all measured: amortization explains the
approach to parity (a fixed positive F yields R → 1 from above, never
R < 1); THP explains the crossing (c_a < c_m required); pin-only span
wrappers recover ~3% because they batch one term of F, not F itself; a
true span transaction must batch liveness + lookup + validation +
CLOCK + guard as one range operation. The governing variable is useful
bytes per protected transaction (U_lease), decoupled from the format
granule G_f: the target is G_f = 4 KiB with U_lease = 16–64 KiB+ when
the plan exposes a run — warm amortization without 16× cold read
amplification. Ruled out by the evidence: multiplicative warm penalty;
intrinsic large-granule pool advantage; crossing-as-artifact;
pin-retention-as-span; format coarsening for warm reasons.

Remaining discriminator: the pre-registered four-arm bench (coarse get
/ true span op / independent gets / bare mmap, × THP on/off), which
separates the transaction-amortization and translation-layout terms on
the real surface. REMIX span measurements then supply the production
values of U_lease and A_read (bytes fetched / bytes useful).

## Constant revision (read-protocol-atomic challenger, same day)

F drops to ~31 ns (Zen 2) / ~25 ns (M1) under the atomic-read protocol
revision (lock-free liveness + collapsed page hash + reader-slot
alignment; see warm-protocol-audit.md). The law and all structural
conclusions are unchanged — the 4 KiB small-set prediction with the new
constant (1 + 31/41.5 = 1.75) matches its measurement exactly.

## The REMIX fact closes the loop (final sharpening)

U_lease is not a prediction for sira — it is a plan artifact. Winner
spans enumerate every (run, block) a drain touches before the first
read, and the ordinal gather already consumes per-(run, block), so
consumption is ALREADY span-shaped: a span lease maps 1:1 onto existing
structure. Consequences by regime: cold — the same enumeration feeds
prefetch windows (this scope, 6–12× measured); warm ranges — one lease
per winner span drops F's share from ~6% (per-block pins) to ~0.4%
(16-block span), parity-to-ahead with the THP term, conditional on span
statistics; warm points — no spans to manufacture, the ~2.5–3% floor
stands (beneath sira's decode noise per the shipping profile), and the
REMIX-adjacent lever there is retention of known-hot winner blocks
(pinned-frame-retention), not protocol. FIRST STEP, no dios code:
measure the U_lease distribution (span lengths, bytes per (run, block)
visit) from real REMIX views — the validated law then predicts the span
lease's warm ratio before get_span exists; the four-arm bench
discriminates only if spans turn out short.

## The consumer class (fix-point Datalog — sira's FIRST consumer, not its purpose; sira is general-purpose with Cozo as its first user, exactly as dios is general-purpose with sira as its first)

Semi-naive evaluation manufactures foreknowledge algorithmically: the
delta set of iteration k is fully enumerated before iteration k+1 runs.
Mapping, with corrections to the naive version of the claim:

- Delta joins: foreknowledge guaranteed, locality NOT — frontiers can
  scatter. Scattered-but-known is the measured cold win (rows 3–8);
  same-frame span collapse pays where hub density meets frame size.
  Production delta joins legitimately coalesce across the whole batch,
  which the frozen R7 ladder forbids per query — the ladder is a LOWER
  bound for this consumer class.
- Repeated iterations: join STRUCTURE repeats, keys do not (that is
  semi-naive's point). What stays warm: hub pages (winner-block reuse →
  retention/hint surfaces) and plan metadata. Regime consequence: a
  fixpoint inner loop is the deeply repeated regime — the 22.6 ns hint
  residual applies, not the fresh-child 51.8 ns; the fresh-child gate
  is conservative for exactly this consumer.
- Magic-set prefix scans and multi-hop probes map onto the n=10–4096
  ladder rungs and 16-span anchored windows as-is.
- The tail and the dedup stream are point-shaped: ΔR shrinks toward
  singletons at convergence, and every derived tuple pays a membership
  probe against the full relation. Fix-point evaluation stresses BOTH
  ladder ends at once — the n=1 gate is the dedup stream's gate, not a
  microbenchmark scruple.

### Consumer taxonomy by foreknowledge horizon (generality check)

Workload classes order themselves by how much future they expose — the
model's S variable populated with real systems. The machinery degrades
gracefully along the axis because window depth and span length ARE the
horizon:

| horizon | classes | what they stress |
|---|---|---|
| full plan | REMIX drains, compiled Datalog (Soufflé/CodeQL), SPARQL property paths | spans, anchored windows, prefetch at depth |
| batch per iteration | semi-naive deltas, IVM/streaming state (Materialize/Flink-on-RocksDB), graph frontiers (disk-backed Neo4j-class) | known-but-scattered prefetch, hub-page hints, cross-batch coalescing |
| one hop | disk-backed ANN beam search (DiskANN-class) | minimum-depth windows, hints under non-repeating keys, medium value sizes — the best SECOND-consumer falsifier for sira's generality |
| zero | anonymous point KV | mmap's last stronghold; the n=1 gate |

Membership requires a storage seam: RAM-resident engines (Ligra/Giraph
BSP, eBPF/XDP LPM tries) rhyme at the CPU/cache level but are not
consumers of this architecture. Note that Soufflé/CodeQL/Materialize/
SPARQL are the Datalog evaluation family — broad market, one pattern
profile, one generality data point, not four.
