use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use sha2::{Digest as _, Sha256};

/// Content of the legacy lanes at `604410a` (AC-19).
const FROZEN_LEGACY_LANES: [(&str, &str); 3] = [
    (
        "benches/mmap_workloads/resident.rs",
        "e19890fbce7ba38aac12a9b1ca1b0da1c4b0295db15b9d3921b4ebf917320fe4",
    ),
    (
        "benches/mmap_workloads/engine.rs",
        "9d7e147c165120db115877616c20a68d4969fa1259f337f75a9ba31984ed30ab",
    ),
    (
        "benches/mmap_tlb_pressure.rs",
        "b2379867942262ce7f55b05443275bcb6094fe599dacd642a9b6471473e0ef48",
    ),
];

const FAIR_LANE_MODULE_PATH: &str = "benches/mmap_workloads/fair.rs";

fn read_repository_file(relative_path: &str) -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path)).unwrap_or_default()
}

#[test]
fn legacy_lanes_keep_their_604410a_content_and_the_fair_arms_live_in_the_fair_lane_module() {
    let drifted: Vec<&str> = FROZEN_LEGACY_LANES
        .iter()
        .filter(|(path, sha256)| {
            let digest = Sha256::digest(read_repository_file(path));
            let actual = digest.iter().fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
            actual != *sha256
        })
        .map(|(path, _)| *path)
        .collect();
    assert!(
        drifted.is_empty(),
        "legacy lanes differ from their content at 604410a: {drifted:?}"
    );

    let fair = String::from_utf8(read_repository_file(FAIR_LANE_MODULE_PATH)).unwrap_or_default();
    for arm in ["Arm::DiosSession", "Arm::DiosGet", "Arm::PlainArray"] {
        assert!(
            fair.contains(arm),
            "{FAIR_LANE_MODULE_PATH} must serve the `{arm}` arm"
        );
    }
}
