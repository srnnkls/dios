//! Owned scatter destinations and completion-slot continuations.

use std::alloc::Layout;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use super::{
    DriverCore, Executor, FileHandle, FileId, FrameLease, IoMode, OpEntry, OpKind, OpState,
    OpToken, Shared, SyncMode,
};
use crate::allocation::MappedArena;
use crate::error::{IoError, SubmitError};
use crate::pool::{Frames, InFlightFrame, ReadFrameIdx};

pub(crate) const VECTOR_FRAMES_MAX: u32 = 32;
pub(crate) const VECTOR_BYTES_MAX: u32 = 128 * 1024;

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the bounded vector must move its owned tokens without a hot-path allocation"
)]
pub(crate) enum ReadDestination {
    Point(InFlightFrame),
    Vector(ReadVector),
}

impl ReadDestination {
    pub(crate) fn frames_in_bounds(&self, count: u32) -> bool {
        match self {
            Self::Point(frame) => frame.frame().get() < count,
            Self::Vector(vector) => vector.frames[..vector.count as usize]
                .iter()
                .all(|frame| frame.as_ref().expect("live prefix").frame().get() < count),
        }
    }

    pub(crate) fn into_point(self) -> InFlightFrame {
        match self {
            Self::Point(frame) => frame,
            Self::Vector(_) => panic!("point completion contains a vector destination"),
        }
    }
}

/// Each occupied position owns one token; prefix extraction also preserves its
/// original ordinal so publication never reconstructs ownership from an index.
#[derive(Debug)]
pub(crate) struct ReadVector {
    frames: [Option<InFlightFrame>; VECTOR_FRAMES_MAX as usize],
    count: u32,
    ordinal: u32,
    filled: u32,
    total: u32,
    arena: Arc<Frames>,
    descriptors: Option<VectorIo>,
    operation: Option<OpToken>,
}

impl ReadVector {
    pub(crate) fn new(arena: &Arc<Frames>) -> Self {
        Self {
            frames: [const { None }; VECTOR_FRAMES_MAX as usize],
            count: 0,
            ordinal: 0,
            filled: 0,
            total: 0,
            arena: Arc::clone(arena),
            descriptors: None,
            operation: None,
        }
    }

    pub(crate) fn push(&mut self, frame: InFlightFrame) {
        assert!(self.count < VECTOR_FRAMES_MAX, "vector frame limit");
        assert_eq!(self.filled, 0, "only an unsubmitted bundle can grow");
        assert!(
            self.operation.is_none(),
            "admitted bundles cannot gain destinations"
        );
        assert!(
            self.frames[..self.count as usize].iter().all(|existing| {
                existing.as_ref().expect("live prefix").frame() != frame.frame()
            }),
            "vector destinations are distinct"
        );
        let granule = self.arena.granule();
        assert!(
            granule <= VECTOR_BYTES_MAX - self.total,
            "vector byte limit"
        );
        let _ = self.arena.transfer_ptr(&frame, 0, granule);
        self.frames[self.count as usize] = Some(frame);
        self.count += 1;
        self.total += granule;
    }

    pub(super) fn bind(&mut self, operation: OpToken) {
        assert!(
            self.operation.replace(operation).is_none(),
            "bind one logical operation"
        );
    }

    pub(crate) fn remaining(&self) -> u32 {
        self.total - self.filled
    }

    pub(crate) fn destination_offset(&self) -> u32 {
        self.filled % self.arena.granule()
    }

    pub(crate) fn record_completion(&mut self, bytes: u32) {
        assert!(
            bytes <= self.remaining(),
            "vector completion stays within its tail"
        );
        self.filled += bytes;
        self.descriptors = None;
    }

    pub(crate) fn take_completed(&mut self, mut consume: impl FnMut(u32, InFlightFrame)) {
        let completed = self.filled / self.arena.granule() - self.ordinal;
        assert!(completed <= self.count, "published prefix is owned");
        for index in 0..completed {
            let frame = self.frames[index as usize].take().expect("live prefix");
            consume(self.ordinal + index, frame);
        }
        self.frames.rotate_left(completed as usize);
        self.ordinal += completed;
        self.count -= completed;
        assert!(
            self.frames[self.count as usize..]
                .iter()
                .all(Option::is_none)
        );
    }

    pub(crate) fn take_remaining(&mut self, mut consume: impl FnMut(u32, InFlightFrame)) {
        for index in 0..self.count {
            let frame = self.frames[index as usize].take().expect("live suffix");
            consume(self.ordinal + index, frame);
        }
        self.ordinal += self.count;
        self.count = 0;
        assert!(self.frames.iter().all(Option::is_none));
    }

