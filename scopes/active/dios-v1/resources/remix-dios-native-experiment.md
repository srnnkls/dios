# REMIX-native aligned-buffer experiment ledger

Date: 2026-08-19. This is the numeric and provenance record for the
retrofitted T019--T023 work and the in-progress T018 R7 point-path work. It records failed gates as well as favorable
results. Raw artifacts remain outside git because the physical stores are
approximately 0.9 GiB per representation.

## Bottom line

- No Dios shipping branch changed. The dependency worktree is based on
  `1004a2e6fcae0bcc9552dc3211c2416e388a250d` and now carries the isolated R7
  feature-mock lease/residency-hint prototype.
- Sira's prototype is based on adaptive-execution commit
  `36211b452cd31980a8123f02ca0fa993e7ae2759`, not the later adaptive branch
  head. It is test/experiment code and is not wired into the shipping segment
  writer or reader.
- The 5M-row structural result is favorable: an n=1 winner always needs one
  aligned frame, while an n=10 result needs a mean of 1.390747 frames and
  never more than two in 8,192 sampled windows.
- The corrected Threadripper replacement preflight is split: n=10 passes the
  1.02 gate at ratio 0.9609 / CI upper 0.9615, while n=1 fails at ratio
  1.1103 / CI upper 1.1408. The breaking format therefore is not approved for
  production by this evidence.
- R7 removed Sira's repeated format work first: an exact native locator cut
  the mmap arm from 519.178 to 244.537 ns/read. General Dios `get` remained
  32.8% slower than that control. The compact typed-lease/native-hint path
  materially beat general `get` at 0.8813 / CI upper 0.8980, but still failed
  locator-mmap parity at 1.2045 / 1.2229. It is not a DIO-G10 pass.
- The independent point-tax experiment shows the observed cost is statistically
  visible, not noise. The one-frame file labeled tightened costs 31.550918
  ns/get, ratio 1.0382 / CI upper 1.0387, but its old binary/source binding was
  not retained; it is advisory rather than proof of the preserved candidate
  checkout. The architectural opportunity remains work displacement and lease
  amortization, not pretending the cost is zero.

## Canonical repo-local worktrees

| Purpose | Path | Branch | Base/HEAD |
|---|---|---|---|
| Aligned format and REMIX-native prototype | `/Users/srnnkls/projects/sira/.worktrees/experiment-sira-aligned-buffers` | `experiment/sira-aligned-buffers` | `36211b452cd31980a8123f02ca0fa993e7ae2759` + working tree |
| Additive point-tax proof | `/Users/srnnkls/projects/sira/.worktrees/experiment-sira-dios-point-proof` | `experiment/sira-dios-point-proof` | `36211b452cd31980a8123f02ca0fa993e7ae2759` + working tree |
| Dios shipping base and R7 feature-mock point prototype | `/Users/srnnkls/projects/dios/.worktrees/experiment-sira-point-proof` | `experiment/sira-point-proof` | `1004a2e6fcae0bcc9552dc3211c2416e388a250d` + working tree |
| Candidate atomic-table protocol source | `/Users/srnnkls/projects/dios/.worktrees/experiment-read-protocol-tightened` | `experiment/read-protocol-tightened` | `1004a2e6fcae0bcc9552dc3211c2416e388a250d` + working tree |
| Existing post-v1 API branch, recreated from stale temp record | `/Users/srnnkls/projects/dios/.worktrees/feat-dios-api` | `feat/dios-api` | `d4203d95378490edb692a9aea091c4ba617105ed` |

At the revision-13 snapshot, the first worktree added 33 tracked lines to `remix.rs`, one module entry, and
three untracked experiment files: 3,791 lines of implementation, 1,705 lines
of white-box tests, and a 521-line amended bench plan. The point-proof
worktree adds 243 benchmark lines, one dev-dependency line, a 214-line net
lockfile expansion (218 additions/four deletions), and an 18-line plan. R7
extends the aligned worktree with its exact-arm profiler and native-locator
arms, and extends the Dios point-proof worktree only behind the mock feature;
the live working trees, rather than those historical line counts, are the
authoritative experiment source. The
temporary source clones were
moved to macOS Trash after byte-identical SHA-256 checks and successful
compile/test checks; the raw evidence directories were retained.

