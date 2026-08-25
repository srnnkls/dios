# R8 resident-set evidence and disposition

R8 is the performance antecedent for this scope, not an implementation to
promote. It established that a bounded read session can acquire physical pages
once and then consume precomposed integer descriptors without a per-read epoch
pin. It did not establish a production-safe retention mechanism.

## Provenance

- Frozen plan: Sira
  `benches/plans/remix-dios-resident-set-r8.md`, 5,108 bytes, SHA-256
  `2355d7723e6e8140d5c94c0afcaa1303c49a7bb7fa2d4635e2401d709ab6bd89`.
- Sira source base: `36211b452cd31980a8123f02ca0fa993e7ae2759` plus the
  experiment diff.
- Dios source base: `1004a2e6fcae0bcc9552dc3211c2416e388a250d` plus the
  experiment diff in `.worktrees/experiment-sira-point-proof`.
- Formal local evidence: `.artifacts/r8/resident-set/formal/`.
- Formal artifact manifest: SHA-256
  `0702a9ddd369344bc68b1ed07edc50cf2d42c42b7004fa6ebaa03b9371458e09`.
- Formal summary: SHA-256
  `ac5e3951b5c3438e891d7ec2da0e739dba128db395cda3a325b733ea8a1f87f1`.
- Formal counts: SHA-256
  `18ff1aea9ae8dd1a386054fd7efdf9bcd668e957c42e4b256a8e0ece2b0bc3d2`.
- Bench host: AMD Ryzen Threadripper 3970X, Linux 6.6.64, CPU 0 pinned,
  performance governor. The formal run used 30 fresh-child pairs per
  comparison, 8,192 n=1 queries, and checksum
  `17131094364009660908`.

## Result

| comparison | base mean | candidate mean | delta | paired-log geomean / CI95 upper | gate |
| --- | ---: | ---: | ---: | ---: | --- |
| resident-set / dios-hint-range | 254.702 ns/value | 231.737 ns/value | -22.965 ns/value | 0.9098 / 0.9121 | PASS <= 0.95 |
| resident-set / locator-range | 226.412 ns/value | 231.456 ns/value | +5.044 ns/value | 1.0223 / 1.0242 | FAIL <= 1.02 |

The frozen R8 verdict is therefore **FAIL**. The threshold is not relaxed by
this scope. The first comparison nevertheless demonstrates the performance
shape worth preserving: the candidate executed zero per-read pins, while the
guarded baseline executed 8,192.

The measured 22.965 ns saving combines removal of the per-read
guard/residency-validation lifecycle with Sira-side removal of a second
high-cardinality descriptor probe and a duplicate lease lookup. The formal
pairs change both simultaneously and do not isolate their shares. A separate
profile decomposition suggested comparable contributions, but that is
diagnostic attribution rather than a formal split. The approximately 5 ns
residual versus locator mmap is the observed cost after both changes, not a
target for further local work.

## Why the R8 mechanism is rejected

R8 was a `mock`-feature upper-bound prototype. Its Dios `Control` stored a
plain `Box<[u32]>` retention count array. The only ceiling was the per-frame
`u32::MAX`; there was no pool-wide admission budget. The formal configuration
retained 8,035 of 8,059 frames (32,911,360 of 33,009,664 bytes), leaving 24
frames outside the set.

## Shipping-backend capacity collision

That exact configuration cannot currently be reproduced with Dios's shipping
io_uring backend on `nix`. The backend registers its frame arena and uses
`READ_FIXED`. A causal probe recorded in
`scopes/draft/plan-prefetch/resources/landscape-audit.md` varied
`RLIMIT_MEMLOCK` at 1, 4, and 8 MiB: registration success moved with the limit,
1,024 frames succeeded at the default 8 MiB ceiling, and 1,984 frames
(7.75 MiB — 256 KiB below the limit) failed with `ENOMEM`; the probe record
attributed this to ring locked-memory overhead (attribution superseded — see
Interpretation boundary below). The host's soft and hard limits are both
8 MiB (systemd `DefaultLimitMEMLOCK`; the kernel's own default is 64 KiB),
so an unprivileged process cannot raise the ceiling itself.

R8's 8,035 retained pages alone occupy 32,911,360 bytes (31.4 MiB retained
set), and its complete 8,059-frame pool occupies 33,009,664 bytes (31.5 MiB
full pool arena). Consequently, a shipping-backend validation of the exact R8
scale required one explicit owner decision (now recorded — see below):

- change the host-level hard limit (root/PAM) and preserve the workload;
- authorize an unregistered-buffer backend, reopening the settled
  registered-`READ_FIXED` decision and gating that backend independently; or
- reduce the retained set to fit the present limit, which creates a different
  capacity experiment and cannot be cited as an exact R8 rerun.

The decision is recorded (scope.md revision 11): the shipping baseline must
operate under kernel-default `RLIMIT_MEMLOCK`; raising the limit is an
opt-in optimization knob only. The R8-scale shipping gate is therefore bound
to the separately gated unregistered-buffer backend. MockDriver evidence
remains an upper-bound experiment, and a smaller registered-buffer run may
characterize the curve but does not discharge the same claim.

R8 kept retained frames `Resident` by rejecting them after CLOCK proposed a
victim. `evict_one_victim` could call the bounded CLOCK sweep up to twice per
frame count while continuing past retained candidates. Saturation therefore
created bounded but potentially quadratic miss-path work and gave the caller no
typed distinction between successful victim enqueue and a complete failed
sweep. This is a performance/admission defect, not a demonstrated
use-after-free: R8 rechecked retention before reuse.

