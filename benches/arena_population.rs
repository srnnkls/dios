//! `arena_population`: constructing a 65,536-frame (256 MiB) arena and
//! touching every frame (base, the eager fill construction used to perform)
//! against constructing it alone (candidate, the lazily populated mapping).
//! The ratio is the share of the old open cost a build still pays. The gate
//! (ci95 upper <= 0.10) is asserted by the shared compare harness, never
//! in-bench; run on the pinned host, not here.

use std::hint::black_box;
use std::path::Path;

use dios::bench::{ratio_gate, run_paired, write_samples};
use dios::testing::TestFrames;

const GRANULE: u32 = 4096;
const FRAMES: u32 = 65_536;
const REPS: u32 = 40;
const ITERS_PER_REP: u32 = 1;
const BOOTSTRAP_RESAMPLES: u32 = 10_000;

fn main() {
    let samples = run_paired(
        "arena_population",
        REPS,
        ITERS_PER_REP,
        || {
            let frames = TestFrames::preallocated(FRAMES, GRANULE);
            frames.populate();
            black_box(&frames);
        },
        || {
            let frames = TestFrames::preallocated(FRAMES, GRANULE);
            black_box(&frames);
        },
    );

    let gate = ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let path =
        write_samples(Path::new("target/bench-samples"), &samples).expect("write samples CSV");
    println!(
        "arena_population: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
        gate.ratio_geomean,
        gate.ratio_ci95_upper,
        path.display()
    );
}
