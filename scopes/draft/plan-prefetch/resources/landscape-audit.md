# Landscape audit (external review, 2026-08-18) — verdicts + dios-side attribution

External claim-by-claim audit of `landscape.md`. Verdict summary: the
scope's five containment decisions (headroom sweep before reclamation
redesign; `READ_FIXED` stays settled; no re-adding mechanisms dios has;
cooperative poll-pass work only; credits compose with INV-9) were all
upheld. Key downgrades of `landscape.md` claims:

- PostgreSQL rings/read-stream verify bounded streaming footprints and
  consumer-to-I/O seams, NOT the specific credit state machine — that
  remains dios's own design, validated by dios's own spike.
- LeanStore's newer NVMe work (PVLDB vol 16, Haas) replaced background
  page-provider threads with cooperative eviction in worker context —
  supporting poll-pass replenishment, not a background reclaimer.
- The io_uring DBMS study is PVLDB 2026 (first arXiv posting 2025-12-04);
  its +18/+11/+20/+21% increments are one YCSB setup on 8× PCIe 5.0
  drives, not universal constants.
- `landscape.md` §5/§16/§17 (watermark inventory, epochs off the
  admission path) are design proposals, not established consensus —
  already contained in scope.md and since killed by the headroom sweep.
- Alignment for O_DIRECT is per-filesystem, queryable via
  `statx(STATX_DIOALIGN)` since Linux 6.1 — do not hardcode assumptions
  beyond the settled granule.

## Memlock attribution (audit §8 asked; RESOLVED — the audit's own expectation overturned)

The audit predicted that on kernel 6.6 with `IORING_FEAT_NATIVE_WORKERS`
registered buffers use cgroup accounting and no memlock limit applies
(`io_uring_registered_buffers(7)`), and asked for causal evidence.
Measured on the pinned host (`spike/prefetch-reserve` worktree,
`benches/memlock_probe.rs`), ring reporting `native_workers=true`,
registration shape two iovecs total (read arena + write arena — buffer
count caps ruled out):

| `RLIMIT_MEMLOCK` soft | 256 frames (1 MiB) | 1024 (4 MiB) | 1984 (7.75 MiB) |
|---|---|---|---|
| 1 MiB | ENOMEM | ENOMEM | ENOMEM |
| 4 MiB | ok | ENOMEM | ENOMEM |
| 8 MiB (default) | ok | ok | ENOMEM (probe-ring overhead atop 7.75 MiB) |

The boundary moves in lockstep with the limit at three settings:
unprivileged registered-buffer memory IS charged against
`RLIMIT_MEMLOCK` on this 6.6 box despite the man-page claim. The
scope.md constraint is causal, not coincidental. The box's hard limit is
also 8 MiB, so pools beyond ~2,000 4-KiB frames need a box-level limits
change (root/PAM), not a `ulimit` call; the unregistered-buffer fallback
stays a flagged owner decision against dios-v1.

## Adopted into scope.md requirements

Exact credit accounting: no heuristic counters, no reset-on-full repair.
Credits change only at explicit transitions (reserve acquired →
speculative in-flight → promoted-on-consume | evicted-unused |
failed-submission-returned), each returning its credit exactly once.
