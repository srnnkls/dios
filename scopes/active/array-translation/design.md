# Design: array-translation

---

## Problem

| Metric | Current | Target |
|--------|---------|--------|
| Resident 64-byte read, Dios/mmap, 1,024 pages, Zen 2 (`resident_access_ordinary`, 2026-09-14) | 8.148, upper 8.300 (46.2 ns against 5.7 ns) | session read upper <= 1.50 (AT-G1) |
| Whole-granule fold, Dios/mmap, 1,024 pages | 1.72 on M1 (advisory) | session read upper <= 1.10 (AT-G2) |
| Whole-granule fold, Dios/mmap, 256 MiB set (`mmap_tlb_pressure` THP attribution, 2026-08-18) | 1.18 with the hugepage-advised arena, 1.48 without | session read upper <= 1.00 (AT-G3) |
| `get` hit cost | 46.2 ns on Zen 2 | new/old upper <= 0.85 (AT-G4), passed at 0.7539 on 2026-11-17 |
| `get` hit cost after the swap, lean harness | about 28 ns on Zen 2 (scratch, 2026-11-17) | hit-path change/before upper <= 0.85 (AT-G8) |
| Copy-out of 64 bytes against an mmap copy | no verb | upper <= 1.50 (AT-G5) |

A resident read today pays for three things an mmap read does not: a hash probe of
40-byte seqlock cells, a guard with its accounting on every read, and an epoch
publication with a full fence on the first guard. Control arms on Zen 2 put the probe
near 10 ns and the fence near 4 ns of the 40 ns gap. A spike on an M1 Pro
(`benches/hot_read_paths.rs`, branch `spike/hot-read-paths`, 60 interleaved pairs
against mmap) separates them: every path through the probe stays at 6x or worse, the
guard machinery costs about 14 ns over the same probe, and paths that neither probe nor
take a guard land between 1.3x and 1.9x.

## Core insight

A page's identity can be its position. When translation is an array indexed by granule
inside a file slot, a lookup is one load whose address is arithmetic on the request,
the page identity needs no stored key, and a reader can prove what it read from the
entry and the frame's existing generation stamp. The hash, the probe sequence, the
seqlock and the per-frame identity cells all leave the read path together.

## Alternatives

### MMU-backed translation (vmcache)

Reserve address space the size of each file and let the hardware page table translate;
eviction is `madvise`. A spike arm with a plain memory region and a state word per page
measured 1.51 to 1.53 on the point lane, the same band as the array, and 1.01 to 1.03
on the range lane.

Rejected: it gives no point-read advantage over the array, and it gives up the
preallocated registered arena, puts a TLB shootdown on every eviction, ties the granule
to the OS page size and needs an out-of-tree kernel module to scale, as its own authors
and both 2026 follow-up papers report.

### Predictive translation (PrediCache)

Keep the hash table and place hot pages at a frame computed from the page identity, so
the CPU reads the predicted frame while the probe verifies it. The spike arm reads from
the predicted frame (verified in the disassembly) and still measured 6.6 to 6.7 on the
point lane against 6.8 to 7.2 for the plain probe.

Rejected: with the working set in cache there is no load latency to hide and the probe's
own work remains; promotion also copies pages between frames, which conflicts with
retained handles and in-flight destinations. CALICO's comparison reports it 6.5x behind
array translation on sequential scans.

### Caller-held hints (pointer swizzling)

The caller keeps a frame hint per page and the pool validates it. Spike arms measured
1.69 to 1.89 with a stamp check and 1.16 to 1.20 for a retained handle.

Rejected as the general path: every consumer must store and refresh the hints. Retained
handles stay for the pages a consumer chooses to pin.

### Selected: per-file array translation

One entry per granule per file slot, written under the control lock and read with a
single load. Spike arms: 1.54 to 1.61 for a session read, 1.75 to 1.83 with the
per-read liveness check, 1.06 to 1.08 on the range lane. It keeps frames, registration,
CLOCK and EBR, and it removes the table probe from every verb. CALICO (Zhou, Hu, Pavlo,
Stonebraker, 2026) reports the same structure matching or beating MMU translation on
4 KiB pages across scans, point lookups and graph traversal.

Accepted tradeoff: translation memory follows the file extent a pool has touched, not
the frame count (AD-4).

