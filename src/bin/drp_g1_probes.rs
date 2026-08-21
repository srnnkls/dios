use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use sha2::{Digest, Sha256};

const HEADER: &str = "phase,driver,slots,files,pattern,seed,current_mean,current_p99,current_max,candidate_mean,candidate_p99,candidate_max,generator_sha256";
const ARTIFACT_BYTES_MAX: u64 = 1 << 20;
const CALIBRATION_DRIVER: u64 = 0x00d1_05ee_d000_0001;
const HOLDOUT_DRIVERS: [u64; 2] = [0x71c3_5a09_d4e2_b687, 0xd903_4f61_28bc_7a55];
const TABLE_SIZES_CALIBRATION: [u32; 3] = [1_024, 131_072, 524_288];
const TABLE_SIZES_HOLDOUT: [u32; 3] = [2_048, 65_536, 262_144];
const FILE_COUNTS_CALIBRATION: [u32; 3] = [1, 16, 256];
const FILE_COUNTS_HOLDOUT: [u32; 3] = [3, 31, 127];
const SHUFFLE_SEEDS: [u64; 2] = [0x243f_6a88_85a3_08d3, 0x1319_8a2e_0370_7344];
const PHI: u64 = 0x9e37_79b9_7f4a_7c15;
const ROWS_EXPECTED: usize = 72;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    driver: u64,
    slot: u32,
    generation: u32,
    granule: u32,
}

#[derive(Debug, Clone, Copy)]
struct ProbeStats {
    sum: u64,
    count: u32,
    p99: u32,
    max: u32,
}

impl ProbeStats {
    #[expect(
        clippy::cast_precision_loss,
        reason = "the fixed matrix bounds the probe sum below 2^53, so f64 represents its integer value exactly"
    )]
    fn mean(self) -> f64 {
        self.sum as f64 / f64::from(self.count)
    }
}

#[derive(Debug)]
struct Row {
    phase: &'static str,
    driver: u64,
    slots: u32,
    files: u32,
    pattern: &'static str,
    seed: u64,
    current: ProbeStats,
    candidate: ProbeStats,
}

impl Row {
    fn identity(&self) -> String {
        format!(
            "{},{:016x},{},{},{},{:016x}",
            self.phase, self.driver, self.slots, self.files, self.pattern, self.seed
        )
    }

    fn csv(&self, generator_sha256: &str) -> String {
        format!(
            "{},{:.9},{},{},{:.9},{},{},{}",
            self.identity(),
            self.current.mean(),
            self.current.p99,
            self.current.max,
            self.candidate.mean(),
            self.candidate.p99,
            self.candidate.max,
            generator_sha256
        )
    }

    fn quality_error(&self) -> Option<String> {
        if self.candidate.sum * 100 > self.current.sum * 105 {
            return Some(format!(
                "{} candidate_mean threshold failed",
                self.identity()
            ));
        }
        if !self.p99_passes() {
            return Some(format!(
                "{} candidate_p99 threshold failed",
                self.identity()
            ));
        }
        if self.current.max >= 64 || self.candidate.max >= 64 {
            return Some(format!(
                "{} maximum-chain threshold failed",
                self.identity()
            ));
        }
        None
    }

    fn p99_passes(&self) -> bool {
        if self.phase == "calibration" && self.slots == 1_024 {
            self.candidate.p99 <= self.current.p99 + 1
        } else {
            self.candidate.p99 * 100 <= self.current.p99 * 105
        }
    }
}

fn mix(mut word: u64) -> u64 {
    word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    word ^ (word >> 31)
}

fn current_hash(key: Key) -> u64 {
    dios::testing::current_page_hash(key.driver, key.slot, key.generation, key.granule)
}

fn candidate_hash(key: Key) -> u64 {
    let file_page = (u64::from(key.generation) << 32) | u64::from(key.granule);
    let seed = key.driver ^ file_page ^ u64::from(key.slot).wrapping_mul(PHI);
    mix(seed)
}

fn probe_stats(
    keys: &[Key],
    order: &[u32],
    slots: u32,
    hash: fn(Key) -> u64,
    nearest_rank: bool,
) -> ProbeStats {
    assert_eq!(
        keys.len(),
        order.len(),
        "every key has one insertion ordinal"
    );
    assert!(
        slots.is_power_of_two(),
        "the probe table size is a power of two"
    );
    let mask = slots - 1;
    let mut table = vec![None; slots as usize];
    for &ordinal in order {
        let key = keys[ordinal as usize];
        let mut slot =
            u32::try_from(hash(key) & u64::from(mask)).expect("the masked hash fits u32");
        for remaining in (1..=slots).rev() {
            if table[slot as usize].is_none() {
                table[slot as usize] = Some(key);
                break;
            }
            assert!(remaining > 1, "the 50%-load table always has a vacant slot");
            slot = (slot + 1) & mask;
        }
    }
    successful_probe_stats(keys, &table, mask, hash, nearest_rank)
}

