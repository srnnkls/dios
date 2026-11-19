---
created: 2026-11-15
status: active
issue_type: Feature
---

# array-translation

## Goal

Replace the hash page table on the pool hit path with per-file array translation, and
add a session read and a bounded copy-out read over it, so resident reads meet the mmap
gates in the [bench plan](../../../benches/plans/array_translation.md).

## Non-Goals

- The frame arena, registered buffers, ring topology, cold read path, readahead and coalescing stay unchanged.
- EBR reclamation, the `FrameState` word and the CLOCK policy and storage stay unchanged.
- Translation memory is not reclaimed per entry group; AD-5 records what reopens that.
- No MMU-backed translation, predictive frame placement or caller-held swizzled pointers.
- Resident hints, file leases and retained handles keep their surface; removing them is a later owner decision.
- No multi-page contiguous slice; a range read stays one slice per granule.
- No sira adapter and no change to sira's SAB006 gate.

## Invariants

Existing design this change MUST preserve. Changing one requires a scope update.

| ID | Invariant | Check |
|----|-----------|-------|
| INV-1 | The control-plane lock is the sole writer of translation state (dios-v1 AD-4 single-writer discipline) | Loom cases in `tests/loom_pool.rs`; `tests/arch3_sync_alias.rs` |
| INV-2 | An entry names frame F for page P only while F is Resident in P's publication: set after `Frames::publish`, cleared before the frame leaves Resident | Loom publish/evict/read cases; assertions at both writers |
| INV-3 | A reader publishes its epoch and `SeqCst`-fences before reading residency; no frame is reclaimed while such an epoch precedes its eviction, and no file slot is closed or registered again while such an epoch precedes the store that cleared the slot's liveness mirror | Existing epoch Loom cases plus the session cases in AC-9 |
| INV-4 | `FrameState` keeps two state bits and a 62-bit generation that advances on Resident publication and panics before wrap | `publish_bumps_the_generation_and_abort_keeps_it`; no edit to the word layout |
| INV-5 | No hit path allocates, locks or enters the kernel | `tests/zero_alloc.rs` on both backends |
| INV-6 | Every capacity is fixed at build; only the backing of touched translation pages grows afterwards (AD-4) | Build-time reservation test; `tests/pool_allocation_failure.rs` |
| INV-7 | A stale `FileId` never observes another file's pages: an epoch-pinned session accepts a file only if the mirror, loaded `Acquire` after its pin, names the requested generation, and reads its entries without a further mirror load; every other lock-free resolution loads the entry and then the mirror, both `Acquire`, and accepts only when the mirror names the requested generation; registration stores the mirror before any entry, and a slot is reused only after the grace period of INV-3 | Retire and slot-reuse Loom cases for `get`, session read and copy; `tests/pool_retire.rs` |
| INV-8 | Operating errors are values and programmer errors assert | `tests/public_api.rs`; fault cases in `tests/faults.rs` |
| INV-9 | Proof-bearing atomics route through `crate::sync`; volatile payload reads live only in `src/pool/read_copy.rs` | `tests/arch3_sync_alias.rs` with the confinement rule of AC-14 |
| INV-10 | Adopted gate bounds stay as recorded: DRP-G2, DRP-G4 and the RC gates | AT-G7 in the bench plan |

## Conventions

Every axis resolves to a provider rule, a decision stated here, or `n/a`. The exemplar is the existing code new code mirrors.