## Protocol rationale and prior warm-path evidence

The measured approximately 41.5 ns shipping warm acquisition is the combined
cost of obligations mmap does not enforce at its raw dereference point:

| Obligation | Approximate measured cost/observation |
|---|---|
| page to frame translation | approximately 9 ns |
| typed file/version liveness | a few ns; lock-free in the measured path |
| read stability against eviction/reuse | approximately 6--10 ns |
| eviction/reference accounting | approximately 1 ns and already elided on the profiled hit path |

The largest sound local saving found by the audit was approximately 4 ns.
Removing the approximately 2--4 ns ordering fence was rejected as unsound.
CLOCK work was already absent from the hot trace. Refcounts and hazard
pointers were more expensive; QSBR changes the fixed-pool progress contract.
Consequently the experiment retained frame read-stability and tested whether
REMIX could reduce acquisitions and displace Sira work.

Historical economic context for that decision: adaptive Sira's borrowed point
path measured approximately 0.815 us/get; the estimated Dios exposure was
approximately 3--7% of a real point get; THP contributed roughly 30 ratio
points in the separate translation experiment; and explicit residency had
already produced approximately 6--12x cold-path wins. These values motivate
amortization/displacement, but they are not substitutes for the paired format
gate below.

The shipping flamegraph attributed approximately 0.75% to PageTable lookup,
0.30% to `ReaderSlot::begin_pin`, 0.08% to file liveness, and 0.05% to CLOCK.
The larger Sira-side surfaces in the same profile were
`ReaderSidecar::artifact` 11.04%, block-cache lookup 1.62%,
`verify_or_strip` 0.66%, prefix value-bound decode 1.29%, and hashing 1.35%.
The flamegraph is
`/private/tmp/sira-dios-point-proof.OqT4Wx/sira-dios-shipping.svg`, SHA-256
`832ea2dfac351afe2a562e032d662ca1443bca6d1935961808374f5fd340bc65`.

## Additive point-tax proof on `nix`

Protocol: Sira adaptive HEAD, 1,000,000 warm point gets per paired sample, 30
paired samples. This deliberately retained Sira's mmap/artifact work and
added Dios, so it is an upper bracket rather than a migration result.

| Path | Geomean candidate/base | one-sided 95% CI upper | observed ratio range | additive ns/get |
|---|---:|---:|---:|---:|
| shipping, one frame | 1.0619 | 1.0623 | 1.059854--1.064136 | 51.277405 |
| shipping, two frames | 1.1047 | 1.1050 | 1.102587--1.106816 | 86.715258 |
| candidate labeled tightened, one frame | 1.0382 | 1.0387 | 1.035768--1.041229 | 31.550918 |
| candidate labeled tightened, two frames | 1.0793 | 1.0796 | 1.076776--1.081330 | 65.096559 |

Every candidate-labeled one-frame sample exceeded 3.5%, so that observed cost
is not statistical noise. The old run did not retain a binary/source identity,
however, so “tightened” is a collection-time label rather than independently
auditable attribution to the preserved uncommitted Dios worktree. For the
observed n=10 mix and a measured 1,556 ns Sira range:

| Projection | Additive ns | Share of 1,556 ns |
|---|---:|---:|
| shipping | 65.12464 | 4.185% |
| candidate labeled tightened (advisory) | 44.65878 | 2.870% |
| same candidate with a hypothetical 3% common-epoch recovery | 43.31902 | 2.784% |
| 2% budget | 31.12 | 2.000% |

Raw matched CSV hashes are:

- shipping one frame: `584d7f2f4cdffd48791f6be20bf3c9778352abd3e1844536e5b4899ec83c2984`
- shipping two frames: `1b2ba7c3ddcc4b286e2191951ad908e2988c603a590eb6e2175be8615e808066`
- tightened one frame: `b1de6cb740ce3996b9f42a583ddf0533b0fefe01f7f3c05fba4627702bd3ea04`
- tightened two frames: `caabbfcec24d49bcf79f8a6cf23f3af94ffaa9a580aacfa05cdf8f91c0046a74`

