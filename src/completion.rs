//! Drained completions: driver-issued token, op kind, and the op result.

use crate::driver::{OpKind, OpToken};
use crate::error::IoError;

/// One drained op result. `result` carries the byte count on success or the
/// operating failure surfaced from the backend.
#[derive(Debug)]
pub struct Completion {
    token: OpToken,
    kind: OpKind,
    result: Result<u32, IoError>,
}

impl Completion {
    pub(crate) fn new(token: OpToken, kind: OpKind, result: Result<u32, IoError>) -> Self {
        Self {
            token,
            kind,
            result,
        }
    }

    #[must_use]
    /// Returns the token issued when this operation was admitted.
    pub fn token(&self) -> OpToken {
        self.token
    }

    #[must_use]
    /// Returns the completed operation kind.
    pub fn kind(&self) -> OpKind {
        self.kind
    }

    /// The op outcome: transferred byte count, or the operating failure.
    ///
    /// # Errors
    ///
    /// Borrows the [`IoError`] the backend surfaced for this op.
    pub fn result(&self) -> Result<u32, &IoError> {
        self.result.as_ref().map(|&bytes| bytes)
    }

    pub(crate) fn into_parts(self) -> (OpToken, OpKind, Result<u32, IoError>) {
        (self.token, self.kind, self.result)
    }
}

/// A caller-owned, fixed-capacity buffer that `poll` drains completions into.
/// Cleared and refilled each poll; never grows past its construction capacity.
#[derive(Debug)]
pub struct CompletionBatch {
    items: Vec<Completion>,
}

impl CompletionBatch {
    /// Allocates a completion batch with a fixed maximum capacity.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero; every poll must be able to make bounded progress.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "completion batch capacity must be positive");
        Self {
            items: Vec::with_capacity(capacity),
        }
    }

    /// Iterates over completions drained by the latest poll.
    pub fn iter(&self) -> std::slice::Iter<'_, Completion> {
        self.items.iter()
    }

    pub(crate) fn reset(&mut self) {
        self.items.clear();
    }

    pub(crate) fn push(&mut self, completion: Completion) {
        self.items.push(completion);
    }

    pub(crate) fn capacity(&self) -> usize {
        self.items.capacity()
    }

    pub(crate) fn pop(&mut self) -> Option<Completion> {
        self.items.pop()
    }
}

impl<'batch> IntoIterator for &'batch CompletionBatch {
    type Item = &'batch Completion;
    type IntoIter = std::slice::Iter<'batch, Completion>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}
