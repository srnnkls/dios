//! Portable eager backend (AD-7): `submit` enqueues, `poll` runs the real
//! pread/pwrite/fsync inline on the calling thread. Reads land in a
//! preallocated, non-moving frame slab (unregistered); barriers use darwin's
//! `F_FULLFSYNC`. The syscall runs outside the driver's submit lock — this
//! backend guards only its own file table and slab.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::driver::{
    Attempt, Backend, DriverBuildError, EagerExecutor, Executor, OpContext, OpKind,
};
use crate::error::IoError;
use crate::pool::Frames;
use crate::pool::write_arena::ArenaState;

const EINTR: i32 = 4;
const EAGAIN: i32 = 35;
const EBADF: i32 = 9;
const EIO: i32 = 5;

#[derive(Debug)]
pub(crate) struct Eager {
    state: Mutex<EagerState>,
    frames: Arc<Frames>,
    _write_arena: Arc<ArenaState>,
    frame_bytes: u32,
}

#[derive(Debug)]
struct EagerState {
    files: Box<[Option<File>]>,
}

impl Eager {
    pub(crate) const KIND: Backend = Backend::Eager;

    pub(crate) fn new(
        frames: Arc<Frames>,
        write_arena: Arc<ArenaState>,
        _queue_capacity: u32,
        file_capacity: u32,
    ) -> Result<Self, DriverBuildError> {
        assert!(frames.count() > 0, "frame count must be positive");
        let frame_bytes = frames.granule();
        assert!(frame_bytes > 0, "frame size must be positive");
        let files = crate::allocation::try_boxed_slice_with(file_capacity, || None)
            .ok_or(DriverBuildError::Allocation)?;
        Ok(Self {
            state: Mutex::new(EagerState { files }),
            frames,
            _write_arena: write_arena,
            frame_bytes,
        })
    }

    fn lock(&self) -> MutexGuard<'_, EagerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EagerExecutor for Eager {
    fn attempt(&self, kind: OpKind, clean_bytes: u32, context: OpContext<'_>) -> Attempt {
        let mut guard = self.lock();
        let EagerState { files } = &mut *guard;
        let slot = context.fd.slot() as usize;
        let Some(file) = files.get(slot).and_then(Option::as_ref) else {
            return Attempt::Failed(EBADF);
        };
        match kind {
            OpKind::Read => {
                debug_assert!(
                    clean_bytes <= self.frame_bytes,
                    "a read transfer spans at most one frame"
                );
                assert_eq!(
                    clean_bytes, context.requested_len,
                    "the eager attempt receives the admitted transfer length"
                );
                let result = self.frames.with_transfer_range_mut(
                    context.frame,
                    context.destination_offset,
                    context.requested_len,
                    |destination| file.read_at(destination, context.file_offset),
                );
                attempt_map_transfer(result, context.requested_len)
            }
            OpKind::Write => {
                debug_assert!(
                    !context.write_buf.is_empty(),
                    "the async poll path carries no write data until T006; only the blocking path (non-empty buffer) may reach the eager write"
                );
                let requested =
                    u32::try_from(context.write_buf.len()).expect("write length within u32 bound");
                attempt_map_transfer(
                    file.write_at(context.write_buf, context.file_offset),
                    requested,
                )
            }
            OpKind::Fsync => match crate::open::full_fsync(file) {
                Ok(()) => Attempt::Done(0),
                Err(error) => classify(&error),
            },
        }
    }
}

impl Executor for Eager {
    fn register_file(&self, slot: u32, file: File) -> Result<(), IoError> {
        let mut state = self.lock();
        assert!(
            (slot as usize) < state.files.len(),
            "fd slot within the file table"
        );
        assert!(
            state.files[slot as usize].is_none(),
            "fd slot reused before its prior file was retired"
        );
        state.files[slot as usize] = Some(file);
        Ok(())
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

    fn retire_file(&self, slot: u32) {
        let mut state = self.lock();
        assert!(
            (slot as usize) < state.files.len(),
            "retire slot within the file table"
        );
        let file = state.files[slot as usize].take();
        debug_assert!(file.is_some(), "retire of a slot that holds no live file");
        drop(file);
    }

    #[cfg(any(feature = "mock", feature = "bench"))]
    fn copy_frame(&self, frame: crate::pool::ReadFrameIdx, out: &mut [u8]) -> usize {
        self.frames.copy_frame(frame, out)
    }
}

fn attempt_map_transfer(result: std::io::Result<usize>, requested: u32) -> Attempt {
    match result {
        Ok(bytes) => {
            assert!(
                bytes <= requested as usize,
                "syscall reported {bytes} bytes, more than the {requested} requested"
            );
            Attempt::Done(u32::try_from(bytes).expect("transfer count within the requested bound"))
        }
        Err(error) => classify(&error),
    }
}

fn classify(error: &std::io::Error) -> Attempt {
    match error.raw_os_error() {
        Some(EINTR) => Attempt::Interrupted,
        Some(EAGAIN) => Attempt::WouldBlock,
        Some(code) => Attempt::Failed(code),
        None => Attempt::Failed(EIO),
    }
}
