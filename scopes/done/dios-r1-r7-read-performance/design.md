# Design: Dios R1-R7 read-path performance

## Problem

R7 changed the denominator. A native REMIX locator reduced aligned mmap from
`519.178` to `244.537 ns/read`, exposing acquisition cost that the older format
had hidden. General Dios get remained `1.3280 / 1.3478` against locator mmap.
The compact lease/hint prototype removed a material share of general-get work
(`0.8813 / 0.8980`) but remained mock-only and lacked the retirement/reuse
proofs required for production.

At the same time, the atomic-read candidate showed that ordinary get still has
removable work: lock-free liveness, reader-slot isolation, and a cheaper hash
moved the warm bracket from `2.00-2.09` to `1.75` on Zen 2. Its exact packed
hash later failed realistic small-table probe distribution, so that source is
not an adoptable unit.

The design therefore separates three decisions:

1. adopt the independently sound liveness mirror and slot alignment;
2. select a hash only through an admissibility gate; and
3. translate the manifest-bound R7 capability to production while making stale
   hints degrade to the existing ordinary-get protocol.

## Source isolation

The clean extraction equation is normative:

```text
1004a2e6fcae0bcc9552dc3211c2416e388a250d
  + sha256 16748dd8... of resources/r7-source.diff
  = the four R7 source hashes in resources/evidence.md
```

The tracked carrier was captured during a later R8 profile, but its bytes are a
base-to-R7-only diff. The current
`.worktrees/experiment-sira-point-proof` is not the extraction input: its
`lib.rs` and `pool/mod.rs` contain 26 and 187 lines of later resident-set work
relative to the reconstructed R7 files. Extraction verifies hashes before any
translation, then treats the prototype as behavioral evidence rather than
production-ready code.

## Selected architecture

### Ordinary get

`Pool` owns `file_live_generations: Box<[AtomicU64]>`, indexed by the bounded
driver file slot. Encoding is `(generation << 1) | LIVE`; zero is non-live.
The complete generation remains present. The ordering is:

1. register under AD-4 installs the exact `PoolFile`, then Release-stores live;
2. warm get asserts pool identity, Acquire-loads the slot, and compares exact;
3. retire under AD-4 marks `Retiring`, then Release-stores zero before unmap;
4. a miss under AD-4 repeats the authoritative `PoolFile` check.

The mirror permits a stale-positive observation only when get can linearize
before retirement. The reader publishes its epoch before exact page-table
validation; that mapping is the ordinary-path authority. Retirement can remove
the mapping, but EBR cannot reclaim or reuse a frame protected by the epoch
publication. A reader that loses the race sees no mapping, takes AD-4, and
returns `StaleFile` at the recheck.

`ReaderSlot` gains `#[repr(align(64))]`. Its existing Release publication,
`SeqCst` fence, Acquire collector scan, nesting count, abort, and last-drop
quiescence remain byte-for-byte in semantics. Alignment is a placement change,
not permission to weaken the protocol.

### Hash decision

The current hash mixes driver, file slot, generation, and granule in four
rounds. The experiment's packed candidate aliases generation and granule into
one 32-bit expression; full-key equality kept it correct, but its interleaved
16-file small-table p99 was `11` versus `5`. It is rejected.

The sole candidate retains every field at full width before one finalizer:

```text
file_page = (u64(generation) << 32) | u64(granule)
seed = driver ^ file_page ^ u64(slot).wrapping_mul(PHI)
hash = mix(seed)
```

DRP-G1 changes the small-table p99 criterion from an impossible percentage on
integer probes to an absolute `+1`, while keeping the `+5%` mean limit and all
larger-table relative limits. This is fixed before the rerun. Passing quality
without a real-backend `<=0.98` speed result retains the current hash; speed
without quality also retains it. There is no third candidate in this scope.

The original matrix is calibration, not a binding holdout. Before any hash
implementation edit, the plan freezes this independent 50%-load holdout:

- driver identities `0x71c3_5a09_d4e2_b687` and
  `0xd903_4f61_28bc_7a55`;