Two additional unique n=1 series are retained but excluded from the selected
matched-pair projection: default `sira_dios_point_1p.csv` is ratio 1.066112,
CI upper 1.0664, +54.976137 ns/get, SHA-256
`52a324f4922c09e0a120a8cede3ad083a656ae1b3845791400b9f373fcc267b9`;
the explicitly named upper bracket is 1.064603 / 1.0650, +53.410965 ns/get,
SHA-256
`58c9e13728292e9d39fde7bee4ba191f4b454b0894dd82b28fc175505b335ffe`.
They were not the selected same-session matched shipping/candidate pair.
The default two-frame file is byte-identical to the shipping-matched file.

The post-hoc point inventory binds every retained Sira/candidate source file,
CSV hash, and byte length while explicitly recording that the candidate binary
identity is unavailable:
`resources/point-tax-retrospective-manifest.json`, 6,308 bytes, SHA-256
`a713872b259c453f0a27dd18110c6741505be1084a1c7eb130ff45b52d006caf`.
Its retained candidate-source Dios microbench has 40 pairs, ratio 1.4631 and
CI upper 1.4926. It passes the underlying mmap-warm-path absolute sanity bound
of 3.0, but the table-tightening relative no-regression verdict is not
auditable because its corresponding baseline CSV was not retained. It is not a
Sira point arm and does not repair the missing old binary binding.

The derived projection JSON is
`/private/tmp/remix-native-evidence-r6/protocol-projections.json`, SHA-256
`466321cf905b009f879c303646358ea2aa31b0641530711f65c8657fdc1db936`.

## Breaking aligned-prefix format prototype

The representation antecedent was adaptive AE010. On the same canonical
24-byte-key/150-byte-value corpus, the fixed-width candidate was approximately
4.2% slower at n=10 and 15.6% slower at n=256, used 0.99% more bytes, and
pre-resolving its minor recovered at most 1.7%. It therefore remains projection
F only, with its 22-row/152-byte-padding geometry; aligned prefix is candidate
B.

The prototype uses no compatibility reader. Each physical residency and
verification unit is exactly 4,096 bytes:

- 16-byte typed `SAP1` header;
- frame-local prefix/restart payload;
- zero padding;
- a four-byte CRC32C at bytes 4,092--4,095 covering bytes 0--4,091;
- eight frames as the approximately 32 KiB encoding/prefetch group;
- REMIX coordinate `(run, file-relative frame, exact entry byte offset)`;
- per-run repacking, including tombstone positions;
- direct read verifies CRC before interpreting the header and validates the
  decoded suffix against REMIX's expected full key.

For the canonical 24-byte key, the frozen boundary contract is a 4,040-byte
ordinary value in one frame and a 4,041-byte value in two independently
verified extent frames. Extents are bounded to 16 frames.

The O path derives requests on demand with zero persistent bytes, reuses fixed
scratch, resolves one handle at a time, restores logical result order, and
retains no Dios `FrameIdx` or epoch identity. P is a control, not the selected
representation. A typed verified-frame state permits warm decode without
repeating CRC verification.

Source identities after the final rewarm correction:

- plan Git blob: `db4756581ecc48d4f12e80fe8703c743d2b0eecb`
- plan SHA-256: `7a0c298f950fac8ed9f1dba375ff21f721ab5b9ae421488563922209bbbeb5af`
- `remix.rs`: `fcb2c1aaa461a1480f0395ad737effe2ec8c13901b80f27463aa344bf78cf231`
- `white_box/mod.rs`: `0c090f0f263376cc59dfcae4905e49512f7cc4a2ea924b40ac1a7432cbd96a39`
- `remix/dios_native.rs`: `f3f47852656ba49c802f9116914ce9669d5180409a95445b3dafa506a806bf36`
- `white_box/remix_dios_native.rs`: `076763b26543122783af4e0254ce17747ede0d37021e9abaaa2c840bda2aa58e`