---

## Design

### Entry format

Decision: an entry is an `AtomicU32`, zero for absent and frame index plus one
otherwise. All slots live in one mapping; an entry's address is
`(file slot << file_granules_shift) | granule`.

Motivation: residency is the only fact a lookup needs. Frame counts stay at or below
`2^30` as today, so 32 bits hold the index with zero free for absent and the high bit
spare, and zero-filled lazily backed memory is a valid empty array. A power-of-two bound keeps the address free of a
per-file base load.

Alternatives: a 64-bit entry with a version field, as CALICO and the spike used. A
version of 24 or 32 bits can repeat while a reader without an epoch pin is descheduled,
and widening it to be wrap-free needs 16 bytes per granule. The frame's 62-bit
generation already is the wrap-free witness, so the entry carries none.

Tradeoff: 4 bytes per granule, half the 8 the owner accepted; the spare high bit
remains available to the AT-G1 lever below.

### Reservation and release

Decision: the builder reserves `registered_file_capacity << file_granules_shift`
entries, counted in `u64`, as one zero-vacant mapping made without commit accounting
(`MAP_NORESERVE` on Linux). Publish sets an entry after `Frames::publish`; eviction
clears it before the frame leaves Resident; retirement releases the whole OS pages
inside the slot's range once its last entry is clear. The default bound is `2^22`
granules per file.

Motivation: a reservation at build keeps the address space bound explicit and makes a
failure a build error. The release is a memory give-back only: correctness rests on the
entries already being zero, so a platform whose advice does not discard pages stays
correct.

`MappedArena::try_map` maps writable memory with commit accounting and
`MappedSlice::try_vacant` takes a `u32` length, so the translation mapping needs its
own reservation flag and a wider length. A slot smaller than one OS page shares its page with neighbors and is never
released; a release never rounds outward.

Alternatives: sizing per file at registration (needs the file extent, a per-file bound
and re-registration on growth) and reserving the full `u32` range per slot (no bound).
Both were put to the owner and declined. A default of `2^28` granules reserved 64 GiB
unconditionally, which the owner rejected, and with commit accounting Linux refuses
that mapping on any host with less RAM plus swap. Making a slot writable only at
registration charges registered files alone and adds a failure path and a syscall to
registration.

Tradeoff: at the defaults the reservation is 1 GiB of address space for 64 slots of
16 GiB files, backed only where entries were written; larger files need the bound
raised at build. Under strict overcommit the kernel ignores `MAP_NORESERVE` and a
refused reservation is a build error. A fully scanned 1 TiB file holds 1 GiB of
entries until it retires.

### Session read

Decision: `Pool::session` performs the first-guard protocol once (publish the epoch,
`SeqCst` fence) and holds it as one guard. `SessionFile::page` then loads the entry,
loads the mirror, sets the CLOCK reference and returns a slice borrowed for the session.
An absent entry returns `Absent`; the caller takes the ordinary `get` for the miss.

Motivation: under a published epoch a frame evicted later cannot be reclaimed, and
INV-2 makes a set entry mean "Resident in this page's publication now", so the borrow
is sound without a per-read guard. Holding the session is the same reclamation debt as
holding one guard for the same time, so no new stall class appears.

Alternatives: a guard per read (the spike measures about 14 ns for that machinery on
M1) and submitting misses inside the session (ties I/O progress to a pinned epoch).

Tradeoff: bytes borrowed from a session stay valid but may belong to a page evicted
during the session, exactly as with a held guard.

### File liveness

Decision: every lock-free resolution, `get` included, loads the entry first and the
file-generation mirror second, both `Acquire`, and accepts only when the mirror equals
the requested generation's word. `SessionFile` computes that word once and
`ReadSession::file` asserts pool identity and bounds the slot once.

Motivation: the hash key carried the file generation, so a probe for a retired file
could not match a successor's cell. An array entry is keyed by slot and granule alone.
A retired file with no resident frames completes retirement without waiting for
readers, so its slot can be registered again, and a successor's page published, between
any two steps of a read. Registration stores the successor's mirror word before any of
its entries is set, both under the control lock, so a reader that observed a successor
entry cannot afterwards observe the predecessor's mirror word. The mirror load after
the entry therefore proves the entry belonged to the requested generation.

