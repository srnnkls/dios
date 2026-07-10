//! io_uring backend — a compile-only stub. The real submit/drain landing is
//! T004; this carries just enough surface for the shared `Driver` to bind and
//! for the backend-agnostic open probe (`src/open.rs`) to run on Linux.

use std::fs::File;

use crate::driver::{Attempt, Backend, Executor, OpContext, OpKind, ReadFrameIdx};

#[derive(Debug)]
pub(crate) struct Uring {
    frame_bytes: u32,
}

impl Uring {
    pub(crate) const KIND: Backend = Backend::Uring;

    pub(crate) fn new(_frames: u32, frame_bytes: u32) -> Self {
        Self { frame_bytes }
    }

    pub(crate) fn register_file(&self, _slot: u32, _file: File) {}

    pub(crate) fn copy_frame(&self, _frame: ReadFrameIdx, _out: &mut [u8]) -> usize {
        0
    }
}

impl Executor for Uring {
    fn attempt(&self, _kind: OpKind, _clean_bytes: u32, _context: OpContext<'_>) -> Attempt {
        unreachable!("the io_uring execute path lands in T004")
    }

    fn clean_bytes(&self, kind: OpKind) -> u32 {
        match kind {
            OpKind::Fsync => 0,
            OpKind::Read | OpKind::Write => self.frame_bytes,
        }
    }

    fn schedule(&self, ready_len: usize) -> usize {
        ready_len
    }

    fn retire_file(&self, _slot: u32) {}
}
