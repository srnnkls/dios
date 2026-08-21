# R1-R7 evidence digest

Date: 2026-08-19. This resource is an index, not a replacement for the
canonical ledger at
`scopes/active/dios-v1/resources/remix-dios-native-experiment.md`.

## Source identity

Both Dios experiment worktrees start at
`1004a2e6fcae0bcc9552dc3211c2416e388a250d`:

- `.worktrees/experiment-read-protocol-tightened` — atomic liveness,
  `ReaderSlot` alignment, and rejected packed hash.
- `.worktrees/experiment-sira-point-proof` — R7 hint ancestry, now mixed with
  later R8 resident-set edits in `src/lib.rs` and `src/pool/mod.rs`.

The tracked file `resources/r7-source.diff` is 16,708 bytes and has SHA-256
`16748dd827b1958b6889535f7244fb8ffe767dac674d587f9fee18738be4a967`.
It is byte-identical to the historical artifact captured under the R8 profile
directory, but is itself a repository-local base-to-R7-only patch. Applying it
to the clean base reproduces the four Rust bindings. Installing the separately
tracked 3,907-byte `resources/sira_native_hint.md` (SHA-256
`337ecf1aafdbded58c38cca5efdf62056e139e2521744d305b43b962ebd62ecf`)
reproduces the fifth, canonical plan binding:

| R7 file | SHA-256 |
|---|---|
| `src/lib.rs` | `d56aa1eff2fe1741f6b9e7665cf07c264bb2398778f69d2191b20178c6183ca0` |
| `src/mock.rs` | `443d06229a59c36d5edc7a887a473a7fc371eecfcfd9a8458062b662594a7d0f` |
| `src/pool/frames.rs` | `8b122ffaa0ae198e4ec9dd68999ac0fbe8232cf5998b6a76d041cbfc4d76372c` |
| `src/pool/mod.rs` | `213697747590988533f829e5c6846b3106f9ae293ce0570cbf4d432dabaf1d10` |
| `benches/plans/sira_native_hint.md` | `337ecf1aafdbded58c38cca5efdf62056e139e2521744d305b43b962ebd62ecf` |

The current point-proof worktree hashes for `mock.rs`, `frames.rs`, and the
plan still match R7. Its `lib.rs` is now
`0ec04cc4db4ee88378bb439d6020db2ddd93cb21ce8d7315f491414d2b5c60c0`
and `pool/mod.rs` is
`8fa1db952a9e2579564480e78e7724757b0fe637f2d86eea936a8009a2e8d26d`;
those are not R7 inputs.

## Atomic-path evidence

The combined experimental candidate moved the warm pool/mmap bracket:

| Host | Base band | Candidate | Candidate CI95 upper |
|---|---:|---:|---:|
| Zen 2 | `2.00-2.09` | `1.75` | `1.79` |
| M1 | `1.69-1.90` | `1.46` | `1.49` |

Unpaired M1 reader throughput rose `14.1 -> 26.2 -> 29.8 -> 49.2 M ops/s`
at 1/2/4/8 threads. This motivates, but does not close, the real paired
contention gate.

The packed hash is structurally rejected. At 1,024 slots with 16 interleaved
files, mean probes rose `1.441 -> 1.709` and p99 `5 -> 11`. The full-width
one-round escalation removed that structural failure; its remaining small-table
p99 deltas were one integer probe, which is why DRP-G1 freezes an absolute
`+1` small-table p99 bound and still requires real-backend speed.

Because that revised integer p99 bound was selected with the calibration
results visible, it is not sufficient evidence for the full-width candidate.
DRP-G1 therefore ran the exact independent holdout frozen in design.md. The
candidate passed all 18 calibration rows but failed the independent holdout,
so the speed lane was forbidden and not run. The selected implementation
remains the current four-round hash.

## R7 ladder

All formal R7 rows used 8,192 deterministic n=1 reads, checksum
`17131094364009660908`, CPU 0 on `nix`, and 30 order-alternated fresh-process
pairs.

