# Bench Plan: Sira native residency hint

## Question

Can Sira retain Dios's file-liveness, eviction, reuse, and read-stability
contracts while replacing the general warm acquisition with a coarse file
lease plus a volatile dense residency hint addressed by REMIX?

The stable REMIX locator remains `(FileId, page_ordinal, byte_offset)`. It never
contains a pool frame index, pointer, epoch, or guard. A separate pool-minted
hint may cache `(frame, residency_generation)` and is valid only after an epoch
pin revalidates both fields. A stale hint falls back to the ordinary `get` path.

## Workload and host

- Pinned host: nix, Threadripper 3970X, CPU 0, performance governor.
- Corpus: preserved Sira 5,000,000-row, eight-shard, one-L1 store.
- Logical work: R7's exact 8,192 deterministic n=1 starts and checksum
  `17131094364009660908`.
- Each gate uses at least 30 fresh-process, order-alternated pairs after the
  arm's untimed warmup. The shared paired-log compare consumes the raw
  two-column CSV.

## Arms

1. `locator`: aligned mmap, stable native locator, borrowed value. This is the
   optimized mmap baseline.
2. `dios-direct`: the identical locator and full resident working set through
   the unmodified general `Pool::get`; decode and hash directly from the live
   `FrameGuard`. This removes the conservative upper arm's duplicate mmap
   dereference without changing the acquisition protocol.
3. `dios-hint`: acquire one typed file lease per source run, address a dense
   volatile hint from the stable locator, publish the existing reader epoch,
   validate frame state plus residency generation, touch CLOCK, and return the
   same `FrameGuard`. Stale hints use general `get` and refresh outside the
   measured all-resident case.

Corruption verification remains once per frame residency generation. The
measured loop allocates nothing and hashes the borrowed key and value while the
guard is live.

## Gates

- Direct-frame displacement: `dios-direct / locator` one-sided CI95 upper
  `<= 1.02`. A pass proves general Dios acquisition is already displaced by
  direct guarded-frame consumption; do not add the hint protocol.
- If direct-frame displacement fails, native hint parity:
  `dios-hint / locator` one-sided CI95 upper `<= 1.02`.
- The hint must also improve over `dios-direct`; a ratio CI95 upper above
  `0.95` rejects the added protocol as immaterial even if noise happens to put
  it below the mmap bar.

Compare commands:

```text
cargo bench --features bench --bench compare -- <paired.csv> 1.02
cargo bench --features bench --bench compare -- <hint-vs-direct.csv> 0.95
```

Only residual trace clusters worth at least 10 ns/read are implementation
targets. The packed one-round page hash is excluded: its retained
probe-distribution gate failed on Dios-realistic interleaved small tables.

## Safety gates

Numeric success is experimental evidence, not adoption authority. Adoption is
blocked until Loom covers both adversaries:

1. file lease acquisition versus retire, full frame retirement, file-slot
   reuse, and a subsequent hinted pin;
2. hinted pin publication versus mapping removal, frame eviction, two epoch
   advances, reuse, and residency-generation change.

Each schedule must yield either a live guard linearized before invalidation or
a stale-hint/stale-file result; it may never expose reused bytes. Sequential
tests must also pin stale-hint fallback, exact checksum, zero allocation after
warmup, and unchanged normal `get` behavior.

## Escalation

If direct-frame displacement fails, profile only its residual acquisition
delta before implementing the hint arm. If the hint arm fails, do not weaken
EBR, store a stable frame identity in REMIX, or adopt the disqualified hash.
Keep the locator layout improvement and amortize the remaining point tax via
range/span composition.