The frozen plan identity itself is SHA-256
`102d959d2d20dfc6326e240467796b0e2294ecfa8c05c72af99aa99c88a276cc`
and names experiment source
`36211b452cd31980+working-tree-e0fb7a54f1331c0e`. Its original
`source-files.sha256` manifest predates the final physical-reader test and
same-arm-rewarm correction: it records `dios_native.rs` as
`586cd876d0bacb019df0693e35ce0924deedeb85afd43073380f1a7eaacd36d7` and
the white-box file as
`479530590e69070e9fd69918e048527483cc0d08766c72e0c96d67d1b5af5a39`.
The corrected artifact directory originally lacked a replacement source
manifest. Revision 13 adds a clearly post-hoc immutable binding of final source,
plan, corpus manifest, artifact hashes, and byte lengths at
`resources/remix-dios-native-rewarm-manifest.json`: 3,442 bytes, SHA-256
`f0184597734b25ef67f0cce8b845bc0b0820df1cad6c3a8e7d20ec3d5b0ca2f7`.
This is adequate for retaining a targeted preflight, but T018's new plan/run
must create its exact per-artifact source manifest as part of the formal
namespace rather than reconstructing it afterward.

## 5M-row structural result

Corpus: selected generation 50, eight shards, one L1 run per shard, 5M rows,
24-byte keys and 150-byte values. There are 16,384 observations: 8,192 at n=1
and 8,192 at n=10. A/O/P logical checksum mismatches: zero.

| Metric | n=1 | n=10 |
|---|---:|---:|
| observations | 8,192 | 8,192 |
| aligned frames, mean | 1.000000 | 1.390747 |
| aligned frame p50/p75/p90/p95/p99 | 1/1/1/1/1 | 1/2/2/2/2 |
| one-frame observations | 8,192 | 4,991 |
| two-frame observations | 0 | 3,201 |
| two-frame fraction | 0% | 39.074707% |
| observations above two frames | 0 | 0 |
| current block visits, mean | 1.000000 | 1.044922 |
| current block-visit p50/p75/p90/p95/p99 | 1/1/1/1/1 | 1/1/1/1/2 |
| current 4 KiB granules, mean | 9.004150 | 9.364010 |
| current-granule p50/p75/p90/p95/p99 | not separately retained | 9/9/9/9/17 |
| aligned covered/useful value bytes | 27.306700x | 3.797670x |

O persistent bytes are zero. Its measured initialized scratch capacities were
1,174 bytes at n=1 and 2,740 bytes at n=10. P used 40 descriptor bytes per
shard, or 0.00006392--0.00006414 bytes per live key, on this one-L1 control;
that storage result is not a churn-corpus verdict.

Structural artifacts:

- `structural.jsonl`: 16,384 rows, SHA-256
  `7d68b38a0f003a2adae0c9693e4d1acfa442609ef90465b3eeca9e01d53f8601`
- `inventory.json`: SHA-256
  `636870afe4c59c282378229fa96e01f72bf020de5e997c6f1d1b7f7db8fc406a`
- `structural-summary.json`: SHA-256
  `a3f504614a0b22635e702a5ec945be647d64bafcd296b3049118f7010f053b10`
- source-file manifest: SHA-256
  `e0fb7a54f1331c0ec8c5b7df4a9006ed0f6cdb3fbeeb496424f604c469843e5f`
- corpus-file manifest: SHA-256
  `7fa249ad9c4c4781650ce4a60baa2035c9e5cbb30d8796e4150fff1f21fb13ae`

## Store size

The current selected stores total 882,572,267 bytes. The aligned-prefix
prototype stores total 890,445,824 bytes: +7,873,557 bytes, ratio 1.008921147,
or +0.892114708%. This is materially smaller than abandoning prefix
compression, but it is still a real storage cost.

## Physical replacement preflights

Every arm processes 8,192 deterministic queries per repetition. n=1 returns
8,192 logical values per arm; n=10 returns 81,920. There are 30 paired
repetitions and exact checksums `17131094364009660908` for n=1 and
`4967664852030774370` for n=10. Base is current-prefix mmap; candidate is the
aligned-prefix direct reader. These are targeted in-process preflights, not the
plan's required fresh-child shipping gate.

| Host/protocol | Length | geomean candidate/base | CI95 upper | 1.02 verdict |
|---|---:|---:|---:|---|
| Mac, original preflight | 1 | 0.9806 | 1.0548 | fail/noisy |
| Mac, original preflight | 10 | 1.0167 | 1.0454 | fail/noisy |
| nix, original alternating run | 1 | 1.0165 | 1.1044 | invalid/order-confounded |
| nix, original alternating run | 10 | 0.9444 | 0.9450 | pass, but retained only as confounded evidence |
| nix, same-arm rewarm before timing | 1 | 1.1103 | 1.1408 | fail |
| nix, same-arm rewarm before timing | 10 | 0.9609 | 0.9615 | pass |