- table sizes 2,048, 65,536, and 262,144 slots;
- file populations 3, 31, and 127 where population does not exceed key count;
- for key ordinal `i` in `0..slots/2`, file ordinal `f = i % files`, file slot
  `3 + 5*f`, generation `0x8000_0001 + 17*f`, and granule
  `11 + 257*(i/files)`;
- one round-robin insertion order (`i` ascending) and two shuffled orders. A
  shuffle starts with `[0, 1, ..., count-1]`, state equal to seed
  `0x243f_6a88_85a3_08d3` or `0x1319_8a2e_0370_7344`, then runs Fisher-Yates
  from `upper=count-1` down to `1`; before each swap it applies
  `state ^= state << 13; state ^= state >> 7; state ^= state << 17` with u64
  wrapping and swaps `upper` with `state % (upper+1)`;
- full-key equality and byte-identical insertion order for base and candidate;
  successful-probe p99 is nearest-rank `ceil(0.99 * count) - 1` in the sorted
  zero-based sample array.

Every driver/size/population/pattern/seed combination uses the scope's same
mean/p99/max bounds. None of these identities, populations, sizes, generation
formulae, or permutations occurs in the calibration script. Results are
written only after the plan and generator hash are frozen.

### File lease

Each fixed file slot has a preallocated proof-bearing `AtomicU32` lease count.
Acquisition and retirement both hold AD-4, so their admission linearization is
ordinary mutual exclusion rather than an additional lock-free protocol:

- acquisition seeing `Live` increments unless at `u32::MAX`;
- acquisition seeing absent, another generation, or `Retiring/Retired` returns
  `ResidentLeaseError::StaleFile`;
- exhaustion returns `ResidentLeaseError::Exhausted` without changing state;
- retirement that wins first clears ordinary admission and refuses the lease;
- a lease that wins first prevents frame retirement, close, and slot reuse;
- lease drop performs one AcqRel decrement and signals the existing pool wake
  path when it releases the last count; the next bounded poll pass completes
  retirement progress.

Only acquisition increments while holding AD-4; concurrent drops can only
reduce the word. Therefore a locked load below `u32::MAX` followed by
`fetch_add(1)` cannot wrap, while a load at the ceiling returns `Exhausted`.
Each preallocated file slot owns an `Arc<ResidentLeaseState>` containing its
proof-bearing count, exact generation state, pool identity, and existing wake
context. `ResidentFileLease` owns one `Arc` clone plus the exact `FileId`, is
not `Clone`, and has no Rust borrow from `Pool`. `Arc::clone` at admission does
not allocate because the control block already exists. This makes pool-first
drop order safe without a self-referential lifetime: the lease's last drop can
decrement and signal only state it owns. It cannot access the destroyed frame
arena, driver, control mutex, or file table. The count is a control-plane
lifetime obligation, not frame retention: CLOCK may evict a page while its
file lease exists.

The lease is not a second owner of the pool or backend file handle. All read
operations still require `&Pool` and validate its identity. Explicit
`retire_file` waits for the count; destroying the entire `Pool` revokes every
operational use, while a later lease drop remains memory-safe and touches only
its shared lease state.

### Packed residency word and hint

Replace each frame's `AtomicU8` state with one proof-bearing `AtomicU64` word:

```text
bits 0..=1  FrameState tag
bits 2..=63 residency generation
```

The exact bit count is asserted against the state enum. AD-4/single-writer
discipline still owns legal transitions. `InFlight -> Resident` checked-adds
the generation and Release-stores the resident word after bytes are filled.
`Resident -> Evicting` Release-stores the non-resident word before the frame is
eligible for grace-period reclamation. Other transitions preserve generation.
Hinted readers Acquire-load the word before validating the exact frame page.
The packed word remains the authority for hints and frame transitions; ordinary
readers rely on the exact page-table mapping under EBR and do not load it. Thus
unused hints add no ordinary hot-path state load, second state array, or second
transition atomic.

`ResidentHint` is the 16-byte tuple `(granule: u32, frame: u32,
stamp: NonZeroU64)` with private fields. `Option<ResidentHint>` occupies the
same 16 bytes through the nonzero niche. A stable `PageId` and file lease are
always supplied separately; neither hint nor locator becomes an authority by
itself.