fn successful_probe_stats(
    keys: &[Key],
    table: &[Option<Key>],
    mask: u32,
    hash: fn(Key) -> u64,
    nearest_rank: bool,
) -> ProbeStats {
    let mut probes = Vec::with_capacity(keys.len());
    for &key in keys {
        let mut slot =
            u32::try_from(hash(key) & u64::from(mask)).expect("the masked hash fits u32");
        for probe in 1..=mask + 1 {
            if table[slot as usize] == Some(key) {
                probes.push(probe);
                break;
            }
            assert!(probe <= mask, "an inserted full key must be found");
            slot = (slot + 1) & mask;
        }
    }
    probes.sort_unstable();
    let count = u32::try_from(probes.len()).expect("matrix row count fits u32");
    let rank = if nearest_rank {
        (99 * count).div_ceil(100)
    } else {
        99 * count / 100
    };
    ProbeStats {
        sum: probes.iter().map(|&probe| u64::from(probe)).sum(),
        count,
        p99: probes[(rank - 1) as usize],
        max: *probes.last().expect("a row contains keys"),
    }
}

fn calibration_keys(slots: u32, files: u32, interleaved: bool) -> Vec<Key> {
    let count = slots / 2;
    assert_eq!(count % files, 0, "calibration keys divide evenly by files");
    let per_file = count / files;
    let mut keys = Vec::with_capacity(count as usize);
    for file in 0..files {
        for ordinal in 0..per_file {
            let granule = if interleaved {
                let block = ordinal / 64;
                block * 64 * files + file * 64 + ordinal % 64
            } else {
                ordinal
            };
            keys.push(Key {
                driver: CALIBRATION_DRIVER,
                slot: file,
                generation: 2 * file + 1,
                granule,
            });
        }
    }
    assert_eq!(keys.len(), count as usize, "calibration row is at 50% load");
    keys
}

fn holdout_keys(driver: u64, slots: u32, files: u32) -> Vec<Key> {
    let count = slots / 2;
    let mut keys = Vec::with_capacity(count as usize);
    for ordinal in 0..count {
        let file = ordinal % files;
        keys.push(Key {
            driver,
            slot: 3 + 5 * file,
            generation: 0x8000_0001 + 17 * file,
            granule: 11 + 257 * (ordinal / files),
        });
    }
    assert_eq!(keys.len(), count as usize, "holdout row is at 50% load");
    keys
}

fn insertion_order(count: u32, seed: u64) -> Vec<u32> {
    let mut order: Vec<u32> = (0..count).collect();
    if seed == 0 {
        return order;
    }
    let mut state = seed;
    for upper in (1..count).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let lower = u32::try_from(state % u64::from(upper + 1)).expect("shuffle index fits u32");
        order.swap(upper as usize, lower as usize);
    }
    order
}

fn make_row(
    phase: &'static str,
    driver: u64,
    slots: u32,
    files: u32,
    pattern: &'static str,
    seed: u64,
    keys: &[Key],
) -> Row {
    let order = insertion_order(u32::try_from(keys.len()).expect("key count fits u32"), seed);
    let nearest_rank = phase == "holdout";
    Row {
        phase,
        driver,
        slots,
        files,
        pattern,
        seed,
        current: probe_stats(keys, &order, slots, current_hash, nearest_rank),
        candidate: probe_stats(keys, &order, slots, candidate_hash, nearest_rank),
    }
}

fn calibration_rows(rows: &mut Vec<Row>) {
    for slots in TABLE_SIZES_CALIBRATION {
        for files in FILE_COUNTS_CALIBRATION {
            for (pattern, interleaved) in [("sequential", false), ("interleaved", true)] {
                let keys = calibration_keys(slots, files, interleaved);
                rows.push(make_row(
                    "calibration",
                    CALIBRATION_DRIVER,
                    slots,
                    files,
                    pattern,
                    0,
                    &keys,
                ));
            }
        }
    }
}

fn holdout_rows(rows: &mut Vec<Row>) {
    for driver in HOLDOUT_DRIVERS {
        for slots in TABLE_SIZES_HOLDOUT {
            for files in FILE_COUNTS_HOLDOUT {
                let keys = holdout_keys(driver, slots, files);
                rows.push(make_row(
                    "holdout",
                    driver,
                    slots,
                    files,
                    "round_robin",
                    0,
                    &keys,
                ));
                for seed in SHUFFLE_SEEDS {
                    rows.push(make_row(
                        "holdout", driver, slots, files, "shuffled", seed, &keys,
                    ));
                }
            }
        }
    }
}

fn expected_rows() -> Vec<Row> {
    let mut rows = Vec::with_capacity(ROWS_EXPECTED);
    calibration_rows(&mut rows);
    holdout_rows(&mut rows);
    assert_eq!(rows.len(), ROWS_EXPECTED, "the frozen matrix has 72 rows");
    rows
}

fn generator_sha256() -> String {
    let mut digest = Sha256::new();
    digest.update(include_bytes!("drp_g1_probes.rs"));
    digest.update([0]);
    digest.update(include_bytes!("../pool/table.rs"));
    let digest = digest.finalize();
    format!("{digest:x}")
}