The R8 forced-pressure precondition ran inside candidate setup and a failure
would have aborted timing, but the retained artifact set contains no separate
pressure-smoke record. It therefore does not independently discharge that
adoption precondition.

The earlier `ResidentFileLease` is a distinct mechanism: it blocks file-slot
retirement only. Hinted reads still validate the volatile residency stamp and
fall back to ordinary `get`; it does not remove the per-read guard and does not
interact with CLOCK victim selection.

## Interpretation boundary and rerun preconditions

The result pair matches the lower-bound structure of warm access: removing
the per-read guard/translation beat the guarded pool path by 23 ns, while
the retained path landed within tie range of the locator mmap arm (+5 ns,
ratio 1.0223). A warm mapped load with a valid PTE is an ordinary
virtual-memory dereference; a pool that removes all per-read software work
approaches that lower bound but — at equal bytes, equal layout, and equal
page-size/TLB state — cannot undercut it (the vmcache measurements show the
same boundary: full custom path within ~8% of a raw memory read, hash-table
translation far above; Leis et al., "Virtual-Memory Assisted Buffer
Management", SIGMOD 2023). Two limits bind any citation of these numbers:

- Both comparisons are end-to-end. The −23 ns conflates guard removal with
  the sira-side probe/lease removal (stated above), and the +5 ns likewise
  does not isolate the retained access itself.
- This disposition records no locator-arm warm-state control. Page-cache
  residency proves neither PTE presence nor TLB state, so a run labelled an
  exact R8 rerun must prefault the locator mapping (`MADV_POPULATE_READ`),
  record timed-region minor-fault counts on both arms and require zero, and
  pin sira's `BlockVerification` mode identically across arms.
- Sira's segment layout has since changed: `feat/sira-sap1-format` adopts
  SAP1 aligned-frame superblocks, postdating the frozen source bases above.
  Both arms of any future pair must run one identical layout, and a pair on
  the new layout is a new baseline, not an R8 reproduction — R8-exact
  identity stays bound to the Provenance source bases.
- The memlock probe's attribution "the probe ring also consumes locked
  memory" needs re-verification: io_uring ring memory moved to memcg
  accounting in kernel 5.12 and does not charge `RLIMIT_MEMLOCK` on the
  6.6 host, and 1,984 frames alone are 7.75 MiB — the entire failure is
  a 256 KiB unattributed charge that a realistic probe ring could not
  produce even pre-5.12. Candidates, in order of parsimony: (1) the
  probe registered TWO arenas — its own record states "two iovecs total
  (read arena + write arena)" (landscape-audit.md), so the frame count
  understates pinned bytes by the write arena's size; (2)
  whole-compound-page pinning — registering buffers over a THP-backed
  arena pins (and charges) entire 2 MiB pages even where only 4 KiB is
  used; (3) pre-5.12 ring accounting, excluded on this host. The probe's
  other rows bound the overhead (256 frames = 1.0 MiB failed at the
  1 MiB limit; 1,024 frames = 4.0 MiB failed at the 4 MiB limit), so it
  is at least ~0.25 MiB and plausibly larger. A rerun records the write
  arena's size, the arena's `AnonHugePages`, and `VmPin` from
  `/proc/PID/status` (io_uring pins charge `pinned_vm`, visible as
  `VmPin`; `VmLck` does NOT reflect io_uring pins and would silently
  read zero) — noting `locked_vm` accounting is per-UID, so a stray
  same-user io_uring or mlock consumer perturbs the boundary.

An outright retained-arm win over the locator is not expected from the
access path at any scale. The sanctioned lever is memory architecture: a
THP-backed arena (`MADV_HUGEPAGE` on anonymous pool memory — no memlock
charge — given a 2 MiB-aligned base and an admitting THP policy) covers the
31.4 MiB retained set with ~16 TLB entries where 4 KiB pages overrun the
pinned host's L2 dTLB reach. The lever is asymmetric only against an
un-madvised or ext4 file-mapping baseline: on XFS >= 5.18 a
2 MiB-file-aligned madvised file mapping MAY itself obtain PMD folios
(unverified on the 6.6.64 host), so any pair claiming the lever records
both sides' `AnonHugePages`/`FilePmdMapped`.

## Design consequence

This scope preserves R8's consumer shape, not its retention array:

1. A bounded read session acquires each distinct page once through ordinary
   `get` and promotes its guard with `into_retained`.
2. Setup is all-or-nothing. Any refusal drops already promoted handles and the
   consumer falls back to guarded reads or copy-out.
3. The consumer freezes a fixed-capacity table of `RetainedFrame` owners and a
   precomposed sequence of integer `(retained_index, byte bounds)` descriptors.
4. Timed/successive reads dereference that table directly. They perform no
   `get`, epoch pin, residency-stamp validation, CLOCK touch, retention atomic,
   allocation, or copy.
5. Dropping the session drops the retained handles. Dios may logically evict a
   retained frame before then, but physical reuse is deferred as `HELD`; CLOCK
   does not repeatedly skip the frame.

The builder's `max_retained_frames` budget and augmented INV-9 watermark make
the R8 saturation state unrepresentable. `retained_evictions_held` records the
selected design's actual pressure signal: logical evictions whose physical
reuse was deferred by a live retained handle.
