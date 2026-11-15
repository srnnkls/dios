# Evidence: array_translation

Evidence for the [array_translation bench plan](../../plans/array_translation.md).
No campaign has run. The [capture contract](../../plans/array_translation.md#capture-contract)
pins one frozen manifest and one capture command per gate; AT8 retains every pair here.

## Layout

| Path | Content |
|------|---------|
| `manifests/at-g1.json` .. `manifests/at-g7.json` | Frozen pre-run manifests, committed before the first measured pair of their gate |
| `at-gN/manifest.json` | Manifest the capture wrote, compared field for field with the frozen one |
| `at-gN/rows/` | Rich process rows as captured |
| `at-gN/paired.csv` | `base_ns,candidate_ns` pairs derived from the validated rows |
| `at-gN/gate.txt` | Output of the `mise run gate` command of the gate |
| `at-g3/hugepage.json` | Hugepage witness of every AT-G3 run |

## Hugepage witness (AT-G3)

Each AT-G3 run records, before the first and after the last timed iteration:

- `transparent_hugepage`: the bracketed mode read from `/sys/kernel/mm/transparent_hugepage/enabled`.
- `anonhugepages_kib`: the `AnonHugePages` line of the arena mapping in the process's own `/proc/self/smaps`.
- `arena_kib`: the `Size` line of that mapping.

A run is valid only when `anonhugepages_kib` equals `arena_kib` at both points. An
invalid run is excluded and recorded, never counted as a failure or a pass. The AT-G3
manifest pins the expected THP mode.

## Prerequisite for AT-G7

The plan's [workload contract](../../plans/array_translation.md#workload-contract) owns
the DRP prerequisite of AT-G7.