Alternatives: loading the mirror before the entry, as `get` does today ahead of its
pin, which admits the successor's page under the old identity once the key carries no
generation. Validating once and delaying slot reuse by a reclamation grace period
removes a load per read and changes registration to an operation that can be refused;
it is recorded as the second AT-G1 lever.

Tradeoff: one independent, cache-resident load per read; the spike measures 0.3 to
0.4 ns for it on M1 once identity is hoisted.

### Copy acceptance

Decision: the optimistic attempt of `read_copy` runs: load the entry to find the frame;
load the frame stamp and require Resident; load the entry again and require the same
frame; copy with the audited volatile word loads; take an `Acquire` fence; load the
stamp again and require equality; load the mirror and require the requested generation.
Any failure takes one ordinary `get`. In-process payload writers take a `Release`
fence after claiming a frame and before their first payload store.

Motivation: an equal full-width stamp at both ends means the frame held one publication
across the copy. The entry read between the two stamp loads names the requested page's
frame at an instant inside that interval, and by INV-2 a set entry identifies the
frame's current publication, so that publication is the requested page's. The control
lock orders every writer, and the reader's `Acquire` loads of `Release` stores carry
that order: having observed a stamp, the reader cannot then observe an entry older than
the eviction that preceded that stamp. An `Acquire` load orders only what follows it,
so without a barrier the payload loads could complete after the second stamp load on a
weakly ordered target and a torn copy would be accepted. The reader's fence closes
that, and the writer's fence keeps a claim visible before the payload it permits. On
x86-64 both fences emit no instruction; on aarch64 the reader's is a load barrier.

Alternatives: comparing only the entry before and after the copy, which the owner first
selected. Without a version the comparison cannot see a page evicted and republished
into the same frame, and a version narrow enough to fit can repeat. WRC-D1's bracket
with atomic identity cells is exact as well and costs two more dependent loads and an
atomic identity representation per frame.

Rejected as well: compiling the optimistic attempt for x86-64 only and sending every
other target to the guarded fallback, which the owner declined in favor of the fences.

Tradeoff: two stamp loads and one fence beside the two entry loads. The identity cells
keep their present representation because no unguarded reader touches them. The
owner's approval of the two fences amends the inherited contract, whose no-fence rule
still bars the `SeqCst` epoch fence from the optimistic path.

### Get hit path

Profile of 2026-11-17 on Zen 2, lean harness, 28 ns per hit: `pin_live` about 12 ns
(a third at function entry, a fifth writing the 48-byte guard into the return slot),
the caller unwrapping `Result<Get, GetError>` about 6 ns, the `get` wrapper about 3 ns.
No single operation dominates; the epoch fence is about 1% of `pin_live`.

AD-17 keeps the public signature and changes how a hit reaches the caller (FR-12):

- The fast path is inlinable into a caller of another crate and ends in one of two
  ways: a hit value of at most two machine words (frame reference and reader slot),
  from which the guard is built in the caller's frame, or a tail call to one
  out-of-line function that owns miss, busy, stale-file and bound errors.
- `pin_live` loses `#[inline(never)]`; the slow path keeps `#[cold]` and
  `#[inline(never)]`.
- The first reference to a frame after a CLOCK sweep cleared its bit (speculation
  consume, bit store, counter) stays one cold out-of-line function shared with the
  session read; a hit on an already referenced frame loads the bit and branches.
- Reader ownership, pool identity and the granule bound are each asserted or checked
  once on the hit path; the translation entry is not bound-checked a second time
  behind the public bound check.
- The INV-7 order (epoch pin, entry load, mirror, pin commit) and every atomic
  ordering are unchanged; the Loom cases of AT2 and AT4 cover the changed path.

Rejected: a second public hit-only verb (surface without a new capability, FR-12 is
met without it) and removing assertions (AGENTS.md safety rules).

### Inherited copy-out contract

The volatile-word exception, its single module, the same-width atomic writers, the
confinement rule, mirror-only liveness, the output validity rule, the absence of a
CLOCK touch on the optimistic path and the Miri and Loom model limits carry over from
`scopes/draft/warm-read-copy-out/design.md` as decided in WRC-D1. Two parts of that
design fall away: the optimistic table hint probe, replaced by the entry load, and the
atomic `ExactPageCells` representation. The writer inventory, the ordering audit and
the Miri model limits for this scope are recorded in [audits](audits.md).

