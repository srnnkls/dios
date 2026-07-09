//! Paired-benchmark support for gate benches: interleaved A/B sample
//! capture and the shared ratio statistic (one-sided 95% CI upper bound
//! of the geometric-mean ratio, percentile bootstrap). Thresholds and
//! workloads live in `benches/plans/`.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

const REPS_MIN: usize = 30;
const RESAMPLES_MIN: u32 = 1_000;
const SAMPLE_ROWS_MAX: usize = 100_000;
const BOOTSTRAP_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
const SAMPLES_HEADER: &str = "base_ns,candidate_ns";

#[derive(Debug, Clone)]
pub struct PairedSamples {
    pub name: String,
    pub base_ns: Vec<u64>,
    pub candidate_ns: Vec<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct RatioGate {
    pub ratio_geomean: f64,
    pub ratio_ci95_upper: f64,
}

/// Captures `reps` paired timings of `base` and `candidate`. The two
/// closures run interleaved within each rep, with their order alternating
/// every rep, so drift lands on both sides equally.
///
/// # Panics
///
/// Panics when `reps < 30` (the gate protocol minimum), when
/// `iters_per_rep` is zero, or when a rep measures zero nanoseconds
/// (raise `iters_per_rep`).
#[must_use]
pub fn run_paired<A, B>(
    name: &str,
    reps: u32,
    iters_per_rep: u32,
    mut base: A,
    mut candidate: B,
) -> PairedSamples
where
    A: FnMut(),
    B: FnMut(),
{
    let rep_count = usize::try_from(reps).expect("reps fits usize");
    assert!(
        rep_count >= REPS_MIN,
        "gate protocol requires reps >= {REPS_MIN}, got {reps}"
    );
    assert!(iters_per_rep >= 1, "iters_per_rep must be at least 1");

    let warmup_reps = reps.div_ceil(10);
    for _ in 0..warmup_reps {
        base();
        candidate();
    }

    let mut base_ns = Vec::with_capacity(rep_count);
    let mut candidate_ns = Vec::with_capacity(rep_count);
    for rep in 0..reps {
        if rep % 2 == 0 {
            base_ns.push(run_paired_rep(&mut base, iters_per_rep));
            candidate_ns.push(run_paired_rep(&mut candidate, iters_per_rep));
        } else {
            candidate_ns.push(run_paired_rep(&mut candidate, iters_per_rep));
            base_ns.push(run_paired_rep(&mut base, iters_per_rep));
        }
    }

    assert_eq!(base_ns.len(), candidate_ns.len());
    let zero_free = base_ns.iter().chain(&candidate_ns).all(|&ns| ns > 0);
    assert!(zero_free, "a rep measured 0 ns; raise iters_per_rep");
    PairedSamples {
        name: name.to_owned(),
        base_ns,
        candidate_ns,
    }
}

fn run_paired_rep<F: FnMut()>(f: &mut F, iters: u32) -> u64 {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    u64::try_from(start.elapsed().as_nanos()).expect("rep duration fits u64 nanoseconds")
}

/// Writes samples as `<dir>/<name>.csv` with a `base_ns,candidate_ns`
/// header, creating `dir` if needed, and returns the written path.
///
/// # Errors
///
/// Returns any filesystem error from creating `dir` or writing the file.
///
/// # Panics
///
/// Panics on empty or length-mismatched samples, or on a name containing
/// a path separator.
pub fn write_samples(dir: &Path, samples: &PairedSamples) -> io::Result<PathBuf> {
    assert!(!samples.base_ns.is_empty(), "no samples to write");
    assert_eq!(samples.base_ns.len(), samples.candidate_ns.len());
    assert!(
        !samples.name.contains(['/', '\\']),
        "sample name must not be a path"
    );

    let mut csv = String::from(SAMPLES_HEADER);
    csv.push('\n');
    for (base, candidate) in samples.base_ns.iter().zip(&samples.candidate_ns) {
        writeln!(csv, "{base},{candidate}").expect("writing to String cannot fail");
    }

    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.csv", samples.name));
    fs::write(&path, csv)?;
    Ok(path)
}

/// Reads a samples CSV produced by [`write_samples`].
///
/// # Errors
///
/// Returns `InvalidData` on a malformed header, a malformed row, an empty
/// file, or more than 100,000 rows, and any underlying filesystem error.
pub fn read_samples(path: &Path) -> io::Result<PairedSamples> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    if header != SAMPLES_HEADER {
        return Err(read_samples_error(path, &format!("bad header {header:?}")));
    }

    let mut base_ns = Vec::new();
    let mut candidate_ns = Vec::new();
    for line in lines {
        if base_ns.len() >= SAMPLE_ROWS_MAX {
            return Err(read_samples_error(
                path,
                &format!("more than {SAMPLE_ROWS_MAX} rows"),
            ));
        }
        let Some((base, candidate)) = line.split_once(',') else {
            return Err(read_samples_error(path, &format!("bad row {line:?}")));
        };
        base_ns.push(read_samples_field(path, base)?);
        candidate_ns.push(read_samples_field(path, candidate)?);
    }
    if base_ns.is_empty() {
        return Err(read_samples_error(path, "no sample rows"));
    }

    let name = path.file_stem().map_or_else(
        || "samples".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    Ok(PairedSamples {
        name,
        base_ns,
        candidate_ns,
    })
}

fn read_samples_field(path: &Path, field: &str) -> io::Result<u64> {
    field
        .trim()
        .parse::<u64>()
        .map_err(|err| read_samples_error(path, &format!("bad value {field:?}: {err}")))
}

fn read_samples_error(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {reason}", path.display()),
    )
}

