# Handoff to sira: the array-translation API

dios branch `feat/array-translation` (not yet merged or pushed). sira pins dios at
`d7548f8` (`sira/Cargo.toml`); the changes below arrive with the rev bump that follows the
merge. Scope: `scopes/active/array-translation/scope.md` (API Contract, TR-8); evidence:
`benches/evidence/array_translation/README.md` (AT8 binding campaign).

## Breaking for sira

- `GetError::BeyondFileBound { page: PageId }` is a new variant: any read verb given a
  granule index at or above the pool's `file_granules_max` returns it without touching
  translation memory. `sira/src/segment/pooled.rs:474` matches `GetError` exhaustively and
  stops compiling until it handles the variant.
- `PoolConfigError` gains `FileGranulesMaxNotPowerOfTwo`, `FrameCountAboveTranslationLimit`
  and `TranslationUnrepresentable`; exhaustive matches on it need the same treatment.
- `PageTable` is removed from the public API (sira does not use it).

## New

- `PoolBuilder::file_granules_max(u32)`: per-file granule bound fixed at build, default
  `2^22`, a power of two; size it to sira's largest segment in 4 KiB granules.
- Session read: `Pool::session(reader) -> ReadSession`, `ReadSession::file(FileId) ->
  Result<SessionFile, GetError>`, `SessionFile::page(granule) -> Result<SessionRead, GetError>`
  (`Hit(&[u8])` or `Absent`); `Pool::session_file(reader, file) -> FileSession` fuses the
  two opens. One epoch pin and one liveness check per view; a session counts as one guard;
  a session read sets the CLOCK reference and never submits I/O. A file retired after its view
  was opened keeps returning its own resident granules or `Absent` until the session drops
  (AT-D21).
- `Pool::read_copy(reader, page, offset_bytes, &mut [u8]) -> Result<ReadCopy, GetError>`
  (`Copied`, `Pending(PendingToken)`, `Busy`): a pin-free optimistic copy with one `get`
  fallback. Correct under Loom, Miri and ASAN, but not adopted on performance (AT-D29): about
  12 ns per 64-byte copy against 5 ns for an mmap copy.

## Unchanged signature, faster

- `Pool::get`: a hit on a frame whose CLOCK reference is set inlines into the caller with no
  call into dios (FR-12); AT-G8 at 0.49 of the AT5 tree's `get`.

## Measured at the campaign tree (nix, mmap = 1.0)

| Workload | dios / mmap |
|---|---:|
| Whole 4 KiB page per request, 1,024-page set (AT-G2) | 1.01 |
| Whole page, 256 MiB set, arena on 2 MiB pages (AT-G3) | 0.92 |
| 64-byte read through a session (AT-G1, pooled) | 1.43 |
| 64-byte `get`, one call per read (AT-G8 mmap pair) | 3.19 |
| 64-byte `read_copy` (AT-G5) | 2.36 to 2.52 |
| DRP-G4 eight readers / scaling (AT-G7) | 0.31 / 0.15 |
| RC-G1 cold / pressure (AT-G7) | 0.84 / 0.60 |

## For sira's read paths

- Block scans that decode whole blocks (`with_frames_ahead`) sit in the parity regime.
- Point lookups that call `get` per segment pay about 11 ns per hit against about 6 for a
  session read; a lookup that touches several frames of one segment can open
  `Pool::session_file` once and read each frame through `FileSession::page`.
- Session length matters: one read per session costs 16.3 ns, sixteen cost 6.0 ns each,
  sixty-four 5.3 ns (session-length sweep in `README.md`).
- Liveness costs about 0.07 ns per session read (`liveness-pricing.md`).