---

## Complexity

| Dimension | Before | After | Delta |
|-----------|--------|-------|-------|
| Loads on a hit before the payload | hash of four fields, 1 to n seqlock cells of 40 bytes, two sequence loads each | one 4-byte entry load | probe removed |
| Translation state | 40 bytes per table cell, `2 x frame_count` cells | 4 bytes per touched file granule | follows file extent |
| Bounded loops on the hit path | linear probe, seqlock spin up to 1,000,000 | none | two loops removed |
| Read verbs | `get`, `get_with_hint`, retained handle | plus session read and `read_copy` | two verbs added |
| Unsafe surface | arena mapping, guard deref | plus volatile word loads in one module | one audited module |
| Modules | `table.rs` | `translation.rs`, `session.rs`, `read_copy.rs` | one removed, three added |

---

## Implementation

| Phase | Tasks | Observable result |
|-------|-------|-------------------|
| Contracts | AT1 | Frozen manifests, writer inventory, ordering audit and capture commands exist before code |
| Translation swap | AT2, AT3 | Every existing test passes without `PageTable`; AT-G4 and AT-G7 pass |
| Session read | AT4, AT5 | Session cases pass under Loom and compile-fail; AT-G1, AT-G2, AT-G3 pass |
| Copy-out | AT6, AT7 | Copy cases pass under Loom, Miri and the confinement rule; AT-G5, AT-G6 pass |
| Get hit path | AT9 | The fair `get` loop holds no call into the crate on a hit; AT-G8 passes |
| Campaign | AT8 | Evidence, translation-memory report and the sira handoff are recorded |

AT-G1 levers, applied only after the failing arm is profiled and the owner approves the
amendment: first, keep the CLOCK reference in the entry's spare bit so a read touches
no second array (spike: 1.34 to 1.43 against 1.54 to 1.61 without liveness); second,
validate liveness once per file and delay slot reuse by a reclamation grace period.

---

## Verification

### Test Cases

| Test | Validates | Expected |
|------|-----------|----------|
| Loom: publish racing a session read | INV-2 | A hit returns only fully published bytes |
| Loom: evict, reclaim and reuse racing a session read | INV-2, INV-3 | No borrowed slice names a reclaimed frame |
| Loom: retirement of an empty file and reuse of its slot, landing after the epoch pin and before the entry load, for `get`, a session read and an optimistic copy | INV-7 | `StaleFile` or fallback; never the new file's bytes |
| Retire one of two files whose slots share an OS page | INV-2 | The other file's resident pages still hit |
| Readahead window and explicit prefetch at the bound, for a middle slot and the last slot | INV-8 | Window ends below the bound; explicit page counted rejected before any lookup; no other slot's entry or frame is touched |
| Loom: reuse of the same frame for another page and for the same page during a copy | INV-2, INV-4 | The copy is rejected or holds one publication of the requested page |
| Writer assertions at publish and evict | INV-1, INV-2 | Entry absent before set, set before clear, frame state as required |
| Build with refused reservation | INV-6 | `PoolBuildError`, nothing leaked |
| Zero-alloc harness over hits, session reads and copies | INV-5 | No allocation on either backend |
| Out-of-bound granule on every verb | INV-8 | `GetError::BeyondFileBound` |
| Confinement fixtures in a pool file and in `src/mock.rs` | INV-9 | Off-module volatile read and seam use rejected |
| Generation test and unchanged `FrameState` layout | INV-4 | Existing test passes untouched |
| DRP and RC regression lanes | INV-10 | Bounds hold as recorded |

---

## Design Notes

- Spike numbers are M1 medians over 60 interleaved pairs on a 1,024-page set; the bench plan lists them as advisory and binds nothing to them.
- The spike's copy arms moved between 6.5 ns and 14.3 ns for unchanged source across two builds, so the copy path has no projection; AT-G6's floor arm exists to attribute that cost on `nix`.
- Sources: [CALICO](https://arxiv.org/pdf/2604.00423), [Predictive Translation](https://db.in.tum.de/people/sites/zinsmeister/papers/predictive-translation.pdf), [vmcache](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/_my_direct_uploads/vmcache.pdf).