Dividing each raw repetition by its 8,192 queries and taking the geometric
mean gives the absolute scale. At n=1, current is 476.474 ns/query and aligned
is 529.038 ns/query, a 52.564 ns increase. At n=10, current is 2,921.224
ns/range and aligned is 2,806.923 ns/range, a 114.301 ns decrease
(292.122 versus 280.692 ns per returned value). This is not the earlier
approximately 3.9% point-tax result: that advisory result divided a
candidate-labeled 31.551 ns increment by the approximately 815 ns full Sira
point path and lacks a retained binary/source binding.

A two-point affine diagnostic over these n=1/n=10 deltas gives approximately
71.105 ns of candidate fixed work and 18.541 ns saved per returned value. It
is a profiling locator, not a causal decomposition or a gate: SAB006/T018 must
profile the n=1 arm before choosing a correction.

The first nix n=1 run had a near-deterministic alternating second-arm penalty.
It was retained and not overwritten. The corrected run warms the exact arm
immediately before measuring it. Its n=10 result is approximately 3.91%
faster, while n=1 is approximately 11.03% slower. The n=1 failure blocks the
format as currently implemented; n=256 and n=4,096 were not run after that
failure.

Artifact SHA-256 values:

| Artifact set | Summary | n=1 CSV | n=10 CSV |
|---|---|---|---|
| Mac original | `cdef53fd91ff5b38be0d3625cd3d623be751519b708359970f1395c811604d2b` | `db8690bdb04f3931e32e6bfbb6eb633e81d9a649d1b8c897444d14252570a469` | `fd1f2ba2ce04a9e21b4c3988812352459a1eab2dbdb0989ca24f08a1a347c86f` |
| nix original | `46b5b3651dd5b080e1df8890d868db5e4f834fba26f21f326b6f85f3055f29d3` | `864ccefea4359ba8bca676bf30a2acee666991c44c00941daef3650f2823ebaa` | `0502911291341d6e5e198766f66db5346e5eed5546673161afaebcc8ea0e8760` |
| nix rewarm | `ab5d538d12e387608000fe23fd69cbd853ba266dc458edebfe338a33eaa66532` | `4232900d89e14d85546837a0fadbdbf712857b0835cf7f735aa3bf4f06a62fdf` | `dcddf650cbcf3acc45ddada02fd14153c5d8e39ee185ae7d808881680f503627` |

Local evidence root:
`/private/tmp/remix-native-evidence-r6`. Remote retained evidence root:
`/home/srnnkls/build/sira-remix-native-artifacts/36211b4/20260818-r6` on
`nix`. Remote source was
`/home/srnnkls/build/sira-remix-native-36211b4`; the selected corpus was
`/home/srnnkls/build/sira-readprof/fp-full/sira`.

## Verification record

- Relocated aligned worktree focused suite: 17 passed, zero failed, two
  explicitly ignored real-corpus tests.
- Relocated full library run on the final prototype: 1,368 passed, zero failed,
  14 ignored, in 42.41 seconds.
- Relocated point-proof worktree: `cargo bench -p sira --bench
  redb_workload --no-run` succeeded against the repo-local Dios symlink.
- Relocated candidate Dios worktree: relevant pool suite 26 passed, zero
  failed; its transferred files match the discarded clone byte for byte.
- Formatting was clean before relocation. Strict Sira clippy remains blocked by
  pre-existing warning debt; the focused experiment module had no strict-path
  diagnostics after its local fixes. The relocated focused build still emits
  the repository's existing warnings, so zero-warning closure is a production
  integration task, not claimed here.

## R7 n=1 format and native-residency experiment

R7 is based on the same Sira commit and exact 8,192 deterministic point
queries. Every arm produced checksum `17131094364009660908`. Runs were pinned
to CPU 0 on `nix`; formal rows use 30 order-alternated fresh-process pairs and
the shared paired-log compare harness. Absolute ns/read values are arithmetic
column means divided by 8,192; ratios are paired-log geomeans. Absolute means
from different rows are not subtracted across sessions.

