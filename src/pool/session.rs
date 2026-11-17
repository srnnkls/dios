//! Session read: one epoch pin shared by many resident-only reads of one
//! reader (FR-6 to FR-8).

use crate::driver::FileId;
use crate::sync::{AtomicU32, AtomicU64};

#[cfg(loom)]
use crate::sync::Ordering;

#[cfg(loom)]
use super::epoch::ReaderSlot;
use super::epoch::{ReaderCtx, SessionPin};
#[cfg(loom)]
use super::frames::Frames;
use super::frames::SessionArena;
use super::translation::{Translation, entry_frame};
use super::{Clock, GetError, PageId, Pool, PoolBackend, file_live_word, live_word_is_live};

/// Many resident-only reads of one reader under one epoch pin; borrows the
/// pool and the reader and is neither `Send` nor `Sync`.
#[derive(Debug)]
pub struct ReadSession<'pool> {
    pin: SessionPin<'pool>,
    translation: &'pool Translation,
    arena: SessionArena<'pool>,
    clock: &'pool Clock,
    file_live_generations: &'pool [AtomicU64],
    identity: u64,
}

/// One file's identity-validated view inside a [`ReadSession`], holding the
/// values every read of the file reuses.
#[derive(Debug)]
pub struct SessionFile<'session> {
    pin: &'session SessionPin<'session>,
    entries: &'session [AtomicU32],
    live_generation: &'session AtomicU64,
    live_word: u64,
    arena: SessionArena<'session>,
    clock: &'session Clock,
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
    /// If `reader` belongs to another pool or already holds its declared peak
    /// of guards.
    #[must_use]
    pub fn session<'pool>(&'pool self, reader: &'pool ReaderCtx) -> ReadSession<'pool> {
        self.assert_reader_owner(reader);
        let slot = reader.slot();
        ReadSession {
            pin: slot.commit_session(self.pin_begin(slot)),
            translation: &self.table,
            arena: self.frames.session_arena(),
            clock: &self.clock,
            file_live_generations: &self.file_live_generations,
            identity: self.identity,
        }
    }
}

#[cfg(loom)]
impl<'pool> ReadSession<'pool> {
    /// A session over the parts of a pool, pinning `slot`'s epoch from
    /// `global_epoch`.
    pub(crate) fn new(
        slot: &'pool ReaderSlot,
        global_epoch: &AtomicU64,
        translation: &'pool Translation,
        frames: &'pool Frames,
        clock: &'pool Clock,
        file_live_generations: &'pool [AtomicU64],
        identity: u64,
    ) -> Self {
        Self {
            pin: slot.commit_session(slot.begin_pin(global_epoch.load(Ordering::Acquire))),
            translation,
            arena: frames.session_arena(),
            clock,
            file_live_generations,
            identity,
        }
    }
}

impl ReadSession<'_> {
    /// Validates `file`'s pool identity and liveness once for the session's
    /// reads of it.
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
        let entries = self.translation.slot_entries(file);
        let live_generation = &self.file_live_generations[file.slot() as usize];
        let live_word = file_live_word(file);
        if !live_word_is_live(live_generation, live_word) {
            return Err(GetError::StaleFile {
                page: PageId::new(file, 0),
            });
        }
        Ok(SessionFile {
            pin: &self.pin,
            entries,
            live_generation,
            live_word,
            arena: self.arena,
            clock: self.clock,
            file,
        })
    }

    /// The session resolution of one page, shared with the Loom seam.
    #[cfg(loom)]
    pub(crate) fn read_page(&self, page: PageId) -> Result<SessionRead<'_>, GetError> {
        self.file(page.file())?.resolve(page.granule_idx())
    }
}

impl<'session> SessionFile<'session> {
    /// Reads one granule of the file without submitting I/O.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::StaleFile`] when the file generation is no longer
    /// live and [`GetError::BeyondFileBound`] for a granule at or above
    /// `file_granules_max`.
    #[inline]
    pub fn page(&self, granule_idx: u32) -> Result<SessionRead<'_>, GetError> {
        self.resolve(granule_idx)
    }

    /// Loads the entry, then the file-generation mirror (INV-7), then sets
    /// the CLOCK reference of the frame it borrows.
    #[inline]
    fn resolve(&self, granule_idx: u32) -> Result<SessionRead<'session>, GetError> {
        let Some(entry) = self.entries.get(granule_idx as usize) else {
            return Err(GetError::BeyondFileBound {
                page: PageId::new(self.file, granule_idx),
            });
        };
        let frame = entry_frame(entry);
        if !live_word_is_live(self.live_generation, self.live_word) {
            return Err(GetError::StaleFile {
                page: PageId::new(self.file, granule_idx),
            });
        }
        let Some(frame) = frame else {
            return Ok(SessionRead::Absent);
        };
        let _ = self.clock.reference_from(frame, self.pin.slot());
        Ok(SessionRead::Hit(self.arena.frame_bytes(self.pin, frame)))
    }
}
