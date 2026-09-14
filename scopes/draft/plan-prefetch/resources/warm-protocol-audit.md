# Warm-protocol suggestions audit (external essay, 2026-08-18) — verified against code and measurement

Seven proposals for tightening the warm-hit protocol via lifetime/scope
assumptions. Each verified against dios source and, where a ceiling was
measurable today, measured on the pinned host. Net verdict: the essay's
own arithmetic is right — a ~1.5–1.6× floor remains even with its full
package — and the two levers it could not know were already measured
(fence, publish) are noise-level. The valuable outputs are the mirror
soundness argument, one loom obligation, and two convergences with
existing dios designs.

## §3 — liveness-mirror race: SOUND, with a sharper argument than the essay's

The proposed adversary (reader observes LIVE → retirer retires, unmaps,
scans, reclaims/reuses → reader publishes pin and "follows previously
observed state") requires the reader to carry pointer/mapping state
observed pre-pin. The mirror reads ONLY the generation word pre-pin.
All mapping state is observed post-publish: `pin_owned` publishes the
epoch, THEN looks up the table, THEN validates `FrameState::Resident`
(the ordering documented on `pin_internal` as the eviction defense). A
retire that removed the mapping makes the post-pin lookup miss; the
path falls to the control lock where the locked check returns
`StaleFile`. Frame reuse cannot alias: `PageId` embeds the file
generation, so a re-registered slot mints distinct `PageId`s and the
retired page can never re-enter the table. Remaining semantics are the
essay's "usually sufficient contract": a get racing an in-flight retire
may linearize before it; the sequential contract (`pool_retire`) holds
because the mirror flips under the control lock before `retire_file`
returns. ADOPTION CONDITION for main: an explicit loom case for
get-vs-retire over the mirror (the T009 model covers advance/grace, not
this pair).

## §4 — scope-amortized pinning: mechanism ALREADY EXISTS, ceiling measured at ~3%

`begin_pin` publishes + fences only when `guard_count == 0`; nested
pins reuse the published epoch. Holding one guard across the warm loop
makes every hit a nested pin — `with_read_scope` semantics with zero
new API. Measured (spike, `peak_guards_per_reader` 2, scope guard held
across the bench loop): Linux 1.96 vs the 2.00–2.09 band, macOS 1.71
vs 1.69–1.90. ~2–4 ns/hit. Consistent with the fence-removal null
(protocol audit in `benches/plans/mmap_warm_path.md`): epoch
publication was never the rock. A `ReadScope` API is NOT justified as a
performance lever; if it ever lands it is ergonomics, and the nested-pin
mechanism is already the implementation.

## §5 — monotonic page-table topology + atomic leaves: ASSUMPTION FALSIFIED

The package requires "page-table nodes never removed until file
retirement." dios's table is a cache index: eviction removes entries
continuously (`remove_shared` on every victim). Add-only-until-retire
holds for mmap-total-residency designs, not for a fixed-budget pool.
The single-word-leaf half additionally requires packing `PageId` +
frame into 64 bits (generation-width assumption that bites — prior
audit). Seqlock stays; its measured share is ~9 ns.

## §6 — approximate CLOCK: ALREADY SHIPPED

`Clock::reference` is load-elided (store only on clear→set, the DIO-G1
elision seam). Remaining cost is one relaxed load. Sampling would trade
policy accuracy for ~1 ns; rejected.

## §7 — pinned resident view: EXISTS AS DRAFT

The "declared set, pinned until dropped" surface is
`scopes/draft/pinned-frame-retention` (`RetainedFrame`, refcounted
retention outside the epoch). The essay independently converged on it,
including the same warning: build it for a demonstrated stable hot-set
workload, not for the 64-page showcase.

## §1/§2 — capability leases and the retirement contract: CLARIFIES, DOESN'T ACCELERATE

dios's concurrent semantics are already effectively the graceful
contract (racing gets linearize before retire; existing guards outlive
it via the epoch); the strong sequential contract rides the under-lock
mirror flip. A `FileLease` handle would eliminate only the ~1 ns
generation load and the per-call branch — not a performance lever. Its
real content is API shape (validate once, borrow repeatedly), which the
general-purpose seam may want someday for ergonomics; it changes no
measured number today.

## "Do not simply downgrade SeqCst": AGREED AND MOOT

Agreed on the reasoning, and moot by measurement — the fence costs
~2–4 ns here (store buffer holds one epoch store at drain), so there is
nothing to buy even with a sound weakening.

## Recovery arithmetic: CONFIRMED

The essay's estimate (removing seqlock + publication entirely leaves
~67 ns ≈ 1.6×; parity requires moving all validation/mapping/protection
outside the hit loop) matches the measured decomposition and the prior
stopping argument. The granule sweep already showed the cheaper truth:
the ratio is machinery ÷ useful-bytes-per-pin, and consumption shape —
not protocol cleverness — is what moves it.

