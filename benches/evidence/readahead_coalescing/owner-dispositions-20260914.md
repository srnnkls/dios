# Owner dispositions for the two RC6 needs-decision findings

From the sibling Claude session, relaying Sören's decisions, 2026-09-14.
No bound, budget, protocol or product change is authorized by either.

## B5-P1: DRP-G4 ordinary eight-thread, upper 1.0742 > 1.00

Evidence on record: the lane is bimodal in all three campaigns (sd of the
log ratio 0.227, 0.249, 0.238); in-mode the candidate is 0.909 / 0.923
against the frozen base; the direct same-build pairing 5edb6a7 / aa97c82
is 0.9187 with upper 0.9811. The suspected branch regression from the
cross-campaign comparison did not reproduce, so the earlier instruction
not to run a larger campaign is withdrawn; its reason no longer exists.

Disposition: one pre-registered confirmation campaign.

- Same lane `drp_g4_ordinary_base_8t`, same frozen runner, same two
  executables (base 0b49dc7d..., candidate ae50a099...), same CPU set,
  iterations and order alternation, same bound 1.00, same
  `mise run gate paired.csv 1.00`.
- 2 qualification pairs plus 400 measured pairs. Power: at sd 0.238 the
  one-sided 95% half-width is 0.020 in log space; the expected geomean
  from the in-mode ratios is about 0.92.
- Pre-register before running: write the campaign declaration (lane,
  executables, pair count, bound, expected geomean, the rule that the
  result is adopted whichever way it goes) into the checkpoint and the
  RC6 evidence README, then run. The gate is asserted on the new sample
  alone. The failed 30-pair run stays in evidence beside it and is never
  pooled with it.
- This is within the frozen DRP protocol, which requires "at least 30
  fresh-process pairs". Pre-registration is what distinguishes it from
  rerun-until-pass. No second attempt if it fails; a failure stands and
  returns to the owner.

Record separately, outside RC6, as an owner follow-up: the frozen DRP-G4
lane cannot resolve a tie (about 3 ms per process, two placement modes,
7% half-width at 30 pairs). A protocol amendment, longer timed region or
one thread per physical core, is a future owner decision on
`benches/plans/dios_r1_r7_read_performance.md`.

## B5-P2: RC-G6 mechanism counters unavailable for frozen arms

The frozen executables predate the observation seam. Actual SQE/CQE
counts and request ranges, EOF and consumer-stop waste, and control-entry
visits cannot be emitted by them, and the capture contract forbids
deriving them from configuration or prefetch totals.

Disposition: amend RC-G6 to "every arm that can emit them", recorded as
an owner clarification in `benches/plans/readahead_coalescing.md` with
this reason, not a relaxation of any numeric bound (RC-G6 has none).

- Frozen arms report CPU and elapsed per useful 4 KiB, polls per page and
  the prefetch stats their product exposes. The three mechanism counters
  remain null with the limitation stated next to them.
- The candidate reports every RC-G6 field; that is the attribution the
  gate exists for.
- Optional, exact SQE totals only: a counter-only replay of the frozen
  arm under `strace -f -e trace=io_uring_enter`, summing the syscall
  return values, gives submitted SQEs without touching the executable or
  any host security setting. Discard its timing; it is not a primary
  sample. If strace is not available to the bench account, skip it and
  keep the null with the limitation. Do not spend more than this on it.
- RC-G6 is complete when the candidate fields are all present and
  qualified and the frozen nulls carry their recorded limitation.

## Then

Resume RC6 from the checkpoint: run the pre-registered campaign, update
results.json/results.md/README, re-run the targeted review, and report
the campaign's geomean and upper in your pane. Adoption follows only if
the pre-registered campaign passes and every other recorded gate stays
green as already measured.
