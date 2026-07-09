use std::env;
use std::path::Path;
use std::process::ExitCode;

use dios::bench::{ratio_gate, read_samples};

const BOOTSTRAP_RESAMPLES: u32 = 10_000;

fn main() -> ExitCode {
    let args: Vec<String> = env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    let [csv, max_ratio_arg] = args.as_slice() else {
        println!(
            "usage: cargo bench --features bench --bench compare -- <samples.csv> <max_ratio>"
        );
        return ExitCode::SUCCESS;
    };

    let max_ratio: f64 = match max_ratio_arg.parse() {
        Ok(ratio) if ratio > 0.0 => ratio,
        Ok(_) | Err(_) => {
            eprintln!("max_ratio must be a positive number, got {max_ratio_arg:?}");
            return ExitCode::FAILURE;
        }
    };
    let samples = match read_samples(Path::new(csv)) {
        Ok(samples) => samples,
        Err(err) => {
            eprintln!("cannot read {csv}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let gate = ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let pass = gate.ratio_ci95_upper <= max_ratio;
    let verdict = if pass { "PASS" } else { "FAIL" };
    println!(
        "gate {}: pairs {}, ratio geomean {:.4}, ci95 upper {:.4}, threshold {max_ratio:.4} -> {verdict}",
        samples.name,
        samples.base_ns.len(),
        gate.ratio_geomean,
        gate.ratio_ci95_upper
    );
    if pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