    #[cfg(feature = "bench")]
    pub(crate) fn visit_frames(&self, mut visit: impl FnMut(u32, ReadFrameIdx)) {
        for index in 0..self.count {
            visit(
                self.ordinal + index,
                self.frames[index as usize]
                    .as_ref()
                    .expect("live prefix")
                    .frame(),
            );
        }
    }

    #[cfg(feature = "bench")]
    pub(crate) fn frame_count(&self) -> u32 {
        self.count
    }

    pub(crate) fn io(&self) -> VectorIo {
        self.descriptors
            .expect("admission prepared stable descriptors")
    }

    pub(crate) fn transfer_prefix(&mut self, bytes: u32, fill: impl FnMut(u32, &mut [u8])) {
        assert!(
            bytes <= self.remaining(),
            "transfer fits the retained vector"
        );
        // SAFETY: this bundle owns all descriptor targets, exclusively borrowed
        // for this synchronous transfer; admission keeps their iovecs live.
        unsafe { self.io().transfer_prefix(bytes, fill) };
    }
}

impl Drop for ReadVector {
    fn drop(&mut self) {
        for frame in &mut self.frames[..self.count as usize] {
            if let Some(frame) = frame.take() {
                self.arena.abort(frame);
            }
        }
    }
}

/// C iovec layout, shared by ordinary READV and the retained-pointer mocks.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Iovec {
    base: *mut c_void,
    len: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VectorIo {
    pointer: *const Iovec,
    count: u32,
    bytes: u32,
}

// SAFETY: the driver serializes descriptor preparation per occupied slot and
// retains both allocations until the corresponding syscall/CQE completes.
unsafe impl Send for VectorIo {}
// SAFETY: descriptors are immutable during execution; mutable payload access
// separately requires the unique bundle or the backend's outstanding-I/O lease.
unsafe impl Sync for VectorIo {}

impl VectorIo {
    #[cfg(target_os = "linux")]
    pub(crate) fn pointer(self) -> *const Iovec {
        self.pointer
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn count(self) -> u32 {
        self.count
    }

    /// # Safety
    /// The op retains its descriptor allocation and every destination token;
    /// no other transfer or reader may access the destinations until return.
    pub(crate) unsafe fn transfer_prefix(self, bytes: u32, mut fill: impl FnMut(u32, &mut [u8])) {
        assert!(
            bytes <= self.bytes,
            "the backend fills only its reported prefix"
        );
        let mut transferred = 0;
        for index in 0..self.count {
            if transferred == bytes {
                break;
            }
            // SAFETY: the caller retains all `count` initialized descriptors.
            let descriptor = unsafe { self.pointer.wrapping_add(index as usize).read() };
            let length = (bytes - transferred)
                .min(u32::try_from(descriptor.len).expect("bounded descriptor length"));
            // SAFETY: the caller retains the unique write token for this exact
            // range, and `length` does not exceed this descriptor's allocation.
            let destination = unsafe {
                std::slice::from_raw_parts_mut(descriptor.base.cast::<u8>(), length as usize)
            };
            fill(transferred, destination);
            transferred += length;
        }
        assert_eq!(transferred, bytes, "every reported byte has a destination");
    }
}

/// Descriptor memory is independent of the mutable completion slab. All
/// addresses derive from its allocation-wide base, never a mutable slab slice.
#[derive(Debug)]
pub(crate) struct VectorStorage {
    allocation: MappedArena,
    slots: u32,
}

// SAFETY: only the submit lock writes a free/just-completed slot's descriptors;
// no reference covers another slot, and outstanding slots remain immutable.
unsafe impl Send for VectorStorage {}
// SAFETY: the same per-slot lifetime protocol excludes concurrent writes.
unsafe impl Sync for VectorStorage {}

impl VectorStorage {
    #[cfg(feature = "bench")]
    pub(crate) fn metadata_bytes(&self) -> u64 {
        u64::try_from(self.allocation.mapped_len()).expect("allocated descriptor bytes fit u64")
    }
    pub(crate) fn try_new(slots: u32) -> Option<Self> {
        let count = (slots as usize).checked_mul(VECTOR_FRAMES_MAX as usize)?;
        let layout = Layout::array::<Iovec>(count).ok()?;
        Some(Self {
            allocation: MappedArena::try_map(layout.size(), layout.align())?,
            slots,
        })
    }