| Axis | Resolution | Exemplar |
|------|------------|----------|
| Data & types | `AGENTS.md` Rust and Safety sections with the loqui Rust guide: newtype indexes, explicitly sized integers, zero-is-vacant mapped storage | `src/pool/frames.rs` |
| Errors | `AGENTS.md` Safety: operating errors as `Result` values, programmer errors assert | `src/pool/mod.rs` (`GetError`, `PoolBuildError`) |
| Effects | Hit paths perform atomic loads only; every mutation happens under the control lock | `src/pool/mod.rs` (`pin_owned`, `evict_frame_internal`) |
| State | Single-writer atomics, `Release` stores and `Acquire` loads through `crate::sync` | `src/pool/epoch.rs` |
| Dependencies & boundaries | Stdlib only; `unsafe` isolated in the mapping module and `src/pool/read_copy.rs` | `src/allocation.rs` |
| Naming | `AGENTS.md` Naming: no abbreviations, qualifiers last (`file_granules_max`) | `src/pool/mod.rs` (`PoolBuilder`) |
| Config & tooling | Builder option validated at build into a `PoolConfigError`; mise tasks unchanged | `src/pool/mod.rs` (`PoolBuilder::registered_file_capacity`) |
| Testing | `AGENTS.md` Verification: fault injection, Loom, Miri, ASAN, compile-fail, zero-alloc | `tests/loom_pool.rs`, `tests/guard_compile_fail.rs` |
| Prose & output | Bench plan per `benches/plans/TEMPLATE.md`; evidence under `benches/evidence/array_translation/` | `benches/plans/readahead_coalescing.md` |

## Decisions

