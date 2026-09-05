//! `table_population`: constructing the page table of a 240,241-frame pool
//! (524,288 slots, the DM009 pool) and touching every slot (base, the fault
//! per page the eager fill construction used to pay) against constructing it
//! alone (candidate, the zero-page mapping). The ratio is the share of the
//! old table cost a build still pays. The gate (ci95 upper <= 0.10) is
//! asserted by the shared compare harness, never in-bench; run on the pinned
//! host, not here.

use std::hint::black_box;
use std::path::Path;

use dios::bench::{ratio_gate, run_paired, write_samples};
use dios::testing::PageTable;

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
            let table = PageTable::with_frame_count(FRAMES);
            table.populate();
            black_box(&table);
        },
        || {
            let table = PageTable::with_frame_count(FRAMES);
            black_box(&table);
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
