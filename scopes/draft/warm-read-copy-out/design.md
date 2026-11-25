# Design: warm-read-copy-out

Revision 3 incorporates the owner decisions in [validation.yaml](validation.yaml).
The volatile-word exception is selected; its implementation audit and tests remain
future deliverables. [Bench plan](../../../benches/plans/warm_read_copy_out.md).

## Bounded call and validation order

Proposed public surface: `Pool::read_copy(reader, page, offset_bytes: u32,
output: &mut [u8]) -> Result<ReadCopy, GetError>`, with `ReadCopy::Copied`,
`Pending(PendingToken)` and `Busy`. Names are proposed new API. Validate pool and
reader identity, nonzero output length fitting u32, and checked range within the
configured granule. The call borrows the pool/reader; pool storage stays live.

1. Check the requested file generation through the existing Acquire mirror and
   use the bounded optimistic table hint probe below. A miss, unstable cell or
   invalid hint takes the ordinary get path once.
2. Read the frame stamp with Acquire and require Resident. Read every exact
   identity component with Acquire and compare primitive values with the requested
   PageId; do not construct an invalid typed identity from an uninitialized cell.
3. Copy the requested bytes with the audited aligned volatile-word primitive.
4. Re-read the exact identity components and frame stamp with Acquire, then read
   the file-generation mirror with Acquire. Accept only if both identity snapshots
   equal the request, the two stamps match and are Resident, and the mirror still
   names the requested live file generation. Otherwise take get once.

This preserves the owner-required bracketing order. The target/compiler audit
must establish the payload ordering that those operations need; an Acquire load
is not described as an arbitrary full barrier. A result must have a linearization
point within the validated residency/liveness interval. Stale mirror after a
failed attempt follows get's existing StaleFile outcome. Concurrent retirement
after successful validation may linearize after the returned old-file copy.
Mirror-only liveness needs no ResidentFileLease because no observation or borrow
outlives this call.

Fallback Hit copies under the ordinary guard and drops it before return; fallback
Pending/Busy/error preserves the existing protocol. Only Copied makes output
valid. Every other return leaves unspecified output, since an abandoned attempt
may have filled it. No loop retries the optimistic attempt, no implicit polling,
no retained caller pointer and no newly allocated storage.

## Optimistic table hint probe

The existing `PageTable::lookup` calls `Cell::read`, whose Acquire fence and
up-to-1,000,000 spin retries are outside this fast-path contract. Add an internal
probe in `src/pool/table.rs` using the existing atomic cells and table layout.
Visit each cell at most once, bounded by table capacity. For each candidate cell,
load its sequence with Acquire, abandon on an odd value, read the atomic key and
frame fields, and re-read the sequence with Acquire. Abandon on change; an empty
cell or no matching primitive key falls back. There is no `Cell::read`, explicit
fence, spin loop or retry of an unstable cell in this probe. Validate the raw
frame index against frame capacity before indexing. Ordinary lookup, insertion,
deletion and the table representation remain unchanged.

This result is only an untrusted frame hint. The no-fence cell snapshot is not a
proof of coherent table identity or absence: mixed atomic fields can only cause
a false hint or fallback. The subsequent exact atomic frame identities, Resident
stamp bracketing and file mirror establish copy acceptance. Do not construct a
typed FileId from unchecked cell fields. A deterministic test pauses a cell writer
at an odd sequence, observes optimistic abandonment without a spin, then releases
the writer before exercising the ordinary fallback. The fallback retains get's
existing ownership, fence and bounded-retry behavior; it is not a fast success.

## Audited word primitive and writers

The proposed exception home is `src/pool/read_copy.rs`. It owns raw volatile loads
and the matching in-process atomic writer helpers. On the audited 64-bit targets,
payload accesses use aligned u64 words and AtomicU64 relaxed stores. Round a byte
range's source start down and end up to 8-byte word boundaries, load those words,
and copy only the requested bytes to output. The granule is a multiple of 8, so
these covering loads never cross its boundary; the maximum extra read is 7 bytes
at either end. No unguarded `&[u8]` or `&mut [u8]` is manufactured for the arena.
Unsupported target/word assumptions use the ordinary get fallback rather than an
unreviewed load width. Target and generated-instruction identities are recorded.

Inventory every in-process read-frame write, including Frames::fill,
transfer_mut-based mock/test fills, partial transfers and initialization. Replace
shared/reachable writes with same-width relaxed atomic stores under the existing
unique InFlightFrame token. For an unaligned partial write, the unique writer
updates only its requested bytes through aligned atomic word load/store helpers;
no overlapping narrower atomic or non-atomic write is permitted. Setup-only writes
must be proven unreachable by a concurrent read_copy. Mock file backing and
caller-owned output are distinct from read-frame payload and are not gratuitously
rewritten. A writer inventory and a tail witness prevent hidden mutable-slice
bypasses. Ordinary guarded readers keep their EBR ownership unchanged.

