use super::{PageId, Source};

#[derive(Debug, Clone, Copy)]
enum PatternState {
    Empty,
    Following {
        last: PageId,
        run: u32,
    },
    Streaming {
        demand: PageId,
        next: u32,
        width: u32,
    },
}

#[derive(Debug)]
pub(super) struct Pattern {
    state: PatternState,
    incarnation: u64,
}

impl Pattern {
    pub(super) fn new() -> Self {
        Self {
            state: PatternState::Empty,
            incarnation: 0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.state = PatternState::Empty;
        self.incarnation = self
            .incarnation
            .checked_add(1)
            .expect("reader pattern incarnation exhausted");
    }

    pub(super) fn observe(&mut self, page: PageId, capacity: u32) {
        assert!(capacity > 0);
        let last = match self.state {
            PatternState::Empty => None,
            PatternState::Following { last, .. } => Some(last),
            PatternState::Streaming { demand, .. } => Some(demand),
        };
        if last == Some(page) {
            return;
        }
        if last.is_some_and(|last| {
            last.file() == page.file()
                && last.granule_idx().checked_add(1) == Some(page.granule_idx())
        }) {
            self.observe_sequential(page, capacity);
        } else {
            self.reset();
            self.state = PatternState::Following { last: page, run: 1 };
        }
    }

    fn observe_sequential(&mut self, page: PageId, capacity: u32) {
        let Some(next) = page.granule_idx().checked_add(1) else {
            self.reset();
            return;
        };
        self.state = match self.state {
            PatternState::Following { run, .. } if run < 2 => PatternState::Following {
                last: page,
                run: run + 1,
            },
            PatternState::Streaming {
                next: issued,
                width,
                ..
            } => PatternState::Streaming {
                demand: page,
                next: issued.max(next),
                width,
            },
            PatternState::Following { .. } => PatternState::Streaming {
                demand: page,
                next,
                width: capacity.min(4),
            },
            PatternState::Empty => unreachable!("a sequential observation has a predecessor"),
        };
    }

    pub(super) fn request(&self, reader: u32) -> Option<(PageId, Source)> {
        let PatternState::Streaming {
            demand,
            next,
            width,
        } = self.state
        else {
            return None;
        };
        if next > demand.granule_idx().saturating_add(width) {
            return None;
        }
        Some((
            PageId::new(demand.file(), next),
            Source::Automatic {
                reader,
                incarnation: self.incarnation,
            },
        ))
    }

    pub(super) fn window(&self, reader: u32, vector_width: u32) -> Option<(PageId, Source, u32)> {
        let (page, source) = self.request(reader)?;
        let PatternState::Streaming {
            demand,
            next,
            width,
        } = self.state
        else {
            unreachable!("stream request");
        };
        let count = width.min(vector_width);
        let eligible = demand.granule_idx().saturating_add(width) - next + 1;
        if eligible < count {
            return None;
        }
        Some((page, source, count))
    }

    pub(super) fn incarnation(&self) -> u64 {
        self.incarnation
    }

    #[cfg(feature = "bench")]
    pub(super) fn width(&self) -> u32 {
        if let PatternState::Streaming { width, .. } = self.state {
            width
        } else {
            0
        }
    }

    pub(super) fn issued(&mut self, page: PageId) {
        let PatternState::Streaming {
            demand,
            next,
            width,
        } = self.state
        else {
            unreachable!("only an active stream issues predictions");
        };
        assert_eq!(page, PageId::new(demand.file(), next));
        if let Some(next) = next.checked_add(1) {
            self.state = PatternState::Streaming {
                demand,
                next,
                width,
            };
        } else {
            self.reset();
        }
    }

    pub(super) fn promoted(&mut self, page: PageId, incarnation: u64, capacity: u32) {
        if self.incarnation != incarnation {
            return;
        }
        let PatternState::Streaming {
            demand,
            next,
            width,
        } = self.state
        else {
            return;
        };
        assert_eq!(demand.file(), page.file());
        if page.granule_idx() <= demand.granule_idx() {
            return;
        }
        if demand.granule_idx().checked_add(1) == Some(page.granule_idx()) {
            self.state = PatternState::Streaming {
                demand: page,
                next,
                width: width.saturating_mul(2).min(capacity),
            };
        } else {
            self.observe(page, capacity);
        }
    }

    pub(super) fn owns(&self, incarnation: u64) -> bool {
        self.incarnation == incarnation
    }
}