    pub(crate) fn prepare(&self, slot: u32, vector: &mut ReadVector) {
        assert!(slot < self.slots, "descriptor slot belongs to this slab");
        assert!(vector.count > 0, "a vector has an unpublished destination");
        assert_eq!(
            vector.ordinal,
            vector.filled / vector.arena.granule(),
            "publish complete frames before rebuilding a suffix"
        );
        let base = self.allocation.base().cast::<Iovec>().as_ptr();
        let start = slot as usize * VECTOR_FRAMES_MAX as usize;
        // SAFETY: each slot owns VECTOR_FRAMES_MAX elements of this allocation.
        let pointer = unsafe { base.add(start) };
        for index in 0..vector.count {
            let offset = if index == 0 {
                vector.destination_offset()
            } else {
                0
            };
            let len = vector.arena.granule() - offset;
            let frame = vector.frames[index as usize].as_ref().expect("live prefix");
            let destination = vector.arena.transfer_ptr(frame, offset, len);
            let descriptor = Iovec {
                base: destination.cast(),
                len: len as usize,
            };
            // SAFETY: the slot is reserved and has no outstanding attempt;
            // writes touch only its own bounded descriptor subrange.
            unsafe { pointer.wrapping_add(index as usize).write(descriptor) };
        }
        vector.descriptors = Some(VectorIo {
            pointer,
            count: vector.count,
            bytes: vector.remaining(),
        });
    }
}

/// No kernel access is outstanding while this exclusive lease is outside the
/// slab. A weak reference permits a completion to outlive driver teardown.
#[derive(Debug)]
pub(crate) struct ReadContinuation {
    shared: Weak<Mutex<Shared>>,
    token: Option<OpToken>,
}

impl ReadContinuation {
    pub(super) fn new(shared: &Arc<Mutex<Shared>>, token: OpToken) -> Self {
        Self {
            shared: Arc::downgrade(shared),
            token: Some(token),
        }
    }
}

impl Drop for ReadContinuation {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.slab.contains(token) {
            assert!(matches!(
                shared.slab.peek(token.slot()).state,
                OpState::Continuation | OpState::Terminal(_)
            ));
            shared.release_read_lease(token.slot());
        }
    }
}

#[derive(Debug)]
pub(super) struct DeferredCompletion {
    pub(super) fd: FileId,
    pub(super) retire: bool,
    outcome: Result<u32, i32>,
}

impl Shared {
    fn release_read_lease(&mut self, slot: u32) {
        let (_, entry) = self.slab.reclaim(slot);
        let outcome = match entry.state {
            OpState::Continuation => Err(super::EIO),
            OpState::Terminal(outcome) => outcome,
            _ => panic!("only a held read lease releases its original slot"),
        };
        assert!(
            entry.frame.is_none(),
            "the completion owns the destinations"
        );
        let retire = self.files.on_complete(entry.fd);
        assert!(
            self.deferred.len() < self.deferred.capacity(),
            "bounded logical completions"
        );
        self.deferred.push_back(DeferredCompletion {
            fd: entry.fd,
            retire,
            outcome,
        });
    }
}

impl<E: Executor> DriverCore<E> {
    pub(crate) fn submit_raw_read_vector(
        &self,
        fd: &FileHandle,
        frames: &[ReadFrameIdx],
        offset: u64,
    ) -> Result<OpToken, SubmitError> {
        self.flush_deferred();
        assert!((2..=VECTOR_FRAMES_MAX as usize).contains(&frames.len()));
        assert!(
            frames.len() <= (VECTOR_BYTES_MAX / self.arena.granule()) as usize,
            "vector fits its byte limit"
        );
        let mut vector = ReadVector::new(&self.arena);
        for &frame in frames {
            vector.push(
                self.arena
                    .claim_unidentified(frame)
                    .expect("a raw vector destination has one owner"),
            );
        }
        self.submit_read_vector(fd, vector, offset, FrameLease::Raw)
            .map_err(|(error, _vector)| error)
    }

