use serde::Serialize;

pub(super) const GRANULE: u32 = 4096;
pub(super) const PAGES: u32 = 65_536;
pub(super) const WINDOW: usize = 16;
pub(super) const POLLS_MAX: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct PageNumber(pub(super) u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Lane {
    ResidentProjection,
    ResidentDecode,
    MinorProjection,
    ColdPoint,
    ColdGather,
    ColdGatherNormal,
    FaultingReaders,
    DependentWalk,
    ScanDecode,
    ScanNormal,
    HotspotMixed,
    PressureScan,
    PressureRandom,
    ResidentAccessOrdinary,
    ResidentAccessHinted,
    ResidentAccessEpochBatch,
    ResidentAccessRetained,
    ResidentAccessPageBatch,
    PrefetchFragmented,
    PrefetchFragmentedWhole,
    PrefetchScan,
    PrefetchPressureScan,
    AutomaticScan,
    AutomaticPressureScan,
    AutomaticFragmented,
    AutomaticDependent,
    PrefetchMmapScan,
    PrefetchMmapPressureScan,
    PrefetchPollution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Arm {
    MmapRandom,
    MmapNormal,
    MmapSequential,
    DiosSerial,
    DiosBatch,
    DiosHinted,
    DiosEpochBatch,
    DiosRetained,
    DiosPageBatch,
    DiosPrefetch,
    DiosPrefetchWhole,
    DiosAutomatic,
    DiosWrongHints,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Cache {
    Resident,
    Minor,
    Cold,
    Hotspot,
    Pressure,
}

impl Lane {
    pub(super) const ALL: [Self; 29] = [
        Self::ResidentProjection,
        Self::ResidentDecode,
        Self::MinorProjection,
        Self::ColdPoint,
        Self::ColdGather,
        Self::ColdGatherNormal,
        Self::FaultingReaders,
        Self::DependentWalk,
        Self::ScanDecode,
        Self::ScanNormal,
        Self::HotspotMixed,
        Self::PressureScan,
        Self::PressureRandom,
        Self::ResidentAccessOrdinary,
        Self::ResidentAccessHinted,
        Self::ResidentAccessEpochBatch,
        Self::ResidentAccessRetained,
        Self::ResidentAccessPageBatch,
        Self::PrefetchFragmented,
        Self::PrefetchFragmentedWhole,
        Self::PrefetchScan,
        Self::PrefetchPressureScan,
        Self::AutomaticScan,
        Self::AutomaticPressureScan,
        Self::AutomaticFragmented,
        Self::AutomaticDependent,
        Self::PrefetchMmapScan,
        Self::PrefetchMmapPressureScan,
        Self::PrefetchPollution,
    ];

    pub(super) fn parse(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|lane| lane.name() == name)
            .ok_or_else(|| format!("unknown workload {name}"))
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::ResidentProjection => "resident_projection",
            Self::ResidentDecode => "resident_decode",
            Self::MinorProjection => "minor_projection",
            Self::ColdPoint => "cold_point",
            Self::ColdGather => "cold_gather",
            Self::ColdGatherNormal => "cold_gather_normal",
            Self::FaultingReaders => "faulting_readers",
            Self::DependentWalk => "dependent_walk",
            Self::ScanDecode => "scan_decode",
            Self::ScanNormal => "scan_normal",
            Self::HotspotMixed => "hotspot_mixed",
            Self::PressureScan => "pressure_scan",
            Self::PressureRandom => "pressure_random",
            Self::ResidentAccessOrdinary => "resident_access_ordinary",
            Self::ResidentAccessHinted => "resident_access_hinted",
            Self::ResidentAccessEpochBatch => "resident_access_epoch_batch",
            Self::ResidentAccessRetained => "resident_access_retained",
            Self::ResidentAccessPageBatch => "resident_access_page_batch",
            Self::PrefetchFragmented => "prefetch_fragmented",
            Self::PrefetchFragmentedWhole => "prefetch_fragmented_whole",
            Self::PrefetchScan => "prefetch_scan",
            Self::PrefetchPressureScan => "prefetch_pressure_scan",
            Self::AutomaticScan => "automatic_scan",
            Self::AutomaticPressureScan => "automatic_pressure_scan",
            Self::AutomaticFragmented => "automatic_fragmented",
            Self::AutomaticDependent => "automatic_dependent",
            Self::PrefetchMmapScan => "prefetch_mmap_scan",
            Self::PrefetchMmapPressureScan => "prefetch_mmap_pressure_scan",
            Self::PrefetchPollution => "prefetch_pollution",
        }
    }

    pub(super) fn shape(self) -> Self {
        match self {
            Self::PrefetchScan | Self::AutomaticScan | Self::PrefetchMmapScan => Self::ScanDecode,
            Self::PrefetchPressureScan
            | Self::AutomaticPressureScan
            | Self::PrefetchMmapPressureScan => Self::PressureScan,
            Self::AutomaticFragmented => Self::ColdPoint,
            Self::AutomaticDependent => Self::DependentWalk,
            _ => self,
        }
    }

    pub(super) fn dependent(self) -> bool {
        self.shape() == Self::DependentWalk
    }

    pub(super) fn cache(self) -> Cache {
        if self.resident_access() {
            return Cache::Resident;
        }
        match self.shape() {
            Self::PrefetchPollution | Self::ResidentProjection | Self::ResidentDecode => {
                Cache::Resident
            }
            Self::MinorProjection => Cache::Minor,
            Self::HotspotMixed => Cache::Hotspot,
            Self::PressureScan | Self::PressureRandom => Cache::Pressure,
            _ => Cache::Cold,
        }
    }

    pub(super) fn operations(self) -> u32 {
        if self.resident_access() {
            return 65_536;
        }
        match self.shape() {
            Self::ResidentProjection => 65_536,
            Self::PrefetchFragmented
            | Self::PrefetchFragmentedWhole
            | Self::ResidentDecode
            | Self::HotspotMixed => 8192,
            Self::MinorProjection | Self::FaultingReaders => 4096,
            Self::PrefetchPollution | Self::ScanDecode | Self::ScanNormal => 16_384,
            Self::PressureScan => 3 * PAGES,
            Self::PressureRandom => 2 * PAGES,
            _ => 1024,
        }
    }

    pub(super) fn workers(self) -> u32 {
        match self {
            Self::FaultingReaders | Self::PressureRandom => 4,
            _ => 1,
        }
    }

    pub(super) fn useful_bytes(self) -> u32 {
        if self.resident_access() {
            return 64;
        }
        match self.shape() {
            Self::PrefetchPollution
            | Self::ResidentProjection
            | Self::MinorProjection
            | Self::ColdPoint
            | Self::DependentWalk
            | Self::HotspotMixed => 64,
            _ => GRANULE,
        }
    }

    pub(super) fn warm_pages(self) -> u32 {
        if self.resident_access() {
            return 1024;
        }
        match self {
            Self::PrefetchPollution => 1000,
            Self::ResidentProjection | Self::HotspotMixed => 1024,
            Self::ResidentDecode | Self::MinorProjection => 4096,
            _ => 0,
        }
    }

    pub(super) fn frames(self) -> u32 {
        if matches!(
            self,
            Self::PrefetchFragmented | Self::PrefetchFragmentedWhole
        ) {
            return 256;
        }
        if self == Self::PrefetchPollution {
            return 1024;
        }
        if self.cache() == Cache::Pressure {
            16_384
        } else {
            self.warm_pages() + 1024
        }
    }

    pub(super) fn arms(self) -> [Arm; 2] {
        match self {
            Self::PrefetchFragmented => return [Arm::DiosSerial, Arm::DiosPrefetch],
            Self::PrefetchFragmentedWhole => return [Arm::DiosSerial, Arm::DiosPrefetchWhole],
            Self::PrefetchScan | Self::PrefetchPressureScan => {
                return [Arm::DiosBatch, Arm::DiosPrefetch];
            }
            Self::AutomaticScan
            | Self::AutomaticPressureScan
            | Self::AutomaticFragmented
            | Self::AutomaticDependent => return [Arm::DiosSerial, Arm::DiosAutomatic],
            Self::PrefetchMmapScan | Self::PrefetchMmapPressureScan => {
                return [Arm::MmapSequential, Arm::DiosPrefetch];
            }
            Self::PrefetchPollution => return [Arm::DiosSerial, Arm::DiosWrongHints],
            _ => {}
        }
        let mmap = match self {
            Self::ColdGatherNormal | Self::ScanNormal => Arm::MmapNormal,
            Self::ScanDecode | Self::PressureScan => Arm::MmapSequential,
            _ => Arm::MmapRandom,
        };
        let dios = match self {
            Self::ResidentAccessHinted => Arm::DiosHinted,
            Self::ResidentAccessEpochBatch => Arm::DiosEpochBatch,
            Self::ResidentAccessRetained => Arm::DiosRetained,
            Self::ResidentAccessPageBatch => Arm::DiosPageBatch,
            Self::ColdGather
            | Self::ColdGatherNormal
            | Self::ScanDecode
            | Self::ScanNormal
            | Self::HotspotMixed
            | Self::PressureScan
            | Self::PressureRandom => Arm::DiosBatch,
            _ => Arm::DiosSerial,
        };
        [mmap, dios]
    }

    pub(super) fn page(self, seed: u32, index: u32) -> PageNumber {
        assert!(index < self.operations());
        if self.resident_access() {
            let index = if self == Self::ResidentAccessPageBatch {
                index / 16
            } else {
                index
            };
            return PageNumber(permute(index, seed) % self.warm_pages());
        }
        let page = match self.shape() {
            Self::PrefetchPollution => permute(index, seed) % self.warm_pages(),
            Self::ResidentProjection | Self::ResidentDecode | Self::MinorProjection => {
                permute(index, seed) % self.warm_pages()
            }
            Self::ScanDecode | Self::ScanNormal | Self::PressureScan => index % PAGES,
            Self::DependentWalk => permute(seed + index, 0),
            Self::HotspotMixed => {
                if index.is_multiple_of(10) {
                    4096 + index / 10 * 61
                } else {
                    permute(index, seed) % 1024
                }
            }
            _ => permute(index, seed),
        };
        assert!(page < PAGES);
        PageNumber(page)
    }

    pub(super) fn resident_access(self) -> bool {
        matches!(
            self,
            Self::ResidentAccessOrdinary
                | Self::ResidentAccessHinted
                | Self::ResidentAccessEpochBatch
                | Self::ResidentAccessRetained
                | Self::ResidentAccessPageBatch
        )
    }

    pub(super) fn word_offset(self, index: u32) -> u32 {
        if self == Self::ResidentAccessPageBatch {
            index % 16 * 8
        } else {
            0
        }
    }
}

impl Arm {
    pub(super) fn parse(name: &str) -> Result<Self, String> {
        match name {
            "mmap_random" => Ok(Self::MmapRandom),
            "mmap_normal" => Ok(Self::MmapNormal),
            "mmap_sequential" => Ok(Self::MmapSequential),
            "dios_serial" => Ok(Self::DiosSerial),
            "dios_batch" => Ok(Self::DiosBatch),
            "dios_hinted" => Ok(Self::DiosHinted),
            "dios_epoch_batch" => Ok(Self::DiosEpochBatch),
            "dios_retained" => Ok(Self::DiosRetained),
            "dios_page_batch" => Ok(Self::DiosPageBatch),
            "dios_prefetch" => Ok(Self::DiosPrefetch),
            "dios_prefetch_whole" => Ok(Self::DiosPrefetchWhole),
            "dios_automatic" => Ok(Self::DiosAutomatic),
            "dios_wrong_hints" => Ok(Self::DiosWrongHints),
            _ => Err(format!("unknown arm {name}")),
        }
    }

    pub(super) fn window(self) -> usize {
        match self {
            Self::DiosBatch => WINDOW,
            _ => 1,
        }
    }

    pub(super) fn is_mmap(self) -> bool {
        matches!(
            self,
            Self::MmapRandom | Self::MmapNormal | Self::MmapSequential
        )
    }

    pub(super) fn explicit_hints(self) -> bool {
        matches!(
            self,
            Self::DiosPrefetch | Self::DiosPrefetchWhole | Self::DiosWrongHints
        )
    }
}

fn permute(index: u32, seed: u32) -> u32 {
    index
        .wrapping_mul(40_503)
        .wrapping_add(seed.wrapping_mul(7919))
        % PAGES
}

pub(super) fn word(page: PageNumber, offset: u32) -> u64 {
    assert!(page.0 <= PAGES);
    assert!(offset < GRANULE / 8);
    if offset == 0 {
        u64::from((page.0 + 40_503) % PAGES)
    } else {
        u64::from(page.0 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(offset)
    }
}

#[inline(never)]
pub(super) fn consume(bytes: &[u8]) -> u64 {
    assert!(bytes.len().is_multiple_of(8));
    assert!(!bytes.is_empty());
    bytes.chunks_exact(8).fold(0_u64, |sum, value| {
        sum.wrapping_add(u64::from_le_bytes(value.try_into().expect("eight bytes")))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn prefetch_controls_preserve_the_scan_and_dependent_request_streams() {
        use super::Lane;
        for (name, base) in [
            ("prefetch_scan", Lane::ScanDecode),
            ("automatic_scan", Lane::ScanDecode),
            ("prefetch_pressure_scan", Lane::PressureScan),
            ("automatic_pressure_scan", Lane::PressureScan),
            ("automatic_dependent", Lane::DependentWalk),
        ] {
            let lane = Lane::parse(name).expect("prefetch comparison exists");
            assert_eq!(lane.operations(), base.operations());
            assert_eq!(lane.frames(), base.frames());
            for index in 0..lane.operations() {
                assert_eq!(lane.page(17, index), base.page(17, index));
            }
        }
    }

    #[test]
    fn resident_controls_preserve_the_independent_request_stream() {
        use super::Lane;

        for name in [
            "resident_access_ordinary",
            "resident_access_hinted",
            "resident_access_epoch_batch",
            "resident_access_retained",
        ] {
            let lane = Lane::parse(name).expect("resident access control exists");
            for seed in [0, 29] {
                for index in 0..Lane::ResidentProjection.operations() {
                    assert_eq!(
                        lane.page(seed, index),
                        Lane::ResidentProjection.page(seed, index)
                    );
                }
            }
        }
    }

    #[test]
    fn dependent_walk_follows_file_words_and_cold_permutation_is_unique() {
        use super::{Lane, PAGES, PageNumber, word};

        for seed in [0, 17, 29] {
            let mut current = Lane::DependentWalk.page(seed, 0);
            for index in 0..1024 {
                assert_eq!(current, Lane::DependentWalk.page(seed, index));
                current = PageNumber(u32::try_from(word(current, 0)).expect("page"));
            }
            let mut seen = vec![false; PAGES as usize];
            for index in 0..PAGES {
                let page = Lane::PressureRandom.page(seed, index);
                assert!(!seen[page.0 as usize]);
                seen[page.0 as usize] = true;
            }
        }
    }
}