fn generate(path: &Path) -> Result<(), String> {
    let hash = generator_sha256();
    let rows = expected_rows();
    let mut artifact = String::with_capacity(16 * 1024);
    writeln!(artifact, "{HEADER}").expect("String writes cannot fail");
    for row in &rows {
        writeln!(artifact, "{}", row.csv(&hash)).expect("String writes cannot fail");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create artifact directory: {error}"))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("freeze new quality artifact: {error}"))?;
    file.write_all(artifact.as_bytes())
        .map_err(|error| format!("write quality artifact: {error}"))?;
    println!("generated {} rows; generator_sha256={hash}", rows.len());
    Ok(())
}

fn parse_number<T: std::str::FromStr>(field: &str, value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{field} is not a valid number: {value:?}"))
}

fn validate_fields(columns: &[&str]) -> Result<(), String> {
    let names = HEADER.split(',').collect::<Vec<_>>();
    for index in [2, 3, 7, 8, 10, 11] {
        let _: u32 = parse_number(names[index], columns[index])?;
    }
    for index in [6, 9] {
        let value: f64 = parse_number(names[index], columns[index])?;
        if !value.is_finite() {
            return Err(format!("{} is not finite", names[index]));
        }
    }
    for index in [1, 5] {
        u64::from_str_radix(columns[index], 16)
            .map_err(|_| format!("{} is not valid hexadecimal", names[index]))?;
    }
    if columns[12].len() != 64 || !columns[12].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("generator_sha256 is not a SHA-256 value".to_owned());
    }
    Ok(())
}

fn validate_row(
    line: &str,
    expected: &BTreeMap<String, String>,
    seen: &mut BTreeSet<String>,
    source_hash: &str,
) -> Result<(), String> {
    let columns = line.split(',').collect::<Vec<_>>();
    if columns.len() != 13 {
        return Err(format!("row has {} fields, expected 13", columns.len()));
    }
    validate_fields(&columns)?;
    let identity = columns[..6].join(",");
    let expected_line = expected
        .get(&identity)
        .ok_or_else(|| format!("extra row or invalid dimensions/pattern/seed: {identity}"))?;
    if !seen.insert(identity.clone()) {
        return Err(format!("duplicate row: {identity}"));
    }
    if columns[12] != source_hash {
        return Err(format!("generator hash mismatch for {identity}"));
    }
    let expected_columns = expected_line.split(',').collect::<Vec<_>>();
    let names = HEADER.split(',').collect::<Vec<_>>();
    for index in 6..12 {
        if columns[index] != expected_columns[index] {
            return Err(format!("{} mismatch for {identity}", names[index]));
        }
    }
    Ok(())
}

fn validate(path: &Path, enforce_quality: bool) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("read quality artifact: {error}"))?;
    if metadata.len() > ARTIFACT_BYTES_MAX {
        return Err("quality artifact exceeds the 1 MiB bound".to_owned());
    }
    let artifact =
        fs::read_to_string(path).map_err(|error| format!("read quality artifact: {error}"))?;
    let mut lines = artifact.lines();
    if lines.next() != Some(HEADER) {
        return Err("quality artifact header mismatch".to_owned());
    }
    let source_hash = generator_sha256();
    let rows = expected_rows();
    let expected = rows
        .iter()
        .map(|row| (row.identity(), row.csv(&source_hash)))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for line in lines {
        validate_row(line, &expected, &mut seen, &source_hash)?;
    }
    validate_completeness(&expected, &seen)?;
    if enforce_quality {
        for row in &rows {
            if let Some(error) = row.quality_error() {
                return Err(error);
            }
        }
    }
    println!("calibration: 18 rows, structure/statistics PASS");
    println!("holdout: 54 rows, structure/statistics PASS");
    println!("generator_sha256={source_hash}");
    Ok(())
}

fn validate_completeness(
    expected: &BTreeMap<String, String>,
    seen: &BTreeSet<String>,
) -> Result<(), String> {
    if expected.len() != seen.len() {
        let missing = expected
            .keys()
            .find(|identity| !seen.contains(*identity))
            .expect("unequal bounded sets have a missing identity");
        let phase = missing.split(',').next().expect("identity includes phase");
        return Err(format!("missing {phase} row: {missing}"));
    }
    Ok(())
}

fn usage() {
    println!("usage: drp_g1_probes <generate|validate> [--structure-only] <artifact.csv>");
}

fn run() -> Result<(), String> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        usage();
        return Ok(());
    }
    let structure_only = arguments
        .iter()
        .any(|argument| argument == "--structure-only");
    let positional = arguments
        .iter()
        .filter(|argument| !argument.starts_with('-'))
        .collect::<Vec<_>>();
    let [operation, path] = positional.as_slice() else {
        usage();
        return Err("expected an operation and artifact path".to_owned());
    };
    match operation.as_str() {
        "generate" => generate(Path::new(path)),
        "validate" => validate(Path::new(path), !structure_only),
        _ => Err(format!("unknown operation {operation:?}")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("DRP-G1: {error}");
            ExitCode::FAILURE
        }
    }
}