| Paired experiment | Base ns/read | Candidate ns/read | Ratio | CI95 upper | Verdict |
|---|---:|---:|---:|---:|---|
| current-prefix mmap / native-locator mmap | 519.178 | 244.537 | 0.4710 | 0.4731 | repeated format work displaced |
| locator mmap / conservative Dios upper | 254.333 | 346.507 | 1.3616 | 1.3856 | failed 1.02 |
| locator mmap / ordinary get + direct guard decode | 252.411 | 335.570 | 1.3280 | 1.3478 | failed 1.02 |
| locator mmap / initial typed lease hint | 252.433 | 324.028 | 1.2818 | 1.3050 | failed 1.02 |
| locator mmap / compact typed lease hint | 253.702 | 305.527 | 1.2045 | 1.2229 | failed 1.02 |
| ordinary get / compact hint | 354.725 | 311.983 | 0.8813 | 0.8980 | passed 0.95 materiality |

This historical result supported the production materiality rerun; it did not
itself establish parity or adoption. The prototype was feature-mock-only and
had no get/retire or hinted-reuse Loom proof. The binding production result is
recorded below.

## Binding negative evidence

- Dense/vacant hint: five-pair smoke residual `+74.124 ns/read`; long-run
  residual `+26.474 ns/read`, `3.882 ns/read` worse than compact. Rejected.
- Epoch fence weakening: unsound by the existing store-buffer adversary and
  worth only a few nanoseconds in prior measurements. Rejected.
- Old candidate-labeled point-tax binary: source/binary identity unavailable.
  Advisory only.
- MockDriver: deterministic correctness substrate only. It cannot close a
  shipping-backend or contention gate.
- R8 resident-set/retention: separate mechanism, scope, invariants, and gates.
  Excluded from every implementation and claim here.

Raw R7 CSV hashes and retained local/remote locations remain in the canonical
ledger. This digest intentionally does not duplicate R8 performance results.

## Binding production decision

DRP-G3 passed and selects the public `lease_file`, `ResidentFileLease`,
`resident_hint`, `ResidentHint`, and `get_with_hint` contract. The materiality
ratio was `0.9273`, with a one-sided 95% CI upper bound of `0.9298` against the
frozen `0.9500` threshold. `get_with_hint` owns fallback to ordinary
`Pool::get`; hints are advisory and opt-in. An ordinary warm `Pool::get` hit
remains the default no-hint path and performs no hint-specific branch, load,
store, or RMW.

The file lease protects one exact file-generation lifetime. It does **not**
retain or pin frames, and pages covered by a live lease remain normally
evictable. This result makes no R8 resident-set, frame-retention, or retention-
policy claim.

All remaining binding performance gates passed without changing their frozen
thresholds:

| Gate | Ratio geomean | CI95 upper | Threshold | Result |
|---|---:|---:|---:|---|
| G2 warm ordinary | `0.9999` | `1.0024` | `1.0100` | PASS |
| G2 cycling reuse | `0.9992` | `0.9996` | `1.0100` | PASS |
| G3 hint materiality | `0.9273` | `0.9298` | `0.9500` | PASS |
| G4 ordinary/base, eight threads | `0.8715` | `0.9282` | `1.0000` | PASS |
| G4 ordinary, eight threads/one thread | `0.2827` | `0.2991` | `0.5000` | PASS |
| G4 hinted, eight threads/one thread | `0.3393` | `0.3558` | `0.5000` | PASS |

Both G2 lanes passed the zero-allocation validator, and the cycling lane's
production proofs passed. After pool construction, the zero-allocation proof
covers ordinary warm hits, hinted hits, stale-hint fallback, lease acquire/drop,
and retirement progress. The final correctness matrix passed eager-inline zero
allocation `21/21`, Linux io_uring zero allocation `21/21`, Loom `7/7`, Miri
`10/10`, and ASan `14/14`.

The measured candidate is
`28e57aa2670aec9ee28b95688cff4ad793d3cdd0`. The reviewed DRP009 handoff point
was published at `c652ef0764e6e783befbbb124ac3db4d31e8be6d`; this identifies the
published Dios branch and is not evidence that Sira consumed it. Exact binary,
runner, base, input, artifact, and checksum identities are in
[`gate-results.yaml`](gate-results.yaml).

The companion `sira-aligned-buffers:SAB008` endpoint remains pending. Dios
therefore does not claim end-to-end Sira closure here, and DRP010 remains open
until that result can be consumed read-only.
