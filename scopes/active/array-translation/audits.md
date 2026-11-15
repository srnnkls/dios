# Audits: array-translation

Records AT1 freezes before any product change. Line numbers are at `main` `ade5310`.

## Read-frame payload writer inventory

Every in-process write into read-frame payload. FR-11 applies to each in-process row:
same-width relaxed `AtomicU64` word stores (WRC-D1) behind one `Release` fence taken
after the claim and before the first payload store (AD-13). AT7 converts the rows
marked convert; the write token stays the unique `InFlightFrame`.

| Writer | Location | Reached from | Disposition |
|--------|----------|--------------|-------------|
| `Frames::fill` | `src/pool/frames.rs:364` | `Pool::insert_resident_frame_internal` (`src/pool/mod.rs:2442`), test support `fill` (`src/lib.rs:185`), unit tests (`src/pool/frames.rs:958`, `:1012`), Loom model (`src/pool/loom_model.rs:530`, `:674`) | Convert: `Release` fence, then atomic word stores |
| `Frames::transfer_mut` partial write | `src/pool/frames.rs:374` | unit test (`src/pool/frames.rs:959`) | Convert: aligned atomic word load and store helpers for an unaligned partial range |
| Mock point read fill | `src/mock.rs:1019` (`fill_read`), `src/mock.rs:1873` (`fill_seeded_prefix`) | `MockDriver` completions through `Frames::transfer_mut` | Convert |
| Mock vector read fill | `src/mock.rs:1043`, `src/mock.rs:1838` | `ReadVector::transfer_prefix` (`src/driver/read_vector.rs:172`, `:238`) | Convert |
| Loom vector fill | `src/pool/loom_model.rs:88` | Loom model | Labeled payload abstraction; converted with the model |
| `Frames::populate` | `src/pool/frames.rs:313` | build and bench setup | Exempt: `&mut self` excludes every reader, so no concurrent `read_copy` reaches it |
| Eager `pread` and `preadv` | `src/backend/eager.rs:97`, `src/backend/eager.rs:125` | eager backend | Kernel write through `transfer_mut`; outside the abstract machine; the syscall follows the claim |
| io_uring READ and READV | `src/backend/uring.rs:332`, `src/driver/read_vector.rs:312` | uring backend | Kernel write through `transfer_ptr`; outside the abstract machine |

Not read-frame payload and unchanged: the write arena (`src/pool/write_arena.rs`), mock
file backing (`src/mock.rs:1004`, `:1061`) and caller-owned output buffers.

## Ordering audit of the optimistic copy

The FR-10 sequence and what each step compiles to. The instruction column is the
expected lowering; AT7 records the generated instructions of `src/pool/read_copy.rs`
for both targets with the rustc version and replaces any row that differs.

| Step | Rust | x86-64 | aarch64 |
|------|------|--------|---------|
| First entry load | `AtomicU32::load(Acquire)` | `mov` | `ldar` |
| First stamp load | `AtomicU64::load(Acquire)` | `mov` | `ldar` |
| Second entry load | `AtomicU32::load(Acquire)` | `mov` | `ldar` |
| Payload copy | `read_volatile::<u64>` per word | `mov` | `ldr` |
| Fence | `fence(Acquire)` | none | `dmb ishld` |
| Second stamp load | `AtomicU64::load(Acquire)` | `mov` | `ldar` |
| Mirror load | `AtomicU64::load(Acquire)` | `mov` | `ldar` |
| Writer fence after claim | `fence(Release)` | none | `dmb ish` |
| Writer payload store | `AtomicU64::store(Relaxed)` | `mov` | `str` |

- Stamp: each stamp load is `Acquire`, so the second entry load and the payload loads
  cannot move above the first stamp load on either target or in the compiler.
- Entry: the second entry load sits between the two stamp loads; by INV-2 an equal
  stamp across the bracket and a set entry naming the frame inside it identify the
  requested page's publication.
- Volatile: an `Acquire` load orders only what follows it, so the volatile payload
  loads need the `Acquire` fence to stay before the second stamp load. On x86-64 TSO
  never reorders loads with loads and the fence is a compiler barrier only; on aarch64
  `dmb ishld` orders the preceding `ldr`s before the following `ldar`.
- Compiler: volatile accesses are neither elided nor reordered with each other, and
  LLVM does not move a load across an `Acquire` fence or an `Acquire` load in the
  direction that would empty the bracket. Volatile loads carry no synchronization of
  their own; the fence pair (writer `Release` after the claim, reader `Acquire` before
  the second stamp) is the only edge that makes a torn copy visible as a stamp change.
- Writer: the claim is an `AcqRel` compare-exchange on the stamp; the `Release` fence
  after it keeps the claim visible to any reader that observes a later payload store.
- Unsupported targets and word widths take the ordinary `get` fallback.

## Miri model limits

What `mise run miri` proves for the copy-out path, and what it does not.

- Covered: the atomic writer helpers, aligned and partial-word stores, alignment of the
  covering word loads, provenance and lifetime of the arena pointers, and the ordinary
  guarded fallback.
- Limit: a `read_volatile` racing an atomic store is a mixed non-atomic and atomic
  access; Miri reports it as a data race, so the concurrent volatile copy is not run
  under Miri and no `cfg(miri)` reader substitution stands in for it.
- Limit: an atomic-load stand-in for the payload checks the stamp algorithm only and is
  labeled as such; it is not evidence for the production volatile primitive.
- Limit: kernel writes through io_uring and the eager syscalls are outside the
  abstract machine; Miri runs only the in-process fills.
- Limit: Miri does not explore weak-memory reorderings exhaustively; the ordering
  audit above and Loom's labeled payload abstraction carry that part.
