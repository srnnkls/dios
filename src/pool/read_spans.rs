//! Bounded routes from one logical vector read to its independent page misses.

use std::num::NonZeroU64;

use crate::allocation::try_boxed_slice_with;
use crate::driver::OpToken;
use crate::driver::read_vector::{
    ReadContinuation, ReadVector, VECTOR_BYTES_MAX, VECTOR_FRAMES_MAX,
};
use crate::error::IoError;

use super::miss::{MissEntry, MissOutcome, MissSlot};
use super::{Control, InFlightFrame, PageId, Pool, PoolBackend, SHORT_READ_EOF_ERRNO};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SpanSlot(u32);

#[derive(Debug, Clone, Copy)]
struct SpanPage {
    slot: MissSlot,
    generation: NonZeroU64,
}

#[derive(Debug)]
pub(super) struct ReadSpan {
    pages: [Option<SpanPage>; VECTOR_FRAMES_MAX as usize],
    first: PageId,
    token: OpToken,
    count: u32,
    bytes: u32,
    filled: u32,
    published: u32,
}

impl ReadSpan {
    pub(super) fn new(first: PageId, token: OpToken, count: u32, granule: u32) -> Self {
        assert!((2..=VECTOR_FRAMES_MAX).contains(&count));
        let bytes = count.checked_mul(granule).expect("bounded vector bytes");
        assert!(bytes <= VECTOR_BYTES_MAX);
        Self {
            pages: [None; VECTOR_FRAMES_MAX as usize],
            first,
            token,
            count,
            bytes,
            filled: 0,
            published: 0,
        }
    }

    pub(super) fn install(&mut self, ordinal: usize, slot: MissSlot, generation: NonZeroU64) {
        assert!(ordinal < self.count as usize);
        assert!(
            self.pages[ordinal]
                .replace(SpanPage { slot, generation })
                .is_none()
        );
    }

    fn entry(&self, control: &Control, ordinal: u32, frame: &InFlightFrame) -> (usize, MissEntry) {
        assert!(ordinal < self.count);
        let page = self.pages[ordinal as usize].expect("committed span ordinal");
        let entry = control.miss.entry(page.slot.index());
        assert_eq!(
            entry.generation(),
            page.generation,
            "span retains its exact miss generation"
        );
        assert_eq!(entry.outcome(), MissOutcome::Pending);
        assert_eq!(entry.frame(), frame.frame());
        assert_eq!(entry.page().file(), self.first.file());
        assert_eq!(
            entry.page().granule_idx(),
            self.first.granule_idx() + ordinal
        );
        (page.slot.index(), entry)
    }
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "routes own fixed metadata without hot-path allocation"
)]
enum State {
    Free(Option<SpanSlot>),
    Preparing,
    InFlight(ReadSpan),
    Completing,
}

#[derive(Debug, Clone, Copy)]
struct Route {
    token: OpToken,
    slot: SpanSlot,
}

#[derive(Debug)]
pub(super) struct ReadSpans {
    slots: Box<[State]>,
    operations: Box<[Option<Route>]>,
    free: Option<SpanSlot>,
}

impl ReadSpans {
    pub(super) fn try_new(reads: u32, operations: u32) -> Option<Self> {
        assert!(operations > 0);
        let mut next = None;
        let mut index = 0;
        let slots = try_boxed_slice_with(reads, || {
            let state = State::Free(next);
            next = Some(SpanSlot(index));
            index += 1;
            state
        })?;
        Some(Self {
            slots,
            operations: try_boxed_slice_with(operations, || None)?,
            free: next,
        })
    }

    pub(super) fn reserve(&mut self) -> Option<SpanSlot> {
        let slot = self.free?;
        let State::Free(next) = self.slots[slot.0 as usize] else {
            panic!("the free list contains only vacant routes");
        };
        self.free = next;
        self.slots[slot.0 as usize] = State::Preparing;
        Some(slot)
    }

    pub(super) fn cancel(&mut self, slot: SpanSlot) {
        assert!(matches!(self.slots[slot.0 as usize], State::Preparing));
        self.slots[slot.0 as usize] = State::Free(self.free);
        self.free = Some(slot);
    }