`Frames` also preallocates one exact-page cell per frame. The cell stores the
full `PageId`, not only a granule. Its single writer fills bytes, writes the
cell while the frame is unpublished, then Release-publishes the new Resident
stamp. The cell is read only after reader epoch publication and Acquire
validation of that exact stamp. Refill cannot overwrite the cell until the old
mapping is removed and two epoch advances make reuse legal. Because this is
non-atomic metadata read concurrently with later reuse, it lives in one small
audited `UnsafeCell<MaybeUninit<PageId>>` module whose safety invariant is the
unchanged EBR reuse barrier.

Every path that can publish `InFlight -> Resident`, including mock/test fill,
writes this identity first. The fixed storage charge is exactly
`frame_count * size_of::<PageId>()` plus container metadata and is recorded in
DRP-G2; it never grows after pool construction.

### Hinted get and fallback

```text
assert reader, pool, lease, and PageId ownership
if hint is None or hint.granule != page.granule:
    ordinary get
if hint.frame is outside the target pool frame arena:
    ordinary get
publish reader epoch (existing begin_pin + SeqCst fence)
Acquire-load packed frame word
if word != hint.stamp or state != Resident:
    abort only if this was the first uncommitted pin
    ordinary get
read frame exact-PageId metadata under the published epoch
if frame_page != supplied_page:
    abort only if this was the first uncommitted pin
    ordinary get
touch CLOCK
commit pin
return ordinary FrameGuard over the exact frame
```

The word comparison binds state and residency generation; the following exact
metadata comparison binds file generation and granule. If the reader observes
the old word before concurrent eviction, epoch publication makes the guard
linearize before eviction and prevents metadata/byte reuse until drop. If it
observes any later word or a mismatched page, it falls back. A hint copied from
another pool has no authority: an out-of-range index falls back before
indexing, and only the target pool's independently validated frame metadata can
match the supplied target `PageId`. A coincidental exact target match is safe
and may hit because it returns the requested target bytes, not bytes from the
source pool.

Hint minting is advisory: lookup a full `PageId`, then read that frame's packed
resident word. Either observation may become stale immediately. Safety lives
entirely in the later publish-and-revalidate sequence.

## Invariants

| ID | Invariant | Enforcement |
|---|---|---|
| DRP-INV1 | A live mirror word denotes exactly one pool/file-slot generation; zero never admits | full-width encoding, identity assert, Release/Acquire, locked miss recheck |
| DRP-INV2 | Get returns stale or a guard linearized before retirement; no retired/reused bytes | publish-before-validate EBR plus get-vs-retire Loom |
| DRP-INV3 | Lease acquisition and retirement have one order; a live lease blocks close and file-slot reuse | AD-4 serialized count/state transitions and lifecycle tests |
| DRP-INV4 | A hint can authorize only the supplied lease's exact full `PageId` in this pool | capability asserts plus target-frame exact-PageId metadata validated under epoch after stamp Acquire |
| DRP-INV5 | An old hint cannot validate after eviction, two advances, and frame reuse | packed state+generation word and hinted-reuse Loom |
| DRP-INV6 | Every returned hinted guard uses the unchanged ReaderSlot fence and normal FrameGuard drop | shared production primitives; no alternate guard type |
| DRP-INV7 | Hint absence/staleness is behaviorally ordinary get and leaves no phantom epoch pin | `get_with_hint` owns abort/fallback; sequential and Loom checks |
| DRP-INV8 | Unused hint support adds no operation to ordinary warm get and no second frame-transition atomic | separate API path, exact mapping plus EBR as ordinary authority, packed state as hint/transition authority, DRP-G2 |
| DRP-INV9 | R8 retention cannot enter this feature | extraction hash allowlist and forbidden-symbol scan |

## Alternatives

### Copy the point-proof worktree

Rejected. It is now a mixed R7/R8 tree; direct copying silently imports
resident-set retention. The base-plus-carrier reconstruction is byte-auditable.

### Packed one-round hash

Rejected. It passed large tables but caused structural clustering in realistic
small interleaved tables. Full-key comparisons preserve correctness, not work
bounds or performance.