| Paired experiment | base ns/read | candidate ns/read | candidate minus base | ratio geomean | CI95 upper | gate |
|---|---:|---:|---:|---:|---:|---|
| current-prefix mmap / aligned native-locator mmap | 519.177686 | 244.536702 | -274.640983 | 0.4710 | 0.4731 | format work displaced |
| locator mmap / conservative Dios upper | 254.333089 | 346.506885 | +92.173796 | 1.3616 | 1.3856 | FAIL 1.02 |
| locator mmap / general `get` with direct guard decode | 252.410754 | 335.570292 | +83.159538 | 1.3280 | 1.3478 | FAIL 1.02 |
| locator mmap / initial typed-lease hint | 252.433020 | 324.027515 | +71.594495 | 1.2818 | 1.3050 | FAIL 1.02 |
| locator mmap / compact typed-lease hint | 253.701737 | 305.527327 | +51.825590 | 1.2045 | 1.2229 | FAIL 1.02 |
| general direct `get` / compact hint, dedicated materiality run | 354.724703 | 311.982703 | -42.742000 | 0.8813 | 0.8980 | PASS 0.95 |

The first row is the largest result: repeated frame-header/footer validation,
coordinate recovery, and key/value copying were format-path work rather than
an inherent Dios tax. `NativeLocator { file_id, page_ordinal, byte_offset }`
removes them while retaining exact one-frame bytes. The conservative upper arm
then performs one ordinary shipping `Pool::get` but reads the value through
mmap; direct guard decode removes that duplicate dereference and recovers
9.014258 ns/read. The initial hint moves file liveness to a typed lease and
replaces the general table lookup with a pool-minted volatile frame/generation
observation. Compacting that observation to 16 bytes and fusing it with the
verified payload end recovers another 18.500188 ns/read. In the independently
paired materiality run the final hint removes 11.87% of general-get time, so it
is meaningful protocol improvement, not parity.

The stable REMIX locator never contains a `FrameIdx`, pointer, epoch, or raw
residency generation. The separate volatile hint is valid only while its typed
file lease is held. Every hinted pin publishes the existing reader epoch,
checks the exact frame residency stamp, touches CLOCK, and returns the normal
`FrameGuard`; a stale observation falls back to ordinary `get`. Frame CRC and
typed header verification are associated with the acquired residency
generation, and the value is decoded and hashed while the guard is live.

The compact fresh-process delta is larger than the steady-state repeated
delta. Over 6,000 repetitions of the same 8,192 points, locator measured
228.428555 ns/read and compact hint 251.020588 ns/read, +22.592034. The matched
cycle profiles differed by approximately 98 cycles/read; the complete
epoch/stamp/CLOCK helper represented only about 6 ns/read. This cache-state
sensitivity implicates the additional random volatile-descriptor access in
the fresh pass rather than another large synchronization primitive.

For scale only, the binding fresh-process delta of 51.825590 ns projects to
6.36% of the previously measured 815 ns adaptive point path, or 5.18% of a
1 us point. The repeated diagnostic delta of 22.592034 ns projects to 2.77%
and 2.26%, respectively. These are arithmetic projections, not integrated
Sira measurements. The delta is statistically resolvable rather than timing
noise; only a new end-to-end point arm can establish how much is hidden or
displaced by the real REMIX/search/decode path.

There is nevertheless substantial format-level headroom relative to today's
current-prefix mmap path: cross-session arithmetic gives
`305.527327 / 519.177686 = 0.588483` for compact hint over current-prefix
mmap. That quotient is suggestive only. It is not a paired comparison and
cannot close a gate; the paired same-layout compact-hint/locator failure
remains binding. The next decisive measurement is one integrated,
fresh-child current-mmap versus aligned-Dios point arm.

The one apparent remaining large instruction cluster was deliberately
falsified. Replacing `Option<FormatDiosHintPage>` with a dense always-present
descriptor made the five-pair smoke 251.398071 versus 325.522217 ns/read,
+74.124146. In the 6,000-repetition diagnostic it measured 227.935919 versus
254.409928, +26.474009, worsening the residual by 3.881975 ns/read versus the
compact representation. Rust's nonzero-stamp niche had already encoded the
option without added size; the dense/vacant variant was reverted. No further
trace cluster worth the plan's 10 ns/read threshold remains.

