use serde::Serialize;

pub(super) const GRANULE: u32 = 4096;
pub(super) const FILE_COUNT: u32 = 8;
pub(super) const FILE_PAGES: u32 = 2048;
pub(super) const FRAMES: u32 = 256;
pub(super) const WINDOW: usize = 16;
pub(super) const COLD_START: u32 = 128;
pub(super) const COLD_SPAN: u32 = 8192;
pub(super) const POLLS_MAX: u32 = 1_000_000;
pub(super) const REPS: u32 = 30;
pub(super) const WARMUPS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Lane {
    MultirunReaders,
    PointBatch,
    PartitionPipeline,
    RetainedSession,
    CompactionInterference,
    SequentialControl,
    DependentControl,
}

impl Lane {
    pub(super) const ALL: [Self; 7] = [
        Self::MultirunReaders,
        Self::PointBatch,
        Self::PartitionPipeline,
        Self::RetainedSession,
        Self::CompactionInterference,
        Self::SequentialControl,
        Self::DependentControl,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::MultirunReaders => "multirun_readers",
            Self::PointBatch => "point_batch",
            Self::PartitionPipeline => "partition_pipeline",
            Self::RetainedSession => "retained_session",
            Self::CompactionInterference => "compaction_interference",
            Self::SequentialControl => "sequential_control",
            Self::DependentControl => "dependent_control",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|lane| lane.name() == value)
            .ok_or_else(|| format!("unknown workload {value:?}"))
    }

    pub(super) fn operations(self) -> u32 {
        match self {
            Self::MultirunReaders | Self::RetainedSession => 4096,
            Self::SequentialControl => 1024,
            _ => 128,
        }
    }

    pub(super) fn warm_pages(self) -> u32 {
        match self {
            Self::MultirunReaders | Self::PartitionPipeline | Self::CompactionInterference => 64,
            Self::RetainedSession => 16,
            _ => 0,
        }
    }

    pub(super) fn useful_bytes(self) -> u32 {
        if self == Self::RetainedSession {
            64
        } else {
            GRANULE
        }
    }

    pub(super) fn decode_passes(self) -> u32 {
        match self {
            Self::PartitionPipeline | Self::CompactionInterference => 4,
            _ => 1,
        }
    }

    pub(super) fn workers(self, arm: Arm) -> u32 {
        match (self, arm) {
            (Self::MultirunReaders, Arm::Candidate) => 4,
            _ => 1,
        }
    }

    pub(super) fn window(self, arm: Arm) -> usize {
        match self {
            Self::PointBatch | Self::PartitionPipeline | Self::SequentialControl => {
                if arm == Arm::Candidate { WINDOW } else { 1 }
            }
            Self::CompactionInterference => WINDOW,
            _ => 1,
        }
    }

    pub(super) fn page(self, round: u32, operation: u32) -> PageNumber {
        assert!(operation < self.operations());
        let cold = |index| PageNumber(COLD_START + ((round * 1024 + index) * 73) % COLD_SPAN);
        match self {
            Self::MultirunReaders => PageNumber(operation % 64),
            Self::RetainedSession => PageNumber(operation % 16),
            Self::PartitionPipeline | Self::CompactionInterference => {
                if operation.is_multiple_of(4) {
                    cold(operation / 4)
                } else {
                    PageNumber(operation % 64)
                }
            }
            Self::SequentialControl => PageNumber(COLD_START + (round % 8) * 1024 + operation),
            Self::PointBatch | Self::DependentControl => cold(operation),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Arm {
    Base,
    Candidate,
}

impl Arm {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Candidate => "candidate",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "base" => Ok(Self::Base),
            "candidate" => Ok(Self::Candidate),
            _ => Err(format!("expected base or candidate, got {value:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PageNumber(pub(super) u32);

impl PageNumber {
    pub(super) fn next(self) -> Self {
        assert!((COLD_START..COLD_START + COLD_SPAN).contains(&self.0));
        Self(COLD_START + (self.0 - COLD_START + 73) % COLD_SPAN)
    }

    pub(super) fn word(self, index: u32) -> u64 {
        if index == 0 {
            if (COLD_START..COLD_START + COLD_SPAN).contains(&self.0) {
                return u64::from(self.next().0);
            }
            return u64::from(self.0);
        }
        (u64::from(self.0) << 32).wrapping_add(u64::from(index).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
}