## Synthesis check (final external pass, verified)

The closing synthesis — fixed per-protected-access cost, amortization
law, THP as a separate offset, composition over format change — is
measured, with two calibrations:

1. "Sira sits close to the unfavorable end" equates useful work with
   payload bytes — now REFUTED BY SIRA'S OWN PROFILES, not just argued.
   The denominator is measured: sira's warm point get on the mmap plane
   costs 1.10 µs on the M1 (point-parity gate, 5M store, 1.39× faster
   than redb) and 1.29 µs on the TR (pre-dios read anchor), with the
   M1 attribution: `lower_bound` probes ~0.45 µs (memory-latency
   bound), header decode + memcmp ~0.25 µs, block acquisition +
   bookkeeping ~0.25 µs, remainder ~0.15 µs. dios's measured machinery
   constant is 41 ns/pin (Zen 2) and 40 ns/pin (M1, derived from the
   same samples: 95.1 − 55.2 ns). Therefore, stated confidently:

   - ABSOLUTE: ~40 ns per protected access, both hosts.
   - RELATIVE, additive worst case: one-pin point get 3.2–3.6% of the
     measured total; the columnar two-pin shape (key + value block)
     6.5–7.3%. Not the microbench 2×.
   - NET, the DIO-G1 question sharpened: dios's get lands in exactly
     the region where sira already spends ~250 ns (23%) on block
     acquisition + bookkeeping — the pre-dios anchor says so verbatim
     ("buffer-pool overhead will land in exactly this region") and
     supplies per-symbol baselines. The 1.02 parity bar is therefore
     NOT met by addition (additive ≈ 1.03–1.07); it requires dios's
     lookup/pin to DISPLACE part of the existing acquisition
     bookkeeping. That displacement, adjudicated per-symbol against
     the anchor's flat tables, is the real content of the binding run.
   - Range scans amortize further: pins are per block, rows per block
     are many (2.24 µs/scan TR anchor; 6.92 µs/scan M1 co-measure).
2. The ~41 ns constant is protocol-fixed but not portable: single
   reader, two microarchitectures. The law transfers; the number does
   not, and under reader contention the liveness-mirror adoption is
   what keeps it from growing.

## Closing verdict (adopted from the final external pass; every clause traces to a recorded artifact)

Warm-path behavior is explained by two independently verified effects:
an approximately fixed protocol cost per protected access (~40 ns both
hosts), amortized by useful work, and a hugepage-backed arena advantage
under translation pressure. The fixed-cost model predicts all three
measured granule points, including the 64 KiB MADV_NOHUGEPAGE result
within 0.002 — the remaining single-page gap is quantitatively
explained; the system carries no unidentified warm-path bottleneck.
The optimization hierarchy: (1) hugepage arena — shipped, causally
validated, 30 ratio points; (2) more useful work per protected access —
validated by the sweep, and sira's measured point path already sits at
percent-scale exposure; (3) true span composition — the credible next
surface, discriminated by the pre-registered four-arm bench; (4) pinned
resident view — only for demonstrated stable hot sets
(pinned-frame-retention); (5) further single-get shaving — measured
candidates are tiny, unsafe, already implemented, or regressions.
STOPPING RULE: further work on single-page `get` is benchmark cosmetics
unless a new profile exposes genuinely removable cost. The productive
frontier is operation composition and workload shape, on the settled
4 KiB format, pending real-segment evidence at T011/T014.

## Why F ≈ 41 ns is the floor (the stopping rule's "why", decomposed by guarantee)

Each component is the cheapest audited sound implementation of a
contract obligation; "cheaper" now means only "fewer guarantees":

| question every warm access must answer | cost | cheapest audited alternative |
|---|---|---|
| which frame holds this page (software TLB) | ~9 ns seqlock lookup | hardware translation — measured NOT free at scale (row 11); single-word packing bites (generation cap) |
| is it live/mapped (typed staleness, no SIGBUS) | few ns | mirror already lock-free; lease API saves ~1 ns |
| will it stay while I read (the guarantee mmap does not offer) | ~6–10 ns publish + guard | fence removal unsound AND worthless (~2–4 ns); membarrier same; refcounts/hazptr cost more; QSBR converts to an unbounded reader obligation that bites a fixed pool |
| should it stay resident (policy) | ~1 ns elided CLOCK | none needed |

