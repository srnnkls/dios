---
created: 2026-09-14
status: draft
issue_type: Feature
revision: 3
---

# Scope: warm-read-copy-out

Successor: [array-translation](../../active/array-translation/scope.md) absorbs this scope (its AD-6); WRC-D1, WRC-D2 and WRC-D3 carry over there except as its AD-7 and AD-13 amend.

Reduce resident independent-read overhead with a bounded copy-out verb beside
`Pool::get`. Revision 3 incorporates Sören's recorded WRC-D1/D2/D3 decisions in
[validation.yaml](validation.yaml). This turn authorizes documentation and review;
product implementation still requires an explicit implement invocation.
The [bench plan](../../../benches/plans/warm_read_copy_out.md) precedes code.

Dios owns the resident 64-byte projection at candidate/mmap upper <=1.5, the
candidate/optimistic-floor ratio against 1.2, and the unchanged DRP regression
bounds. Sira owns its unchanged SAB006 whole-lookup 1.02 gate after Dios lands.
The 24 ns figure is a motivating budget note, not a separate Dios assertion.

## Baseline and completed prerequisite

Use `origin/main` at `c1733cf819efaeb140c0b408178b3db1c47e05d6`,
the squash-merged RC6 product from [PR #17](https://github.com/srnnkls/dios/pull/17),
with the actual 512 KiB speculative byte budget and 4 KiB granules. After Sören
invokes implement, create the warm implementation branch from this main commit.
The canonical main checkout owns this scope and its product baseline.

`Pool::get` already uses the Acquire file-generation mirror; `ReaderSlot` is aligned to 64 bytes; the production-linked Loom case
`mirror_get_racing_retire_and_slot_reuse_never_admits_reused_bytes` already covers
the mirror, retirement and reuse. RC5 retained 20 passing Loom cases. Preserve
these mechanisms and evidence; the old tightened-protocol experiment is historical.

## Owner-selected contract

1. One requested byte range in one page is copied into caller-owned output. Make
   one optimistic attempt, then at most one ordinary get fallback. There is no
   internal spin, wait, epoch publication, fence, CLOCK touch or allocation on the
   successful optimistic path. Misses expose the existing PendingToken/ready/poll
   protocol; Busy and StaleFile preserve their meanings.
2. WRC-D1 selects an owned, audited ARCH-3-style volatile-word exception. Copy
   payload through aligned word-width volatile loads. Bracket the copy with
   Acquire reads of the packed frame stamp and atomic exact-identity cells before
   and after, followed by an Acquire file-generation mirror read. Success requires
   the same Resident stamp, exact requested identity in both snapshots, and the
   requested live file generation after the copy. A fingerprint alone is never
   an identity witness. Mirror-only liveness is selected; no ResidentFileLease.
3. Every in-process writer of read-frame payload, including mock and test fills,
   uses relaxed atomic stores of that same word width. Exact-identity cells become
   atomics. The exception and raw payload operations live in one audited module;
   proof-bearing atomics follow the existing crate::sync/Loom discipline. The
   [design](design.md) separates the accepted platform exception, the stamp model,
   and the limits of Miri's writer validation. Kernel writes remain explicitly
   outside the abstract-machine model. No model stand-in becomes a production
   soundness claim.
4. Preserve two state bits and 62 generation bits, generation advance on Resident
   publication, and panic before wrap. This owner decision permits atomic identity
   storage and atomic in-process payload writers; it does not change FrameState,
   EBR, ring topology, guard accounting or ordinary-get ownership. Byte slices
   borrowed from the arena exist only on the ordinary guarded fallback.
5. On Copied, the complete requested output is valid. On Pending, Busy or error,
   output is unspecified and callers must not consume it. An invalid optimistic
   attempt may have written bytes before fallback. A successful guarded fallback
   replaces the complete output before dropping its guard. No caller-buffer
   pointer is retained for DMA or after return.

## Acceptance and verification

Stable exact residency returns the requested bytes. Reuse or retirement during
copy returns only a result linearized under the accepted stamp/identity/mirror
contract, or takes the existing fallback; successor identity and mixed bytes
must never be accepted. Wrong pool/reader or an out-of-page range asserts as a
programmer error; operating errors remain values. Zero-length ranges assert.

WRC1 materializes the module boundary, platform/compiler-order audit and writer
inventory before GREEN. WRC2 adds a confinement check in
`tests/arch3_sync_alias.rs` over all `src/**/*.rs` for the volatile exception
and its unguarded pointer seam, including non-pool mock/driver and in-module fills, plus one contended-cell probe-abandonment witness. Existing guarded access
remains separately inventoried. WRC2 pins representative stable-copy, reuse, retirement,
identity-collision, range/tail and fallback falsifiers. Loom exercises the real
stamp/identity synchronization. Miri qualifies the stated in-process writer and
pure-memory ownership coverage; it must report its actual model limits. Atomic
writers alone are not evidence that a concurrent volatile reader is race-free
under Rust. A failing required safety check or a proof/qualification contradiction
returns to the owner rather than silently changing the primitive or hiding a test.
Both backends retain syscall fault, terminal, ASAN, lifetime and zero-allocation
coverage appropriate to their paths. No new code or test pass is claimed here.

Dios adoption requires WRC-G1/G2/G4 and its safety checks. WRC-G1/G2
validate rich process rows against the frozen per-arm manifest and raw work
witnesses before deriving paired CSVs for the shared ratio harness. WRC-G3 is the external
sira acceptance handoff after landing, with no new denominator or Dios-side
24 ns assertion. Preserve the historical SAB006 failure and all old DRP results.

## Protocol and sequencing

WRC-D3 is decided in the [DRP owner amendment](../../../benches/plans/dios_r1_r7_read_performance.md#owner-amendment-2026-09-14-drp-g4-eight-thread-protocol):
physical CPUs 0-7, siblings 32-39 idle, BINDING_ITERATIONS 2,097,152 for the
8-thread and scaling lanes (about 200 ms), bounds 1.00/0.50 unchanged. A matching
frozen-runner protocol revision and fresh base baseline must exist before the
first binding campaign; old-protocol samples are not paired with new candidates.
Those are pending execution deliverables, not an unresolved protocol choice.

After separate implement invocations, warm work starts first and the
[shared-pool I/O study](../shared-pool-io-scaling/scope.md) may run in parallel on
its frozen RC6 baseline; scan-efficiency and poll-wait work follow. This does not
authorize simultaneous perf campaigns on nix. The optimistic path deliberately
omits CLOCK recency writes: optimistic-only hot pages remain evictable. WRC4
records its named pressure/fallback characterization without changing that policy.