### Ship the full-width hash on simulation alone

Rejected. Its remaining small-table failures may be quantization, but a hash
change also needs material speed on real `Pool<Driver>`. DRP-G1 requires both.

### Dense always-present hint

Rejected. The five-pair R7 smoke and 6,000-repetition diagnostic both regressed;
the long-run residual worsened by `3.882 ns/read`. The nonzero niche already
makes absence free in representation size.

### Remove or weaken the epoch fence

Rejected as unsound. The fence closes the store-buffer execution where the
collector sees `QUIESCENT`, advances twice, and reuses bytes beneath a reader.
Prior measurements also cap the possible saving at only a few nanoseconds.

### Stable frame identity in REMIX

Rejected. A `FrameIdx`, pointer, epoch, stamp, or hint is volatile pool state.
The stable locator remains file-relative; only a live pool capability can turn
an observation into a guard.

### R8 retention

Rejected from this scope. It changes physical reuse, capacity, reclamation, and
the read-session contract. It has its own `pinned-frame-retention` scope and
cannot be smuggled in as a hint optimization.

### MockDriver as product proof

Rejected. Mock remains useful for deterministic errors and sequential
contracts, but it does not exercise io_uring registration, real file handles,
kernel-backed frame fill, or production contention. All binding performance
gates use `Pool<Driver>`.

## Complexity and cost

| Dimension | Before | After | Bound |
|---|---|---|---|
| file liveness | AD-4 lookup on every get | one `AtomicU64` per fixed file slot + one Acquire hit load | `MAX_FILES` at pool build |
| reader slots | adjacent struct layout | 64-byte alignment | `max_concurrent_readers` at build |
| frame state | one `AtomicU8` | one packed `AtomicU64` | `frame_count` at build |
| file leases | none | one preallocated Arc lease state per fixed file slot; one nonallocating Arc clone per live lease | `u32::MAX`, exhaustion typed |
| frame page identity | page table only | one preallocated exact-PageId cell per frame, read only under validated epoch | `frame_count` at build |
| hinted hit | ordinary table+liveness discovery | one epoch publish, one stamp load/compare, CLOCK, normal guard | constant time |
| stale hint | n/a | one failed validation then ordinary get | constant prefix + existing bounded path |

No queue, retry loop, or dynamically growing collection is introduced. The
only hash alternative is decided before merge, so production keeps one hash.

## Verification model

The existing `src/pool/loom_model.rs` deliberately delegates to real
proof-bearing primitives. Extend it rather than constructing a shadow state
machine. The bounded model gains two file generations and a frame reuse cycle:

| Case | Required schedule | Oracle |
|---|---|---|
| mirror get vs retire | live load; retire clears, unmaps, advances/reuses; reader validates | stale/fallback or old bytes under guard, never reused bytes |
| lease vs retire/reopen | acquire and retire race; old lease may drop; slot reopens | exactly one wins; old capability never authorizes new generation |
| hint vs eviction/reuse | hint minted; pin publish races unmap; two advances; frame refilled with another exact PageId | fallback or old bytes protected until guard drop |
| wrong file/pool hint | same granule/frame/stamp-shaped observation crosses file or pool | mismatch falls back; an exact match returns only requested target bytes |
| nested stale fallback | existing outer guard; hinted validation fails; ordinary nested get | guard count changes exactly once; outer guard keeps epoch published |

Sequential tests cover wrong pool/file/granule, `None`, stale stamp, refresh,
lease exhaustion without mutation, retirement progress, full state-generation
wrap assertions, hint niche size, non-Clone and pool-first-drop traits, exact
byte checks, and zero allocation. Miri covers the exact-page unsafe cell and
pure-memory lifetime/state paths; Linux asan covers real backend acquisition
and teardown.

## Cross-repository boundary

The companion `sira-aligned-buffers` scope owns the native locator,
hint storage/refresh policy, REMIX integration, and current-mmap versus
aligned-Dios end-to-end gates. This scope supplies a pinned Dios commit and API
contract. Its final closeout reads the Sira scope's result and records the
dependency state only; it never writes under `/Users/srnnkls/projects/sira`.