Raw paired CSV SHA-256 identities:

- current/locator: `42fb1b383dd2fe1c31558389a923f10b6d2e822002f1d763feee7d2c109c6881`
- locator/conservative upper: `cf641938b39d6874e94eda6ff9025cc80e87b03abf3b9208ade015889a460c0e`
- locator/direct: `fa132bd862ddf154143c6ab43206dafacd1f32ca89b36e341f78cd5f859b72b6`
- locator/initial hint: `9e2f15f2353a31a7f0c148b11890bf4fbca522b400bc3a940c3da23adcc9f622`
- locator/compact hint: `82d13d6d76595b42020c587d2be21927919045462e0e24038e0075c3c39f1d08`
- direct/compact-hint materiality: `707a3f93a19660e0859a4f151d5dcd9ecc39977dc8de0b54bde271221f296841`
- rejected dense smoke: `c4af062d8727d1551355c0aa8f19487fb4f72df7d4ecf8f820100f59f73ba99e`

Post-measurement source binding for the retained compact-hint tree:

- Sira R7 plan: `2a3b4e984cb0ac74c3edbcfeb2e5bc8e1b0be34cca1b15d46d1363e22b1408a0`
- Sira `remix/dios_native.rs`: `0f6b081584cd4c230291fec93c1fd829aac512ee9a5922db37b0e4d9080c6110`
- Sira white-box launcher: `24e3b1208ecea62f948568ee8b9fee4c1b77af7b5e7f2def25ba75ae4910a4f0`
- paired runner: `d7b8745a9c927fae8c32f782cc8ef3a8344df9d5b1dffae33126759902b8ef1a`
- Sira `Cargo.toml`: `ea09fdbd78df2d6fb391e3dda75c81a9aed5a7e86944ce530363f43cfd377947`
- Sira lockfile: `3138d81a5a6a76e278e2667dd89617bbdd109545d9769317762a3f7e876320eb`
- Dios hint plan: `337ecf1aafdbded58c38cca5efdf62056e139e2521744d305b43b962ebd62ecf`
- Dios `lib.rs`: `d56aa1eff2fe1741f6b9e7665cf07c264bb2398778f69d2191b20178c6183ca0`
- Dios `mock.rs`: `443d06229a59c36d5edc7a887a473a7fc371eecfcfd9a8458062b662594a7d0f`
- Dios `pool/frames.rs`: `8b122ffaa0ae198e4ec9dd68999ac0fbe8232cf5998b6a76d041cbfc4d76372c`
- Dios `pool/mod.rs`: `213697747590988533f829e5c6846b3106f9ae293ce0570cbf4d432dabaf1d10`

This binding was recorded after timing and is therefore provenance, not
preregistration. The thresholds came from the two plans written before their
respective implementations; no threshold was relaxed after observing data.

The retained local root is
`/Users/srnnkls/projects/sira/.worktrees/experiment-sira-aligned-buffers/.artifacts/r7`;
the remote source/artifact root is
`/home/srnnkls/build/sira-remix-native-36211b4` on `nix`. The pre-code hint
plan is `benches/plans/sira_native_hint.md` in the Dios point-proof worktree.
The experiment compiles in release mode and exact checksums pass, but the hint
surface is feature-mock-only. Production adoption is blocked until Loom covers
both file-lease acquisition versus retire/file-slot reuse and hinted pin
publication versus eviction/two epoch advances/frame reuse. Numeric success
cannot substitute for either proof.

## Decision carried forward

The data supports retaining the format grammar, exact REMIX coordinates,
native locator, one-handle reusable scratch, compact typed-lease hint as a
research candidate, and the n=10 selective-read direction as design inputs.
It does not support shipping the prototype unchanged: the native hint
materially improves general get but misses locator mmap by 20.45%, and its
safety proof is unfinished. The next design must explain and remove the
remaining n=1 regression, then run an independent
fresh-process current-prefix versus aligned-prefix gate at n=1, 10, 256, and
4,096. No attempt to shave EBR guarantees is justified by these measurements.
