use std::fs;
use std::hint::black_box;
use std::path::Path;

use sha2::{Digest as _, Sha256};

pub(crate) const BASE_SOURCE_COMMIT: &str = "4264896e7d2e1a2a5d6d71322a46cb7d8a3de7e7";
pub(crate) const CANDIDATE_SOURCE_COMMIT: &str = "10faa9ec8b98dab5209313cf83b5d84a3fa0e954";
pub(crate) const GRANULE_BYTES: u32 = 4096;
pub(crate) const PAIR_COUNT: u32 = 40;
pub(crate) const SEGMENT_LAYOUT: &str = "pfr-flat-4096-v1";
pub(crate) const PROCESS_HEADER: &str = "lane,pair,order,arm,workload,process_id,process_start_ticks,source_commit,executable_sha256,cargo_lock_sha256,harness_cargo_lock_sha256,rust_version,runner_sha256,build_profile,arguments_sha256,provenance_sha256,iterations,useful_operations,useful_bytes,elapsed_ns,checksum,allocations,cpu_set,thread_affinities,thread_minflt,thread_majflt,pool_capacity,retained_pages,arena_posture,arena_base,arena_span,frame_bytes,kernel_page_bytes,mmu_page_bytes,anon_huge_bytes,segment_layout,corpus_sha256,memlock_soft,memlock_hard,retention_budget,occupied_budget,refused_budget,refused_ceiling,refused_contention,refused_retiring,retained_evictions_held,reclaimed_frames,backend_completions,evictions,wake_cycles,parked_wakes,wake_acks,ring_drains,held_transitions,governor,kernel,numa_nodes,numa_policy,numa_balancing,storage,runner_host,os_hostname,cpu_model,cpu_topology";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lane {
    TransientGuard,
    NonzeroPoll,
    ZeroBudgetBypass,
    PromoteReleaseWake,
    SameFramePromotion,
}