    pub(super) fn commit(&mut self, slot: SpanSlot, span: ReadSpan) {
        assert!(matches!(self.slots[slot.0 as usize], State::Preparing));
        assert!(
            span.pages[..span.count as usize]
                .iter()
                .all(Option::is_some)
        );
        let operation = &mut self.operations[span.token.slot() as usize];
        assert!(
            operation.is_none(),
            "a vector retains its driver slot through routing"
        );
        *operation = Some(Route {
            token: span.token,
            slot,
        });
        self.slots[slot.0 as usize] = State::InFlight(span);
    }

    fn completing(&mut self, token: OpToken) -> Option<(SpanSlot, ReadSpan)> {
        let route = self.operations[token.slot() as usize]?;
        assert_eq!(route.token, token, "route generation is exact");
        let State::InFlight(span) =
            std::mem::replace(&mut self.slots[route.slot.0 as usize], State::Completing)
        else {
            panic!("only an in-flight span receives a completion");
        };
        assert_eq!(span.token, token);
        Some((route.slot, span))
    }

    fn resume(&mut self, slot: SpanSlot, span: ReadSpan) {
        assert!(matches!(self.slots[slot.0 as usize], State::Completing));
        assert!(span.filled < span.bytes);
        self.slots[slot.0 as usize] = State::InFlight(span);
    }

    fn finish(&mut self, slot: SpanSlot, token: OpToken) {
        assert!(matches!(self.slots[slot.0 as usize], State::Completing));
        let route = self.operations[token.slot() as usize]
            .take()
            .expect("live route");
        assert_eq!(route.token, token);
        assert_eq!(route.slot, slot);
        self.slots[slot.0 as usize] = State::Free(self.free);
        self.free = Some(slot);
    }

    #[cfg(feature = "bench")]
    pub(super) fn metadata_bytes(&self) -> u64 {
        u64::try_from(size_of_val(&*self.slots) + size_of_val(&*self.operations))
            .expect("route storage fits u64")
    }
}

#[expect(
    private_bounds,
    reason = "the sealed backend preserves static dispatch"
)]
impl<D: PoolBackend> Pool<D> {
    pub(super) fn route_span_completion(
        &self,
        control: &mut Control,
        token: OpToken,
        mut vector: ReadVector,
        continuation: Option<ReadContinuation>,
        result: Result<u32, IoError>,
    ) {
        let (slot, mut span) = control
            .read_spans
            .completing(token)
            .expect("a pool vector has a route");
        let bytes = result.as_ref().copied().unwrap_or(0);
        assert!(bytes <= span.bytes - span.filled);
        span.filled += bytes;
        vector.take_completed(|ordinal, write| {
            assert_eq!(
                ordinal, span.published,
                "publication follows the complete prefix"
            );
            let (index, entry) = span.entry(control, ordinal, &write);
            Self::release_read_credit(control);
            self.drain_completions_finish_success(
                &mut control.miss,
                &mut control.frame_pages,
                index,
                entry,
                write,
            );
            span.published += 1;
        });
        assert_eq!(span.published, span.filled / self.granule);
        if span.filled == span.bytes {
            assert_eq!(span.published, span.count);
            control.read_spans.finish(slot, token);
            drop(continuation);
            return;
        }
        if bytes > 0 {
            let continuation =
                continuation.expect("positive short progress retains the original slot");
            match self.driver.continue_read_vector(continuation, vector) {
                Ok(resubmitted) => {
                    assert_eq!(resubmitted, token);
                    control.read_spans.resume(slot, span);
                }
                Err((error, vector)) => {
                    self.route_span_failure(control, slot, &span, vector, &error);
                }
            }
        } else {
            let error = result
                .err()
                .unwrap_or_else(|| IoError::from_raw(SHORT_READ_EOF_ERRNO));
            self.route_span_failure(control, slot, &span, vector, &error);
            drop(continuation);
        }
    }

    fn route_span_failure(
        &self,
        control: &mut Control,
        slot: SpanSlot,
        span: &ReadSpan,
        mut vector: ReadVector,
        error: &IoError,
    ) {
        let errno = error.raw_os_error().unwrap_or(SHORT_READ_EOF_ERRNO);
        let mut failed = 0;
        vector.take_remaining(|ordinal, write| {
            assert_eq!(ordinal, span.published + failed);
            let (index, entry) = span.entry(control, ordinal, &write);
            Self::release_read_credit(control);
            self.drain_completions_finish_failure(control, index, entry, write, errno);
            failed += 1;
        });
        assert_eq!(
            span.published + failed,
            span.count,
            "each destination terminates once"
        );
        control.read_spans.finish(slot, span.token);
    }
}