- AD-1: Per-file array translation replaces `PageTable`; rejected MMU-backed translation, predictive translation and caller-held hints: [design](design.md#alternatives).
- AD-2: An entry is one `AtomicU32` per granule, zero for absent and frame index plus one otherwise; rejected a 64-bit entry carrying a version: [design](design.md#entry-format).
- AD-3: A pool-level `file_granules_max` set at build sizes every file slot's array (owner, 2026-11-14), default `2^22`; rejected sizing per file at registration and reserving the full `u32` range.
- AD-4: Translation backing may grow with touched file pages instead of frame count (owner relaxation of capacity-at-startup, 2026-11-14, for the translation array only).
- AD-5: A file's translation backing is released at retirement and per-lane translation memory is reported (owner, 2026-11-14); per-group hole punching returns only by owner decision on that report.
- AD-6: The scope delivers the swap under every existing verb, a session read and the copy-out read (owner, 2026-11-14); `warm-read-copy-out` is absorbed and its WRC-D1, WRC-D2 and WRC-D3 decisions carry over except as AD-7 amends.
- AD-7: Copy acceptance observes the entry inside the frame-stamp bracket and drops the identity cells from the read path; rejected an entry-only bracket and WRC-D1's identity-cell bracket: [design](design.md#copy-acceptance).
- AD-8: A session read is resident-only and borrow-scoped; rejected per-read guards and miss submission inside a session: [design](design.md#session-read).
- AD-9: A session validates file liveness once, when it opens its view of the file, after its epoch pin; reuse of a retired file's slot and the OS close of its handle wait until the global epoch has advanced twice past the store that cleared the slot's mirror, so a held view can never resolve a successor's entry (owner, 2026-11-19, AT-D18, AT-D19, AT-D21). `get` and the copy read keep entry-then-mirror on every call. Rejected: liveness on every session read (the first design of this scope), a generation tag in spare entry bits, a per-file holder count, and stale-on-next-read semantics: [design](design.md#file-liveness).
- AD-18: `Pool::session_file` opens a one-file session in one call, with every assertion and check of `Pool::session` followed by `ReadSession::file`; `ReadSession` stays for sessions over several files (owner, 2026-11-19, AT-D22). Rejected: the CLOCK reference bit in the translation entry, which measured no gain and would make readers write translation memory.
- AD-10: `PageTable`, `page_hash`, the DRP-G1 probe binary and the `table_population` bench leave the product; their recorded evidence stays as history.
- AD-11: Gates AT-G1 to AT-G7 carry the owner-selected bounds of 2026-11-14 in the bench plan.
- AD-12: The translation mapping is reserved without commit accounting (`MAP_NORESERVE` on Linux) and sized by the two builder options (owner, 2026-11-15); rejected a 64 GiB default reservation and commit per file at registration: [design](design.md#reservation-and-release).
- AD-13: The optimistic copy takes one `Acquire` fence before its second stamp load and in-process payload writers take one `Release` fence after the claim (owner, 2026-11-15, amending the WRC no-fence contract, which still bars the `SeqCst` epoch fence); rejected an x86-only fast path: [design](design.md#copy-acceptance).
- AD-14: Benches against mmap follow the fairness contract of the bench plan (owner, 2026-11-17): the legacy resident lanes are frozen as harness-shape lanes, gated comparisons run in a fair-lane module with a plain-array floor arm, the ratio bounds stay and ns per read is reported beside them; rejected fixing the legacy lanes in place, gating on an inlined fold and gating on absolute nanoseconds.
- AD-15: AT-G3 claims a hugepage-backed arena against ext4 mmap on 4 KiB pages under a recorded THP `madvise` host profile, with an ungated equal-reach row (owner, 2026-11-17); rejected equal reach only and demoting the gate to characterization.
- AD-16: AT-G1, AT-G5 and AT-G6 pass only on three builds with perturbed code layout (owner, 2026-11-17); rejected deciding them on one build.
- AD-17: The scope adds a `get` hit-path change after the session read, gated by AT-G8 at 0.85 (owner, 2026-11-17): a register-sized hit result, an inlinable fast path with the slow path out of line, and each hit-path assertion made once; rejected carrying it inside the session work.

## Context

### Key Files

| File | Lines | Role |
|------|-------|------|
| `src/pool/table.rs` | 1-360 | Hash page table being replaced |
| `src/pool/mod.rs` | 1665-1760 | `Pool::get` hit path |
| `src/pool/mod.rs` | 2279-2304 | Retirement eviction of a file's frames |
| `src/pool/mod.rs` | 2376-2392 | `pin_owned`: epoch pin, lookup, CLOCK reference, guard commit |
| `src/pool/mod.rs` | 2435-2484 | Publish and evict writers of the mapping |
| `src/pool/mod.rs` | 3019-3043 | File-generation mirror (`file_live_word`) |
| `src/pool/prefetch.rs` | 251-273, 450, 821 | Prefetch lookups and eviction |
| `src/pool/epoch.rs` | 178-250 | `begin_pin`, `commit_pin`, `release_guard` |
| `src/pool/frames.rs` | 1-260 | Frame state word, payload access, arena mapping |
| `src/allocation.rs` | 100-300 | `MappedSlice` and `ZeroVacant` lazily backed storage |
| `src/pool/loom_model.rs` | 300-1300 | Loom model of the table protocol |
| `benches/mmap_workloads/resident.rs` | 120-270 | Resident lane arms |
| `benches/mmap_tlb_pressure.rs` | 1-200 | 256 MiB working-set lane |
| `benches/hot_read_paths.rs` (branch `spike/hot-read-paths`) | 1-760 | Spike that produced the advisory numbers |

### Constraints

- Product baseline is `main` at `ade5310`.
- Perf gates run only on `nix` under the bench plan's protocol; macOS numbers are advisory.
- The AT-G7 DRP lanes need the frozen-runner protocol revision and a fresh base from the DRP owner amendment of 2026-09-14 before their first binding campaign.
- `file_granules_max` is a power of two so an entry index is `(file slot << shift) | granule`, computed in `u64`, with no per-file base load.
- A slot's range is released only in whole OS pages inside it; a slot smaller than one OS page is never released.

### Data Model

| Entity | Purpose | Key Fields |
|--------|---------|------------|
| `Translation` | One reserved, lazily backed mapping holding every file slot's array | `AtomicU32` entries in a mapping whose length and index are `u64`, `file_granules_shift: u32`, `registered_file_capacity: u32` |
| Entry | Residency of one granule of one file slot | `0` absent; otherwise `frame index + 1` |
| `ReadSession` | Epoch pin shared by many reads of one reader | reader slot borrow, pool borrow, not `Send` |
| `SessionFile` | Liveness-validated view of one file inside a session | entry range of the slot, arena geometry, reader slot |
| `FileSession` | One-file session opened in one call; owns its epoch pin | as `SessionFile`, plus the pin |

## Requirements

### Functional Requirements

- FR-1: `get`, `get_with_hint`, `ready`, prefetch, publish, eviction and retirement MUST resolve and maintain residency through `Translation` and no other map, and every lock-free hit MUST validate the mirror after its entry load, or, inside a session, under its epoch pin when the session opens its view of the file (INV-7).
- FR-2: `PoolBuilder::file_granules_max` MUST bound the granule index of every file, default `2^22`; a zero or non-power-of-two value MUST fail the build as `PoolConfigError::FileGranulesMaxNotPowerOfTwo`, a `frame_count` above `2^30` as `PoolConfigError::FrameCountAboveTranslationLimit`, and an entry count that overflows `u64` bytes as `PoolConfigError::TranslationUnrepresentable`.
- FR-3: Pool construction MUST reserve the whole translation mapping without commit accounting and return a `PoolBuildError` when the reservation fails.
- FR-4: Every read verb given a granule index at or above `file_granules_max` MUST return `GetError::BeyondFileBound` without touching translation memory; explicit prefetch MUST check the bound before its protection and classification lookups and count such a page as `rejected` without touching translation memory, an automatic readahead window MUST end below the bound, and every translation lookup and writer MUST assert the granule is below it.
- FR-5: Retirement MUST leave every entry of the file's slot absent and release the whole OS pages inside that slot's range before the slot can be registered again, leaving every other slot's entries intact; it MUST NOT close the file's handle, release the slot or report `RetireStatus::Retired` before the global epoch has advanced twice past the store that cleared the slot's mirror; registration finding no free slot meanwhile returns `FileRegistrationError::AtCapacity`. The epoch is recorded once, when the file goes from live to retiring; later `retire_file` calls leave the record unchanged.
- FR-6: `Pool::session` and `Pool::session_file` MUST pin the reader's epoch as one guard and unpin it when the `ReadSession` or `FileSession` drops.
- FR-7: `ReadSession::file` and `Pool::session_file` MUST return `GetError::StaleFile` unless the mirror, loaded after the epoch pin, names the requested generation. `SessionFile::page` and `FileSession::page` MUST load the entry once and return the resident granule as a slice borrowed for the session, `Absent` when no entry is set, or `BeyondFileBound`; for a file retired after the view was opened they keep returning that file's own resident granules or `Absent` until the session drops, and never another file's or granule's bytes (AT-D21).
- FR-8: A session read MUST set the CLOCK reference of the frame it returns and MUST NOT submit I/O.
- FR-9: `Pool::read_copy` MUST make one optimistic attempt and at most one ordinary `get` fallback, returning `Copied`, `Pending` or `Busy`, with the output valid only on `Copied`.
- FR-10: The optimistic attempt MUST run in this order and accept only if every step holds: load the entry to find the frame; load the stamp and require Resident; load the entry again and require the same frame; copy; `Acquire` fence; load the stamp again and require equality; load the mirror and require the requested generation.
- FR-11: Every in-process writer of read-frame payload MUST use same-width relaxed atomic word stores (WRC-D1) and MUST take a `Release` fence after claiming the frame and before its first payload store.
- FR-12: In an optimized caller of another crate that matches the result of `get` at the call site (witness: the loop function of the fair `get` lane, profile `profiling`, x86-64 on `nix`), a hit on a frame whose CLOCK reference is already set MUST execute no call into the crate and MUST carry its hit value in registers, the first reference to a frame after its CLOCK reference was cleared takes one cold out-of-line call as in the session read, and miss, busy and error outcomes leave through one out-of-line path; the hit value of the fast path is a private value of at most two machine words and the public signature of `get` is unchanged; every argument and invariant the hit path asserts at `604410a` MUST still be asserted, once.

### Technical Requirements

- TR-1: A hit on `get`, a session read and a successful optimistic copy MUST perform no allocation, lock, syscall, spin or probe loop.
- TR-2: Entries MUST be written only under the control lock with `Release` stores and read with `Acquire` loads through `crate::sync`.
- TR-3: A session, `ReadSession` or `FileSession`, MUST count as exactly one guard against `peak_guards_per_reader`, and a nested `get` inside it MUST count as the next.
- TR-4: `ReadSession`, `SessionFile` and `FileSession` MUST be neither `Send` nor `Sync` and MUST NOT outlive the pool or the reader; a slice from a session MUST NOT outlive that session (it is an ordinary `&[u8]` and may be read by scoped threads while the session lives); escapes MUST fail to compile.
- TR-5: Releasing translation backing MUST be advisory for correctness: every entry is already absent when the release runs.
- TR-6: Volatile payload loads MUST live only in `src/pool/read_copy.rs` behind the single `Frames::optimistic_payload_ptr` seam.
- TR-7: Gates AT-G1 to AT-G8 MUST pass on `nix` under the bench plan's protocol before adoption.
- TR-8: The public API MUST drop `PageTable` and add the types `ReadSession`, `SessionFile`, `FileSession`, `SessionRead` and `ReadCopy`, the methods `Pool::session`, `Pool::session_file`, `ReadSession::file`, `SessionFile::page`, `FileSession::page`, `Pool::read_copy` and `PoolBuilder::file_granules_max`, and the variants `GetError::BeyondFileBound`, `PoolConfigError::FileGranulesMaxNotPowerOfTwo`, `PoolConfigError::FrameCountAboveTranslationLimit` and `PoolConfigError::TranslationUnrepresentable`, pinned by `tests/public_api.rs`.
- TR-9: Every lane that decides AT-G1, AT-G2, AT-G3, AT-G5, AT-G6 or AT-G8 MUST satisfy rules F1 to F11 of the bench plan's fairness contract, AT-G1, AT-G5 and AT-G6 MUST pass under F12, and the legacy resident lanes MUST keep the loop code of `604410a`; AT-G4 stays on its legacy lane, where both arms run one loop, and is recaptured only with one runner built against both trees.

## Acceptance Criteria

- [ ] AC-1 (FR-1): Given a pool built on `main`'s test suite, when the hash table is removed, then every existing pool, miss, prefetch, retirement and retention test passes unchanged in behavior, except that `RetireStatus::Retired`, the close of the file's handle and the release of its slot wait for the grace period of FR-5 (AT-D19).
- [ ] AC-2 (FR-2): Given `file_granules_max` of zero or a non-power-of-two, a `frame_count` above `2^30`, or an unrepresentable entry count, when the pool builds, then it returns the variant FR-2 names for that case.
- [ ] AC-3 (FR-3): Given a reservation that the injected allocator refuses, when the pool builds, then it returns `PoolBuildError` and leaks nothing; and given builder options whose reservation exceeds the host's RAM plus swap under Linux heuristic overcommit, when the pool builds, then it succeeds.
- [ ] AC-4 (FR-4): Given a page at `file_granules_max`, when `get`, a session read or `read_copy` runs, then each returns `GetError::BeyondFileBound`; when it is explicitly prefetched for a middle slot and for the last slot, then the report counts it `rejected`, no frame of another file is protected and nothing is read past the reservation; and when a sequential scan approaches the bound, then no readahead window names a granule at or above it and no other slot's entry changes.
- [ ] AC-5 (FR-5): Given a retired file whose slot is registered again, when the new file is read, then no page of the old file is observable and the slot's entries all read absent first; and given two files in neighboring slots that share an OS page, when one retires, then the other's resident pages still hit.
- [ ] AC-6 (FR-6, TR-3): Given `peak_guards_per_reader` of two, when a session holds one nested `get` guard, then a third acquisition asserts.
- [ ] AC-7 (FR-7): Given a resident page, an absent page and a file retired before its view is opened, when each is read in a session, then the results are the page bytes, `Absent` and `StaleFile` from `ReadSession::file` or `Pool::session_file`; given a file retired after its view was opened, reads through that view return the file's own bytes or `Absent` until the session drops (AT-D21).
- [ ] AC-8 (FR-8): Given a session read of a frame whose reference bit is clear, when eviction pressure follows, then the frame is not the first victim and the driver recorded no read.
- [ ] AC-9 (INV-2, INV-3, INV-7): Given Loom interleavings of a session read and of `get` against publish, eviction, reclamation, retirement of an empty file and reuse of its slot, where retirement and reuse run through the production retirement record and its two-advance grace period, when every schedule runs, then no read returns bytes of another page, another file or a reclaimed frame; for `get`, which keeps its per-call mirror check, the deterministic window after its epoch pin and before its entry load shows a `get` that resumes after retirement return `StaleFile`, and the slot is not reused while that `get` is paused; after it resumes, two advancing polls make the slot reusable.
- [ ] AC-10 (TR-4): Given code that returns a session slice past the session, drops the reader before the session, or sends or shares a `ReadSession` across threads, when it compiles, then each is rejected.
- [ ] AC-11 (FR-9): Given a resident page, a miss and an exhausted pool, when `read_copy` runs, then it returns `Copied` with exact bytes, `Pending` and `Busy`.
- [ ] AC-12 (FR-10): Given Loom interleavings of the optimistic attempt against eviction, frame reuse for another page, reuse for the same page and retirement, when every schedule runs, then an accepted copy holds only bytes of the requested page's publication.
- [ ] AC-13 (FR-11): Given the writer inventory, when Miri runs the in-process fills, then every read-frame payload write is a same-width atomic store.
- [ ] AC-14 (TR-6): Given a volatile payload read or a seam call outside `src/pool/read_copy.rs` in a pool file and in `src/mock.rs`, when `tests/arch3_sync_alias.rs` runs, then both are rejected.
- [ ] AC-15 (TR-1): Given warmed pools on both backends, when hits, session reads and successful copies run under the zero-alloc harness, then no allocation is recorded.
- [ ] AC-16 (TR-7): Given the frozen manifests, when the campaign runs on `nix`, then AT-G1 to AT-G8 pass and the evidence is retained.
- [ ] AC-17 (TR-8): Given the public API snapshot, when it is regenerated, then it shows exactly the additions and the removal of TR-8.
- [ ] AC-18 (TR-9): Given a capture of a fair lane, when `warm-prepare` validates it, then unequal runner hashes across product versions, a missing warm pass, a timed region under 100 ms, a fault in the timed region of a resident lane, a missing page-size witness or a missing floor comparison each reject the set, and `report.json` shows ns per read per arm and their difference with its interval.
- [ ] AC-19 (TR-9): Given the legacy resident lanes, when their loop functions are compared with `604410a`, then they are unchanged, and the session, copy, fair `get` and floor arms run in the fair-lane module.
- [ ] AC-20 (FR-12, TR-7): Given one runner source built against the tree AT5 leaves and the tree after the hit-path change, when AT-G8 runs on `nix`, then its upper bound is at or below 0.85, the AT-G7 lanes still pass, the hit path makes each of its assertions once, the retained disassembly of the fair `get` loop shows no call into `dios` on the hit path of a frame whose CLOCK reference is set, one cold target for the first reference and one out-of-line target for miss, busy and error, and the public API snapshot is unchanged.
- [ ] AC-21 (TR-9): Given the three layout builds of AT-G1, AT-G5 and AT-G6, when each is captured, then every build passes its bound.
- [ ] AC-22 (FR-5, INV-3): Given `registered_file_capacity` of one and a session that opened its view of the file, when the file is retired and the pool is polled repeatedly, then `retire_file` reports `Retiring`, the handle stays open and registering another file returns `AtCapacity`; when the session drops and two polls advance the epoch, then `retire_file` reports `Retired` and registration succeeds, also when `retire_file` is called between the polls.
- [ ] AC-23 (FR-7, AD-9): Given Loom interleavings of a session that validated a file and holds its view against retirement of that file, two advancing polls, registration of a successor in the slot and publication of the successor's page, when every schedule runs, then the session never returns the successor's bytes; the case fails against a retirement that releases the slot after fewer than two advances.
- [ ] AC-24 (AD-18, TR-3, TR-4): Given `Pool::session_file`, when it opens a view, then it asserts and checks exactly what `Pool::session` followed by `ReadSession::file` does, counts as one guard, releases the pin when the view drops or the open fails, and its view is neither `Send` nor `Sync`.

## API Contract

| Item | Signature | Purpose |
|------|-----------|---------|
| `PoolBuilder::file_granules_max` | `fn(self, file_granules_max: u32) -> Self` | Per-file granule bound fixed at build |
| `Pool::session` | `fn session<'pool>(&'pool self, reader: &'pool ReaderCtx) -> ReadSession<'pool>` | Pin the reader's epoch once for many reads |
| `ReadSession::file` | `fn(&self, file: FileId) -> Result<SessionFile<'_>, GetError>` | Validate pool identity and liveness once per file |
| `Pool::session_file` | `fn session_file<'pool>(&'pool self, reader: &'pool ReaderCtx, file: FileId) -> Result<FileSession<'pool>, GetError>` | Pin the epoch and validate one file in one call |
| `FileSession::page` | `fn(&self, granule_idx: u32) -> Result<SessionRead<'_>, GetError>` | As `SessionFile::page` |
| `SessionFile::page` | `fn(&self, granule_idx: u32) -> Result<SessionRead<'_>, GetError>` | `SessionRead::Hit(&[u8])` or `SessionRead::Absent` |
| `Pool::read_copy` | `fn(&self, reader: &ReaderCtx, page: PageId, offset_bytes: u32, output: &mut [u8]) -> Result<ReadCopy<'_>, GetError>` | `ReadCopy::Copied`, `Pending(PendingToken)` or `Busy` |
| `GetError::BeyondFileBound` | `{ page: PageId }` | Granule index at or above the build bound |
| `PoolConfigError::FileGranulesMaxNotPowerOfTwo` | `{ requested: u32 }` | Zero or non-power-of-two bound |
| `PoolConfigError::FrameCountAboveTranslationLimit` | `{ requested: u32, limit: u32 }` | Frame count above `2^30` |
| `PoolConfigError::TranslationUnrepresentable` | `{ registered_file_capacity: u32, file_granules_max: u32 }` | Entry count overflows the mapping size |
| `Pool::get` | signature unchanged | Hit result sized for register return, fast path inlinable (FR-12) |
| `PageTable` | removed | Hash table no longer part of the product |

## Verification

- `mise run test`: unit and integration suites, including translation, session, copy-out and public API cases.
- `mise run loom`: publish, evict, reclaim, retire and slot-reuse interleavings against session reads and optimistic copies.
- `mise run miri`: translation storage, atomic payload writers and the guarded fallback.
- `mise run asan`: syscall-touching suites on the translation and copy paths.
- `mise run lint`: clippy pedantic and rustfmt clean.
- `mise run remote -- mise run bench-mmap-workloads <output>`: resident session, copy and regression lanes on `nix`.
- `mise run gate <paired.csv> <bound>`: asserts each of AT-G1 to AT-G8 against its bound.
