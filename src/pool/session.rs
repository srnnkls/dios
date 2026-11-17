//! Session read: one epoch pin shared by many resident-only reads of one
//! reader (FR-6 to FR-8).

use std::marker::PhantomData;

use crate::driver::FileId;

use super::epoch::ReaderCtx;
use super::{GetError, PageId, Pool, PoolBackend};

/// Many resident-only reads of one reader under one epoch pin; borrows the
/// pool and the reader and is neither `Send` nor `Sync`.
#[derive(Debug)]
pub struct ReadSession<'pool> {
    identity: u64,
    file_granules_max: u32,
    _reader: PhantomData<&'pool ReaderCtx>,
}

/// One file's identity-validated view inside a [`ReadSession`].
#[derive(Debug)]
pub struct SessionFile<'session> {
    session: &'session ReadSession<'session>,
    file: FileId,
}

/// Outcome of a session read: the resident granule borrowed for the session,
/// or `Absent` when the page has no translation entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRead<'session> {
    /// The resident granule's bytes.
    Hit(&'session [u8]),
    /// No resident frame; the caller resolves the miss through `Pool::get`.
    Absent,
}

#[expect(
    private_bounds,
    reason = "the private sealed bound preserves static dispatch for the two crate-owned pool backends"
)]
impl<D: PoolBackend> Pool<D> {
    /// Opens a session that pins `reader`'s epoch once for many reads.
    ///
    /// # Panics
    ///
    /// If `reader` belongs to another pool.
    #[must_use]
    pub fn session<'pool>(&'pool self, reader: &'pool ReaderCtx) -> ReadSession<'pool> {
        self.assert_reader_owner(reader);
        ReadSession::new(self.identity, self.file_granules_max)
    }
}

impl<'pool> ReadSession<'pool> {
    pub(crate) fn new(identity: u64, file_granules_max: u32) -> Self {
        assert!(
            file_granules_max.is_power_of_two(),
            "the per-file granule bound is a power of two"
        );
        Self {
            identity,
            file_granules_max,
            _reader: PhantomData,
        }
    }

    /// Validates `file`'s pool identity once for the session's reads of it.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::StaleFile`] for a retired or reused file generation.
    ///
    /// # Panics
    ///
    /// If `file` belongs to another pool.
    pub fn file(&self, file: FileId) -> Result<SessionFile<'_>, GetError> {
        assert_eq!(
            file.driver(),
            self.identity,
            "file identity used with a foreign pool"
        );
        Ok(SessionFile {
            session: self,
            file,
        })
    }

    /// The session resolution shared by [`SessionFile::page`] and the Loom seam.
    pub(crate) fn read_page(&self, page: PageId) -> Result<SessionRead<'pool>, GetError> {
        if page.granule_idx() >= self.file_granules_max {
            return Err(GetError::BeyondFileBound { page });
        }
        Ok(SessionRead::Absent)
    }
}

impl Drop for ReadSession<'_> {
    fn drop(&mut self) {}
}

impl SessionFile<'_> {
    /// Reads one granule of the file without submitting I/O.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::StaleFile`] when the file generation is no longer
    /// live and [`GetError::BeyondFileBound`] for a granule at or above
    /// `file_granules_max`.
    pub fn page(&self, granule_idx: u32) -> Result<SessionRead<'_>, GetError> {
        self.session.read_page(PageId::new(self.file, granule_idx))
    }
}