/// Computes the candidate/base ratio statistic: geometric mean and the
/// one-sided 95% CI upper bound of the mean log-ratio, via percentile
/// bootstrap with a fixed seed (reruns reproduce exactly).
///
/// # Panics
///
/// Panics on fewer than 30 pairs, a length mismatch, a zero sample, or
/// fewer than 1,000 resamples.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "durations and counts stay far below 2^53"
)]
pub fn ratio_gate(samples: &PairedSamples, resamples: u32) -> RatioGate {
    let pair_count = samples.base_ns.len();
    assert!(
        pair_count >= REPS_MIN,
        "gate protocol requires >= {REPS_MIN} pairs"
    );
    assert_eq!(pair_count, samples.candidate_ns.len());
    assert!(
        resamples >= RESAMPLES_MIN,
        "bootstrap needs >= {RESAMPLES_MIN} resamples"
    );

    let log_ratios: Vec<f64> = samples
        .base_ns
        .iter()
        .zip(&samples.candidate_ns)
        .map(|(&base, &candidate)| {
            assert!(base > 0, "zero base sample");
            assert!(candidate > 0, "zero candidate sample");
            (candidate as f64 / base as f64).ln()
        })
        .collect();
    let mean = log_ratios.iter().sum::<f64>() / pair_count as f64;

    let pair_count_u64 = u64::try_from(pair_count).expect("pair count fits u64");
    let mut rng_state = BOOTSTRAP_SEED;
    let mut means = Vec::with_capacity(usize::try_from(resamples).expect("resamples fits usize"));
    for _ in 0..resamples {
        let mut sum = 0.0_f64;
        for _ in 0..pair_count {
            let draw = splitmix64(&mut rng_state);
            let idx = usize::try_from(draw % pair_count_u64).expect("index fits usize");
            sum += log_ratios[idx];
        }
        means.push(sum / pair_count as f64);
    }
    means.sort_by(f64::total_cmp);

    let upper_rank = usize::try_from(resamples).expect("resamples fits usize") * 95;
    let upper_idx = upper_rank.div_ceil(100) - 1;
    RatioGate {
        ratio_geomean: mean.exp(),
        ratio_ci95_upper: means[upper_idx].exp(),
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_matches_vigna_reference_vectors_for_seed_1234567() {
        let mut state = 1_234_567_u64;
        let expected = [
            6_457_827_717_110_365_317_u64,
            3_203_168_211_198_807_973,
            9_817_491_932_198_370_423,
            4_593_380_528_125_082_431,
            16_408_922_859_458_223_821,
        ];
        for want in expected {
            assert_eq!(splitmix64(&mut state), want);
        }
    }

    #[test]
    fn ratio_gate_recovers_an_exact_constant_ratio() {
        let base_ns: Vec<u64> = (0..64_u64).map(|i| 1_000 + i).collect();
        let candidate_ns: Vec<u64> = base_ns.iter().map(|&ns| ns * 2).collect();
        let samples = PairedSamples {
            name: "exact".to_owned(),
            base_ns,
            candidate_ns,
        };

        let gate = ratio_gate(&samples, 2_000);

        assert!((gate.ratio_geomean - 2.0).abs() < 1e-9);
        assert!((gate.ratio_ci95_upper - 2.0).abs() < 1e-9);
    }

    #[test]
    fn ratio_gate_upper_bound_sits_above_the_geomean_under_noise() {
        let base_ns: Vec<u64> = vec![1_000; 64];
        let candidate_ns: Vec<u64> = (0..64_u64)
            .map(|i| if i % 2 == 0 { 950 } else { 1_050 })
            .collect();
        let samples = PairedSamples {
            name: "noisy".to_owned(),
            base_ns,
            candidate_ns,
        };

        let gate = ratio_gate(&samples, 2_000);

        assert!(gate.ratio_ci95_upper > gate.ratio_geomean);
        assert!((gate.ratio_geomean - 1.0).abs() < 0.01);
    }

    #[test]
    fn samples_round_trip_through_csv() {
        let dir = std::env::temp_dir().join(format!("dios-bench-{}", std::process::id()));
        let samples = PairedSamples {
            name: "round_trip".to_owned(),
            base_ns: (0..32_u64).map(|i| 100 + i).collect(),
            candidate_ns: (0..32_u64).map(|i| 200 + i).collect(),
        };

        let path = write_samples(&dir, &samples).unwrap();
        let read = read_samples(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(read.name, samples.name);
        assert_eq!(read.base_ns, samples.base_ns);
        assert_eq!(read.candidate_ns, samples.candidate_ns);
    }

    #[test]
    fn read_samples_rejects_a_bad_header() {
        let dir = std::env::temp_dir().join(format!("dios-bench-hdr-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.csv");
        fs::write(&path, "wrong,header\n1,2\n").unwrap();

        let err = read_samples(&path).unwrap_err();
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn run_paired_captures_the_requested_rep_count() {
        let mut base_counter = 0_u64;
        let mut candidate_counter = 0_u64;
        let samples = run_paired(
            "reps",
            30,
            8,
            || base_counter = base_counter.wrapping_add(1),
            || candidate_counter = candidate_counter.wrapping_add(2),
        );

        assert_eq!(samples.base_ns.len(), 30);
        assert_eq!(samples.candidate_ns.len(), 30);
    }
}