impl Lane {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pfr_transient_guard" => Ok(Self::TransientGuard),
            "pfr_nonzero_poll" => Ok(Self::NonzeroPoll),
            "pfr_zero_budget_bypass" => Ok(Self::ZeroBudgetBypass),
            "pfr_promote_release_wake" => Ok(Self::PromoteReleaseWake),
            "pfr_same_frame_promotion" => Ok(Self::SameFramePromotion),
            _ => Err(format!("unknown frozen PFR lane {value:?}")),
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::TransientGuard => "pfr_transient_guard",
            Self::NonzeroPoll => "pfr_nonzero_poll",
            Self::ZeroBudgetBypass => "pfr_zero_budget_bypass",
            Self::PromoteReleaseWake => "pfr_promote_release_wake",
            Self::SameFramePromotion => "pfr_same_frame_promotion",
        }
    }

    pub(crate) const fn arms(self) -> (&'static str, &'static str) {
        match self {
            Self::TransientGuard | Self::ZeroBudgetBypass => ("base", "candidate"),
            Self::NonzeroPoll => ("zero", "budget64"),
            Self::PromoteReleaseWake => ("transient", "retained"),
            Self::SameFramePromotion => ("one_worker", "eight_workers"),
        }
    }

    pub(crate) fn validate_arm(self, arm: &str) -> Result<(), String> {
        let (base, candidate) = self.arms();
        if arm == base || arm == candidate {
            Ok(())
        } else {
            Err(format!("unknown arm {arm:?} for lane {:?}", self.name()))
        }
    }

    pub(crate) const fn cpu_set(self) -> &'static str {
        match self {
            Self::PromoteReleaseWake => "0-1",
            Self::SameFramePromotion => "0-3,32-35",
            Self::TransientGuard | Self::NonzeroPoll | Self::ZeroBudgetBypass => "0",
        }
    }

    pub(crate) const fn iterations(self) -> u64 {
        match self {
            Self::TransientGuard => 8_192,
            Self::NonzeroPoll | Self::ZeroBudgetBypass => 256,
            Self::PromoteReleaseWake => 4_096,
            Self::SameFramePromotion => 1_000_000,
        }
    }

    pub(crate) const fn pool_capacity(self) -> u32 {
        match self {
            Self::TransientGuard | Self::NonzeroPoll => 256,
            Self::ZeroBudgetBypass => 128,
            Self::PromoteReleaseWake => 64,
            Self::SameFramePromotion => 20,
        }
    }

    pub(crate) const fn workload(self) -> &'static str {
        match self {
            Self::TransientGuard => "shipping_registered_128_page_warm_get_64_byte_fold_v1",
            Self::NonzeroPoll => "shipping_registered_256_batches_64_matured_reclaims_v1",
            Self::ZeroBudgetBypass => "shipping_registered_96_hit_16_miss_clock_cycle_v1",
            Self::PromoteReleaseWake => "shipping_registered_4096_ownership_64_parked_wakes_v1",
            Self::SameFramePromotion => {
                "shipping_registered_same_frame_1000000_attempts_per_worker_v1"
            }
        }
    }

    pub(crate) const fn retention_budget(self, arm: &str) -> u32 {
        match self {
            Self::NonzeroPoll if matches!(arm.as_bytes(), b"budget64") => 64,
            Self::PromoteReleaseWake | Self::SameFramePromotion => 1,
            _ => 0,
        }
    }

    pub(crate) const fn source_comparison(self) -> bool {
        matches!(self, Self::TransientGuard | Self::ZeroBudgetBypass)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RetentionEvidence {
    pub(crate) occupied_budget: u32,
    pub(crate) refused_budget: u64,
    pub(crate) refused_ceiling: u64,
    pub(crate) refused_contention: u64,
    pub(crate) refused_retiring: u64,
    pub(crate) retained_evictions_held: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FaultDelta {
    pub(crate) minor: u64,
    pub(crate) major: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ThreadEvidence {
    pub(crate) affinities: String,
    pub(crate) faults: Vec<FaultDelta>,
}

#[derive(Clone, Debug)]
pub(crate) struct ArenaEvidence {
    pub(crate) base: usize,
    pub(crate) span: u64,
    pub(crate) kernel_page_bytes: u64,
    pub(crate) mmu_page_bytes: u64,
    pub(crate) anon_huge_bytes: u64,
    pub(crate) numa_policy: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Measurement {
    pub(crate) iterations: u64,
    pub(crate) useful_operations: u64,
    pub(crate) useful_bytes: u64,
    pub(crate) elapsed_ns: u64,
    pub(crate) checksum: u64,
    pub(crate) allocations: u64,
    pub(crate) threads: ThreadEvidence,
    pub(crate) arena: ArenaEvidence,
    pub(crate) pool_capacity: u32,
    pub(crate) retained_pages: u32,
    pub(crate) retention: RetentionEvidence,
    pub(crate) reclaimed_frames: u64,
    pub(crate) backend_completions: u64,
    pub(crate) evictions: u64,
    pub(crate) wake_cycles: u64,
    pub(crate) parked_wakes: u64,
    pub(crate) wake_acks: u64,
    pub(crate) ring_drains: u64,
    pub(crate) held_transitions: u64,
}

pub(crate) fn fold_bytes(bytes: &[u8], offset: usize) -> u64 {
    assert!(
        offset + 64 <= bytes.len(),
        "a folded range lies within one frame"
    );
    bytes[offset..offset + 64]
        .iter()
        .map(|&byte| u64::from(black_box(byte)))
        .sum()
}

pub(crate) fn descriptor_offset(ordinal: u64) -> usize {
    let range = u64::from(GRANULE_BYTES - 64);
    usize::try_from((ordinal.wrapping_mul(73) % range) & !7).expect("frame offset fits usize")
}

pub(crate) fn sha256_path(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn parse_number<T: std::str::FromStr>(value: &str, field: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {field} value {value:?}"))
}

pub(crate) fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
