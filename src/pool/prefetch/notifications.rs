//! Fixed-height dirty-frame notification with one atomic root on an idle poll.

use crate::allocation::try_boxed_slice_with;
use crate::pool::ReadFrameIdx;
use crate::sync::{AtomicU64, Ordering};

const LEVELS_MAX: usize = 6;

#[derive(Debug)]
pub(in crate::pool) struct DirtyFrames {
    levels: [Option<Box<[AtomicU64]>>; LEVELS_MAX],
    height: usize,
    frames: u32,
}

impl DirtyFrames {
    pub(in crate::pool) fn try_new(frames: u32) -> Option<Self> {
        assert!(frames > 0);
        let mut levels = std::array::from_fn(|_| None);
        let mut count = frames;
        let mut height = 0;
        for level in &mut levels {
            count = count.div_ceil(64);
            *level = Some(try_boxed_slice_with(count, || AtomicU64::new(0))?);
            height += 1;
            if count == 1 {
                break;
            }
        }
        assert_eq!(count, 1);
        Some(Self {
            levels,
            height,
            frames,
        })
    }

    pub(in crate::pool) fn notify(&self, frame: ReadFrameIdx) {
        assert!(frame.get() < self.frames);
        let mut index = frame.get();
        for level in &self.levels[..self.height] {
            let word = index / 64;
            level.as_ref().expect("initialized level")[word as usize]
                .fetch_or(1_u64 << (index % 64), Ordering::Release);
            // Every ancestor is republished: a concurrent drain may have taken
            // an ancestor while leaving this child's previously set bit intact.
            index = word;
        }
        assert_eq!(index, 0);
    }

    pub(in crate::pool) fn drain(&self, mut visit: impl FnMut(ReadFrameIdx)) -> u32 {
        let mut level = self.height - 1;
        let mut words = [0_u32; LEVELS_MAX];
        let mut bits = [0_u64; LEVELS_MAX];
        bits[level] = self.levels[level].as_ref().expect("root")[0].swap(0, Ordering::Acquire);
        let mut visited = 0;
        let bound = u64::from(self.frames) * (LEVELS_MAX as u64 + 1) + LEVELS_MAX as u64;
        for _ in 0..bound {
            if bits[level] == 0 {
                if level == self.height - 1 {
                    return visited;
                }
                level += 1;
                continue;
            }
            let bit = bits[level].trailing_zeros();
            bits[level] &= bits[level] - 1;
            let index = words[level] * 64 + bit;
            if level == 0 {
                assert!(index < self.frames);
                visit(ReadFrameIdx::new(index));
                visited += 1;
            } else {
                level -= 1;
                words[level] = index;
                bits[level] = self.levels[level].as_ref().expect("child level")[index as usize]
                    .swap(0, Ordering::Acquire);
            }
        }
        unreachable!("each bounded bitmap node is visited at most once");
    }

    #[cfg(feature = "bench")]
    pub(in crate::pool) fn metadata_bytes(&self) -> u64 {
        self.levels[..self.height]
            .iter()
            .map(|level| {
                (level.as_ref().expect("initialized level").len() * size_of::<AtomicU64>()) as u64
            })
            .sum()
    }
}
