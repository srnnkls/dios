//! Portable eager backend (AD-7): `submit` enqueues, `poll` runs the real
//! pread/pwrite/fsync inline on the calling thread. Reads land in a
//! preallocated, non-moving frame slab (unregistered); barriers use darwin's
//! `F_FULLFSYNC`. The syscall runs outside the driver's submit lock — this
//! backend guards only its own file table and slab.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::driver::{
    Attempt, Backend, EagerExecutor, Executor, MAX_FILES, OpContext, OpKind, ReadFrameIdx,
};
use crate::error::IoError;

const EINTR: i32 = 4;
const EAGAIN: i32 = 35;
const EBADF: i32 = 9;
const EIO: i32 = 5;

#[derive(Debug)]
pub(crate) struct Eager {
    state: Mutex<EagerState>,
    frame_bytes: u32,
}

#[derive(Debug)]
struct EagerState {
    files: Box<[Option<File>]>,
    slab: Box<[u8]>,
}

impl Eager {
    pub(crate) const KIND: Backend = Backend::Eager;

    pub(crate) fn new(frames: u32, frame_bytes: u32, _queue_capacity: u32) -> Self {
        assert!(frames > 0, "frame count must be positive");
        assert!(frame_bytes > 0, "frame size must be positive");
        let slab_bytes = frames as usize * frame_bytes as usize;
        let mut files = Vec::with_capacity(MAX_FILES as usize);
        files.resize_with(MAX_FILES as usize, || None);
        Self {
            state: Mutex::new(EagerState {
                files: files.into_boxed_slice(),
                slab: vec![0u8; slab_bytes].into_boxed_slice(),
            }),
            frame_bytes,
        }
    }

    fn lock(&self) -> MutexGuard<'_, EagerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn copy_frame(&self, frame: ReadFrameIdx, out: &mut [u8]) -> usize {
        let state = self.lock();
        let frame_bytes = self.frame_bytes as usize;
        let frames = state.slab.len() / frame_bytes;
        assert!(
            (frame.get() as usize) < frames,
            "copy_frame index out of range for the configured frame count"
        );
        let start = frame.get() as usize * frame_bytes;
        let count = out.len().min(frame_bytes);
        assert!(
            start + count <= state.slab.len(),
            "frame region within the slab"
        );
        out[..count].copy_from_slice(&state.slab[start..start + count]);
        count
    }
}

impl EagerExecutor for Eager {
    fn attempt(&self, kind: OpKind, clean_bytes: u32, context: OpContext<'_>) -> Attempt {
        let mut guard = self.lock();
        let EagerState { files, slab } = &mut *guard;
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
                let start = context.frame.get() as usize * self.frame_bytes as usize;
                let end = start + clean_bytes as usize;
                assert!(start < slab.len(), "read frame start within the slab");
                assert!(end <= slab.len(), "read frame region within the slab");
                attempt_map_transfer(
                    file.read_at(&mut slab[start..end], context.offset),
                    clean_bytes,
                )
            }
            OpKind::Write => {
                debug_assert!(
                    !context.write_buf.is_empty(),
                    "the async poll path carries no write data until T006; only the blocking path (non-empty buffer) may reach the eager write"
                );
                let requested =
                    u32::try_from(context.write_buf.len()).expect("write length within u32 bound");
                attempt_map_transfer(file.write_at(context.write_buf, context.offset), requested)
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
