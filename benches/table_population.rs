//! `table_population`: constructing the frame-scaled control tables of a
//! 240,241-frame pool (the DM009 pool: page table, miss table, frame page
//! index) and touching every slot (base, the fault per page the eager
//! constructor loops used to pay) against constructing them alone (candidate,
//! the zero-page mappings). The ratio is the share of the old table cost a
//! build still pays. The gate (ci95 upper <= 0.10) is asserted by the shared
//! compare harness, never in-bench; run on the pinned host, not here.

use std::hint::black_box;
use std::path::Path;

use dios::bench::{ratio_gate, run_paired, write_samples};
use dios::testing::ControlTables;

const FRAMES: u32 = 240_241;
const REPS: u32 = 40;
const ITERS_PER_REP: u32 = 1;
const BOOTSTRAP_RESAMPLES: u32 = 10_000;

fn main() {
    let samples = run_paired(
        "table_population",
        REPS,
        ITERS_PER_REP,
        || {
            let mut tables = ControlTables::with_frame_count(FRAMES);
            tables.populate();
            black_box(&tables);
        },
        || {
            let tables = ControlTables::with_frame_count(FRAMES);
            black_box(&tables);
        },
    );

    let gate = ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let path =
        write_samples(Path::new("target/bench-samples"), &samples).expect("write samples CSV");
    println!(
        "table_population: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
        gate.ratio_geomean,
        gate.ratio_ci95_upper,
        path.display()
    );
}