Desirability: R − 1 = (F − Δ)/T. Cutting F fights for nanoseconds
inside loom-verified unsafe code (silent use-after-free as the failure
mode); growing the denominator is ordinary API design — span
composition divides F by run length, THP already supplies Δ = 30 ratio
points, and sira's measured exposure is 3–7% of a point get, so even
free-and-safe total protocol elimination caps at that. The fixed cost
is the price of OWNING residency in userspace rather than renting it
from the kernel; owning it is what makes the cold path winnable
(rows 3–8). Open: cache-line fusion (~10–15 ns ceiling, redesign, no
workload justifies it); contention scaling of F (unmeasured — the
liveness mirror is the hedge).

## Challenger accepted: `read-protocol-atomic` (2026-08-18, /private/tmp checkout at 1004a2e)

An independent protocol revision falsified the letter of the stopping
rule within hours of its recording. Contents: (1) the same lock-free
liveness mirror as the spike (identical encoding, independently
converged, plus the locked re-check on the miss path); (2)
`#[repr(align(64))]` on `ReaderSlot` (false-sharing hedge); (3) the
find: `page_hash` collapsed from four serial mix rounds to one round
over a packed word (`slot << 32 | generation ^ granule_idx`), exploiting
one-live-generation-per-slot — and safe even without that invariant,
since lookup compares full keys (collisions cost probes, never
correctness).

Measured (full suite passes incl. `pool_retire`; clippy clean):

| host | main band | challenger | F implied |
|---|---|---|---|
| Zen 2 | 2.00–2.09 | 1.75 (ci95 1.79) | ~31 ns (was 41) |
| M1 | 1.69–1.90 | 1.46 (ci95 1.49) | ~25 ns (was 40) |

Reader scaling (its `warm_scaling` bench, M1): 14.1 → 26.2 → 29.8 →
49.2 M ops/s at 1/2/4/8 threads — positive throughout, unpaired vs the
mutex protocol.

CORRECTIONS this forces on the audit above: the hash was a rock
(~6–14 ns), not gravel — my decomposition missed it because the four
mix rounds inlined partly into `get`'s 18% share and I never split
lookup into hash-vs-probe. "The system carries no unidentified
warm-path bottleneck" was falsified by code-reading where sampling
misled. The floor table's lookup row is wrong as stated. The law
survives with new constants (4 KiB small-set: 1 + 31/41.5 = 1.75 —
matches); sira's single-pin exposure drops to ~2.4–2.8%; the crossover
U* shrinks ~25%. The stopping rule's SPIRIT stands — composition
remains the 10× lever — but its evidentiary basis is now "audited and
then independently attacked," which is worth more than the original.

