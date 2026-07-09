use std::hint::black_box;
use std::path::Path;

use dios::bench::{ratio_gate, run_paired, write_samples};

const GRANULE_BYTES: usize = 4096;
const REPS: u32 = 40;
const ITERS_PER_REP: u32 = 256;
const BOOTSTRAP_RESAMPLES: u32 = 10_000;
const IDENTICAL_WORKLOAD_RATIO_MAX: f64 = 1.25;

fn main() {
    let src = vec![0xA5_u8; GRANULE_BYTES];
    let mut dst_base = vec![0_u8; GRANULE_BYTES];
    let mut dst_candidate = vec![0_u8; GRANULE_BYTES];

    let samples = run_paired(
        "paired_smoke",
        REPS,
        ITERS_PER_REP,
        || {
            dst_base.copy_from_slice(black_box(&src));
            black_box(dst_base.as_slice());
        },
        || {
            dst_candidate.copy_from_slice(black_box(&src));
            black_box(dst_candidate.as_slice());
        },
    );

    let gate = ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let path =
        write_samples(Path::new("target/bench-samples"), &samples).expect("write samples CSV");

    println!(
        "paired_smoke: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
        gate.ratio_geomean,
        gate.ratio_ci95_upper,
        path.display()
    );
    assert!(
        gate.ratio_ci95_upper <= IDENTICAL_WORKLOAD_RATIO_MAX,
        "identical workloads must gate near 1.0, got ci95 upper {:.4}",
        gate.ratio_ci95_upper
    );
}