    #[expect(
        clippy::result_large_err,
        reason = "refusal returns the entire unique frame bundle without allocating"
    )]
    pub(crate) fn submit_read_vector(
        &self,
        fd: &FileHandle,
        mut vector: ReadVector,
        offset: u64,
        frame_lease: FrameLease,
    ) -> Result<OpToken, (SubmitError, ReadVector)> {
        assert!(
            Arc::ptr_eq(&vector.arena, &self.arena),
            "vector belongs to this driver"
        );
        assert_eq!(vector.filled, 0, "new admission starts at byte zero");
        assert!((2..=VECTOR_FRAMES_MAX).contains(&vector.count));
        let requested_len = vector.remaining();
        assert!(
            offset.checked_add(u64::from(requested_len)).is_some(),
            "file range fits u64"
        );
        if let IoMode::Direct(alignment) = fd.io_mode() {
            assert!(
                alignment.check(offset).is_ok(),
                "aligned vector file offset"
            );
            assert!(alignment.check(u64::from(self.arena.granule())).is_ok());
        }
        let mut shared = self.lock();
        let slot = match self.admit(&mut shared, fd.file_id()) {
            Ok(slot) => slot,
            Err(error) => return Err((error, vector)),
        };
        self.vectors.prepare(slot, &mut vector);
        let entry = OpEntry {
            #[cfg(feature = "bench")]
            read_purpose: super::ReadPurpose::Speculative,
            kind: OpKind::Read,
            sync_mode: SyncMode::Full,
            fd: fd.file_id(),
            file_offset: offset,
            frame: Some(ReadDestination::Vector(vector)),
            destination_offset: 0,
            requested_len,
            frame_lease,
            write_slot: None,
            retries: 0,
            io_mode: fd.io_mode(),
            state: OpState::Queued,
        };
        Ok(self.commit(&mut shared, slot, entry))
    }

    #[expect(
        clippy::result_large_err,
        reason = "a refused continuation returns every unpublished token without allocating"
    )]
    pub(crate) fn continue_read_vector(
        &self,
        mut lease: ReadContinuation,
        mut vector: ReadVector,
    ) -> Result<OpToken, (IoError, ReadVector)> {
        assert!(Weak::ptr_eq(&lease.shared, &Arc::downgrade(&self.inner)));
        assert!(Arc::ptr_eq(&vector.arena, &self.arena));
        let token = lease.token.expect("continuation is consumed once");
        assert_eq!(
            vector.operation,
            Some(token),
            "suffix belongs to its exclusive lease"
        );
        let mut shared = self.lock();
        assert!(
            shared.slab.contains(token),
            "continuation owns the original generation"
        );
        let entry = shared.slab.peek_mut(token.slot());
        assert_eq!(entry.state, OpState::Continuation);
        assert_eq!(entry.requested_len, vector.remaining());
        if let IoMode::Direct(alignment) = entry.io_mode {
            let aligned = [
                entry.file_offset,
                u64::from(vector.destination_offset()),
                u64::from(vector.remaining()),
            ]
            .into_iter()
            .all(|value| alignment.check(value).is_ok());
            if !aligned {
                drop(shared);
                return Err((IoError::from_raw(22), vector));
            }
        }
        self.vectors.prepare(token.slot(), &mut vector);
        entry.frame = Some(ReadDestination::Vector(vector));
        entry.state = OpState::Queued;
        entry.retries = 0;
        assert!(
            shared.ready.len() < self.queue_capacity as usize,
            "continuation queue is bounded"
        );
        shared.ready.push_back(token.slot());
        lease.token = None;
        Ok(token)
    }

    pub(super) fn flush_deferred(&self) {
        for _ in 0..self.queue_capacity {
            let Some(completion) = self.lock().deferred.pop_front() else {
                break;
            };
            self.executor.on_op_completed(
                completion.fd,
                OpKind::Read,
                &completion.outcome.map_err(IoError::from_raw),
            );
            self.executor.on_op_finalized();
            if completion.retire {
                self.retire(completion.fd);
            }
        }
    }

    pub(super) fn quiesce_continue_vectors(&self, batch: &mut super::CompletionBatch) {
        for _ in 0..batch.capacity() {
            let Some(completion) = batch.pop() else { break };
            let (_, _, result, destination, continuation) = completion.into_parts();
            match destination {
                Some(ReadDestination::Vector(mut vector)) => {
                    vector.take_completed(|_, frame| {
                        self.arena.abort(frame);
                    });
                    if vector.remaining() == 0 {
                        continue;
                    }
                    if !result.is_ok_and(|bytes| bytes > 0) {
                        continue;
                    }
                    let Some(continuation) = continuation else {
                        continue;
                    };
                    if let Err((_error, vector)) = self.continue_read_vector(continuation, vector) {
                        drop(vector);
                    }
                }
                Some(ReadDestination::Point(frame)) => {
                    self.arena.abort(frame);
                }
                None => assert!(continuation.is_none()),
            }
        }
    }

    pub(super) fn abandon_held_continuations(&self) {
        let mut shared = self.lock();
        for slot in 0..self.queue_capacity {
            if shared.slab.slots[slot as usize]
                .payload
                .as_ref()
                .is_some_and(|entry| {
                    matches!(entry.state, OpState::Continuation | OpState::Terminal(_))
                })
            {
                shared.release_read_lease(slot);
            }
        }
        drop(shared);
        self.flush_deferred();
    }
}
