# Characterization for AT-G8: `get` has two per-process modes that follow stack alignment

Host `nix`, 2026-11-17, bench profile active, CPU 0, THP `never`. Executable of the AT-G2
capture at `377a0ad` (default build), lane `resident_access_fair_get`, arm `dios_get`,
134,217,728 reads per fresh process. Not gate evidence.

| Condition | ns per read, one value per process |
|-----------|------------------------------------|
| ASLR on, 16 processes | 25.82 25.78 18.62 19.55 24.99 25.77 26.09 24.91 25.82 25.73 25.85 25.05 25.29 25.83 18.31 19.25 |
| ASLR off (`setarch -R`), 8 processes | 25.12 25.90 26.33 25.07 26.04 26.24 26.09 26.39 |
| ASLR off, environment padded by 16, 32, ... 128 bytes | 25.37 25.07 26.03 17.65 25.87 25.08 25.07 17.61 |
| Session arm (`dios_session`, projection lane), ASLR on, 8 processes | 5.78 5.74 5.74 5.84 5.77 5.74 5.74 5.76 |

- `get` reads at about 25.5 ns or at about 18 ns per process; 4 of 16 processes with
  ASLR are in the fast mode.
- With ASLR off every process is slow. Shifting the initial stack by 64 or 128 bytes of
  environment puts the process in the fast mode; the other shifts of 16 to 112 bytes do
  not. The mode follows the stack's position within a 64-byte cache line.
- The session read, which returns a slice in registers, has one mode.
- Consequence for AT-G8: the base arm of the gate (`get` as AT5 leaves it) draws one of
  two modes per fresh process, about a quarter fast, so the paired ratio carries that
  variance; a hit result that no longer passes through stack memory (FR-12) should
  remove the slow mode.