Confinement is a checked acceptance condition. Extend
`tests/arch3_sync_alias.rs` with a separate volatile-exception rule. Its own
source walk covers every Rust file under `src/`, including `src/mock.rs`,
`src/driver.rs` and in-module test fills; it must not reuse the pool-only walk.
The existing sync-alias rule keeps its pool-only scope and diagnostics owner.
Reject payload
`read_volatile` uses outside `src/pool/read_copy.rs`; restrict calls to the new
unguarded `Frames::optimistic_payload_ptr` seam to that module, apart from its
single raw-address-only definition in frames.rs. WRC1 records the existing guarded,
DMA and exclusive-setup access inventory so these do not become blanket exceptions
for new unguarded dereferences. The audit must account for every changed raw arena
access. Pin the scan with one off-module volatile read and one off-module seam-use
fixture that fail, with both a pool file and a non-pool file such as mock.rs
in the source-walk fixture, alongside an accepted owner-module fixture. This bounded source
guard catches the named bypasses; it does not claim to prove arbitrary Rust aliases.
No new public raw-pointer API is introduced.

Replace the non-atomic ExactPageCells representation with initialized atomic
primitive components covering the complete FileId and granule index. Writes remain
serialized by the existing token; reads before/after copy use Acquire. The stamp
protocol rejects mixed identity snapshots during reuse. All proof-bearing atomics
use crate::sync and the actual production synchronization seam in Loom. The fixed
identity metadata cost is reported per frame; no shadow payload or allocation is
introduced. FrameState stays AtomicU64 with two state bits and a 62-bit generation,
incremented only on Resident publication and checked before exhaustion.

## Scope of the accepted exception and validation

Sören accepts the volatile-word platform exception in ARCH-3 style: one audited
module, a named invariant, explicit compiler/target assumptions and a recorded
model boundary. Kernel writes through io_uring and the eager syscall are outside
the abstract-machine test model. That boundary does not claim that all concurrent
memory accesses become defined Rust operations.

The [Rust volatile contract](https://doc.rust-lang.org/std/ptr/fn.read_volatile.html)
and [atomic memory model](https://doc.rust-lang.org/std/sync/atomic/index.html)
distinguish volatile access from atomic synchronization. A volatile read racing
an atomic store is still a mixed non-atomic/atomic conflict; same-width atomic
writers alone do not make that pair a Miri proof. Retain the owner's decision
verbatim in validation.yaml and record exactly which writer-discipline cases
Miri covers. A model that substitutes atomic payload reads can check the stamp
algorithm but is not the production volatile primitive. No silent cfg(miri)
reader substitution, suppressed race failure or portable-soundness claim is
permitted. Any required qualification that the selected primitive cannot satisfy
is a concrete review/owner issue, not permission to reinterpret the decision.

Loom validates the stamp/identity/mirror transitions and rejected-copy outcomes
with an explicitly labeled payload abstraction. The native compiler/target audit
must cover the real volatile loads and recheck ordering, byte-exact copied output,
reuse and retirement. Miri covers the actual atomic writer helpers, alignment,
partial-word stores, provenance/lifetime and ordinary guarded fallback, with its
concurrent-copy coverage and limitations reported explicitly. ASAN and both-backend
fault/terminal checks retain their existing roles; they do not prove ordering.
No validation is marked complete merely because the exception is owner-approved.

## Costs, alternatives and integration

The selected path performs one probe, two stamp/identity snapshots, mirror checks
and O(requested bytes) word loads, then at most one guarded fallback. Success
publishes no epoch, fence or CLOCK reference. Record atomic identity footprint,
writer costs and observed fallback rate; the uninstrumented primary carries no
new diagnostic counters. Optimistic-only pages remain ordinary CLOCK victims;
WRC4 owns the separately identified pressure-characterization artifact.

Guarded copy is the control and fallback. An all-atomic payload read, new fence,
shadow payload or storage/EBR/FrameState amendment would change the selected
contract and requires an owner disposition; it is not an automatic fix. The
existing mirror/alignment/Loom prerequisite is already implemented.

WRC-D2 leaves sira's SAB006 whole-lookup upper <=1.02 with sira after Dios lands.
The 24 ns budget motivated the target and is not a separate Dios assertion.
Dios neither creates a replacement denominator nor owns a sira adapter in this
scope. WRC-D3 adopts the physical-core/2,097,152-iteration DRP revision; the frozen
runner and fresh base evidence must be prepared before binding warm/scan gates.