Adoption review items (with the mirror's loom case): probe-length
distribution of the packed hash under sequential `granule_idx`
workloads (one finalizer round on a structured word; table tests pass,
distribution unexamined), and pairing `warm_scaling` against the mutex
protocol for the contention claim.

## Shipping confirmation (sira-dios point proof, external session, 2026-08-18)

The sira-side point-proof profile on the pinned host measured the
model's prediction on the REAL workload: explicit dios frames ≈ 2.1% of
mixed-run cycles ≈ 4.1% of candidate time (`Pool::get` 0.75%, table
lookup 0.75%, epoch begin 0.30%, pin validation 0.15%, guard drop
0.13%) — against our pre-registered 3.2–3.6% additive prediction, with
the residual delta attributed to inlined liveness-lock/hash work plus
cache perturbation. Sira's own stacks dominate as the law requires:
varint decode 19.6%, REMIX partitioning 17.9%, memmove 13.1%, sidecar
lookup 11.0%, lower_bound 5.4%. The two named residuals are exactly
the two fixes in hand (liveness mirror, packed hash) — the shipping
profile independently fingered what the atomic-protocol revision
removes. Projected exposure with it adopted: ~2.5–3% of candidate
time, pre-composition. Caveats: mixed-run attribution assumes paired
symmetry; point shape only — the span regime awaits its own shipping
test. Artifacts live with the sira session (SVG + CSVs retained).

## R7 update (2026-08-19): the aligned format halved the denominator — the law struck as written

`scopes/active/dios-v1/resources/remix-dios-native-experiment.md` (R7,
formal fresh-process pairs, CPU-pinned): the baseline halved —
519 → 245 ns/read (0.4710) — driven NOT by the 4 KiB alignment itself
but by the improved REMIX augmented with dios-shaped metadata: the
native locator (run, file-relative frame ordinal, exact frame-local
entry byte offset) eliminates per-read search/decode discovery.
Alignment is the enabler (stable, per-frame-verifiable coordinates
over an evicting pool), not the driver. Note the model's signature:
the locator is I_p pushed into the index — and being plan-layer
metadata, it is byte-source-agnostic, so the mmap arm inherited the
full win too. Consequence, exactly per
R = 1 + F/(c_m·U): dios's point exposure exploded in relative terms —
all dios arms fail 1.02 (conservative 1.36 → compact typed-lease hint
1.20, fixed work clawed from +92 to +52 ns). The affine diagnostic:
~71 ns candidate fixed work, 18.5 ns saved per returned value — n=1
loses (+52), n=10 wins (−114, the 0.9609 PASS).

SUPERSEDED by this: the "percent-scale, beneath decode noise" exposure
claims above were true of the OLD format's 1.1 µs denominator; the new
format removed the decode that buried the tax. The n=1 budget
arithmetic now reads: pass at 1.02 requires fixed ≤ ~24 ns/query;
the atomic protocol's F ≈ 31 ns is necessary-not-sufficient; the
per-access floor audit therefore says an n=1 PASS cannot come from
protocol alone — it needs cross-query amortization (retention of
known-hot winner frames: the §7 pinned-resident view, exactly the
route this audit reserved for "near bare-load parity"), DIFFERENTIAL
displacement — work only the pool arm can skip, since the locator's
savings were plan-layer and accrued to both arms (candidate: per-
residency verification state a frame can hold and a page cache
cannot), or an owner decision on the n=1 gate. That is SAB006/T018's live question; the profiling-before-
correction discipline in their record matches this audit's own rule.

## Floor claim, second correction (R7 hint path, 2026-08-19): the letter falls to consumer-supplied identity

"F ≈ 41 ns is the floor" held for the ANONYMOUS get — a caller carrying
only a `PageId`. The R7 hint path changes the contract in the direction
the model rewards: the consumer supplies MORE information (a persisted
`NativeLocator` plus a volatile `ResidentHint` carrying frame identity
and a residency stamp), and dios validates instead of discovering —
stamp check + lease-held liveness instead of table lookup + per-access
liveness. Measured: compact hint beats the general guarded get by
42.7 ns/value on the same protocol; the repeated-regime TOTAL residual
vs same-layout mmap is 22.6 ns (epoch/stamp/CLOCK helper cluster ~6 ns
of it); the fresh-child residual is 51.8 ns, of which ~29 ns is the
fresh-vs-repeated regime gap itself — machinery code footprint under
cold predictors/caches, an argument for compact acquisition paths that
the dense-descriptor falsifier independently confirmed (bigger
structures lost despite fewer branches). The floor ARGUMENT survives
verbatim — "cheaper means fewer guarantees or a different contract" —
but the operative contract change was never weakening: it was the
consumer carrying knowledge (locator, hint, lease), i.e. the
information-loss model applied to the acquisition path itself. Their
§13 falsification list is accepted as written, including this one.
Open safety gates for the hint surface (their §14, matching this
audit's own conditions): loom get-vs-retire AND hinted-acquisition-vs-
eviction/reuse; no timing result waives them.

## R8 (2026-08-19): the §7 route reached its predicted endpoint

The resident-set arm (`ResidentSetLease` + precomposed descriptor
stream: retain the selected pages, one sequential walk, lifetime-bound
borrowing, NO per-read guard) — i.e. the pinned-resident view this
audit named "the only route near bare-load parity" — measured, 30
fresh pairs, formal:

| vs | result | gate |
|---|---|---|
| guarded dios | 0.9098 / ci95 0.9121 (−23 ns) | PASS ≤ 0.95 |
| locator mmap | 1.0223 / ci95 1.0242 (+5.04 ns, 2.23%) | FAIL ≤ 1.02 by 0.2–0.4 pts |

Reading: with retention, per-read protection is gone and the residual
~5 ns is descriptor-walk CPU path — the measured full price of owning
residency under the strongest amortization available. The route ladder
is now complete and monotone: anonymous get ~41 ns → atomic protocol
~31 → hint fresh ~52/repeated ~22.6 → composition ~2.4%/value floor →
resident set 2.23% total. Every step was a pre-registered route in
this audit; none was reached by weakening a guarantee.

The fork (owner decision, their own wording adopted): a "deliberate
acceptance-boundary change" if the candidate proceeds at 2.23%. This
audit's recommendation: do NOT decide the boundary on mock evidence —
the arm is a MockDriver upper bound (pool frames mock-backed, mmap arm
real page cache; same bytes, different physical paths) and the
shipping-backend rerun is required anyway; decide there. Standing
safety debt now THREE loom cases: get-vs-retire, hinted-acquisition-
vs-eviction/reuse, retention-vs-eviction/reuse (the resident set also
needs bounded admission — 8,035 retained pages interacts with INV-9
and the memlock ceiling; pinned-frame-retention's budget design is now
load-bearing, not optional).

Strategic summary the numbers now support: warm same-layout, dios sits
2–2.5% behind mmap across ALL shapes (point via retention, ranges via
composition) — the fully-amortized ownership premium — while ahead
6–12× cold, ahead under concurrency, and the locator format itself
(built FOR dios) beat the old mmap path 2×. The stopping rule applied
a second time by owner instruction: the 5 ns floor is not chased.
