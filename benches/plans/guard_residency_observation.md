# Guard-bound residency observations

Add an optional observation of the physical residency protected by a live
FrameGuard. Ordinary get and guard construction remain unchanged. Consumers
may memoize verification only for equal observations under the same pool and
exact PageId; an Evicting guard returns no observation. No allocation or unsafe
code is added, and existing EBR, file retirement and retention protocols remain.

## Protocol and gates (recorded before implementation)

Linux nix, Threadripper 3970X, performance governor, THP never. No build,
profiling or test workloads overlap timing. Preserve normal-release baseline
74d765a and candidate binaries, compiler features, hashes and raw measurements.
At least 30 alternating fresh-process pairs, untimed warmup, same pinned inputs.

Ordinary warm Pool::get latency candidate/baseline CI95 upper <= 1.03 using
existing pool warm benchmark. Sira production retained-snapshot 1M random
150-byte values, eight shards, FullRewrite: candidate/2ec6851 point latency
CI95 upper <= 0.80 and candidate/2a5e6bf baseline <= 1.00. Checksums must match.
Use Dios shared paired comparison, never custom gate statistics:

    cargo bench --features bench --bench compare -- <paired.csv> <threshold>

Escalation on failure: keep draft, inspect residual CPU cost and remove or
revise observation/cache implementation; never loosen verification, epoch,
retention or memory-budget contracts. Correctness tests cover same-generation
stability, eviction while a guard lives, reload changing observation, and
foreign-pool/wrong-page guard rejection. Existing resident hint Loom and
zero-allocation regressions remain required. No extra loads on ordinary get.

## Validation status

Mac: full mock-enabled suite passed, strict Clippy and rustfmt passed, three
new guard-observation tests passed under Miri. The existing Loom hinted-pin vs
eviction/two-advances/reuse schedule passed. The extended mock and shipping
backend allocation tests prove guard-bound observation allocates nothing.

Linux gates remain pending. This is a draft dependency change and must not be
adopted on the strength of the Mac diagnostics. Source transfer to the Linux
host was stopped by automatic approval review, and an explicit destination/
payload confirmation is pending. No gate has been relaxed or claimed passed.

The Sira integration tests rejected a persistent file lease: it delayed
explicit registration retirement beyond the last value guard. The observation
therefore uses only the existing live guard and exact PageId, with no file
lease argument or lifetime extension. This changes no get/retirement protocol.
The same performance gates remain pending.
