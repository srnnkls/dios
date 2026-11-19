//! Session read: one epoch pin shared by many resident-only reads of one
//! reader (FR-6 to FR-8), with file liveness validated once per view.

use crate::driver::FileId;
use crate::sync::{AtomicU32, AtomicU64};

#[cfg(loom)]
use crate::sync::Ordering;

use super::epoch::{ReaderCtx, ReaderSlot, SessionPin};
#[cfg(loom)]
use super::frames::Frames;
use super::frames::SessionArena;
use super::translation::{Translation, entry_frame};
use super::{Clock, GetError, PageId, Pool, PoolBackend, file_generation_is_live};

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
    view: FileView<'session>,
}

/// A one-file session opened by [`Pool::session_file`]: its own epoch pin and
/// the validated view of one file; borrows the pool and the reader and is
/// neither `Send` nor `Sync`.
#[derive(Debug)]
pub struct FileSession<'pool> {
    pin: SessionPin<'pool>,
    view: FileView<'pool>,
}

/// The loop-invariant values of one file's reads under a session pin, taken
/// after the pin validated the file's liveness.
#[derive(Debug, Clone, Copy)]
struct FileView<'pool> {
    reader: &'pool ReaderSlot,
    entries: &'pool [AtomicU32],
    arena: SessionArena<'pool>,
    clock: &'pool Clock,
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
    #[inline]
    pub fn session<'pool>(&'pool self, reader: &'pool ReaderCtx) -> ReadSession<'pool> {
        self.assert_reader_owner(reader);
        let slot = reader.slot();
        ReadSession {
            pin: slot.commit_session(self.pin_begin(slot)),
            translation: &self.table,
            arena: self
                .frames
                .session_arena(self.clock.session_reference_bits()),
            clock: &self.clock,
            file_live_generations: &self.file_live_generations,
            identity: self.identity,
        }
    }

    /// Opens a one-file session in one call: pins `reader`'s epoch as one
    /// guard, then validates `file`'s pool identity and liveness once, as
    /// [`Pool::session`] followed by [`ReadSession::file`]. The pin is
    /// released when the [`FileSession`] drops or the open fails.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::StaleFile`] when [`Pool::retire_file`] was called
    /// for `file` before this call, or when its slot holds another file.
    ///
    /// # Panics
    ///
    /// If `reader` or `file` belongs to another pool, or `reader` already
    /// holds its declared peak of guards.
    #[inline]
    pub fn session_file<'pool>(
        &'pool self,
        reader: &'pool ReaderCtx,
        file: FileId,
    ) -> Result<FileSession<'pool>, GetError> {
        let ReadSession {
            pin,
            translation,
            arena,
            clock,
            file_live_generations,
            identity,
        } = self.session(reader);
        let view = FileView::open(
            pin.slot(),
            translation,
            arena,
            clock,
            file_live_generations,
            identity,
            file,
        )?;
        Ok(FileSession { pin, view })
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
            arena: frames.session_arena(clock.session_reference_bits()),
            clock,
            file_live_generations,
            identity,
        }
    }
}

impl ReadSession<'_> {
    /// Validates `file`'s pool identity and liveness once for the reads of the
    /// returned view.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::StaleFile`] when [`Pool::retire_file`] was called
    /// for `file` before this call, or when its slot holds another file.
    ///
    /// # Panics
    ///
    /// If `file` belongs to another pool.
    #[inline]
    pub fn file(&self, file: FileId) -> Result<SessionFile<'_>, GetError> {
        Ok(SessionFile {
            pin: &self.pin,
            view: FileView::open(
                self.pin.slot(),
                self.translation,
                self.arena,
                self.clock,
                self.file_live_generations,
                self.identity,
                file,
            )?,
        })
    }

    /// The session resolution of one page, shared with the Loom seam.
    #[cfg(loom)]
    pub(crate) fn read_page(&self, page: PageId) -> Result<SessionRead<'_>, GetError> {
        self.file(page.file())?
            .view
            .resolve(&self.pin, page.granule_idx())
    }
}

impl SessionFile<'_> {
    /// Reads one granule of the file without submitting I/O: `Hit` with the
    /// granule's bytes, borrowed until the session drops, or `Absent` when
    /// the granule is not resident.
    ///
    /// If the file is retired after [`ReadSession::file`] returned this view,
    /// reads keep returning that file's resident granules or `Absent` until
    /// the session drops. They never return another file's or another
    /// granule's bytes, including after the slot is registered again, because
    /// the slot is not reused while this session is open. Retirement removes
    /// the file's resident granules, so most reads after it return `Absent`,
    /// and the [`Pool::get`] that resolves one returns
    /// [`GetError::StaleFile`]. A live resident lease, miss waiters, or a miss
    /// submitted before retirement can keep a granule of the file resident
    /// until the next progress pass; its bytes come from the file's own
    /// handle.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::BeyondFileBound`] for a granule at or above
    /// `file_granules_max`.
    #[inline]
    pub fn page(&self, granule_idx: u32) -> Result<SessionRead<'_>, GetError> {
        self.view.resolve(self.pin, granule_idx)
    }
}

impl FileSession<'_> {
    /// Reads one granule of the file as [`SessionFile::page`] does, with the
    /// same semantics for a file retired after [`Pool::session_file`]
    /// returned this session.
    ///
    /// # Errors
    ///
    /// Returns [`GetError::BeyondFileBound`] for a granule at or above
    /// `file_granules_max`.
    #[inline]
    pub fn page(&self, granule_idx: u32) -> Result<SessionRead<'_>, GetError> {
        self.view.resolve(&self.pin, granule_idx)
    }
}

impl<'pool> FileView<'pool> {
    /// Asserts `file`'s pool identity, then loads the file-generation mirror
    /// once, after the session pin published the reader's epoch (INV-7).
    #[inline]
    fn open(
        reader: &'pool ReaderSlot,
        translation: &'pool Translation,
        arena: SessionArena<'pool>,
        clock: &'pool Clock,
        file_live_generations: &'pool [AtomicU64],
        identity: u64,
        file: FileId,
    ) -> Result<Self, GetError> {
        assert!(
            file.driver() == identity,
            "file identity used with a foreign pool"
        );
        let entries = translation.slot_entries(file);
        if !file_generation_is_live(&file_live_generations[file.slot() as usize], file) {
            return Err(GetError::StaleFile {
                page: PageId::new(file, 0),
            });
        }
        Ok(Self {
            reader,
            entries,
            arena,
            clock,
            file,
        })
    }

    /// Loads the entry, then sets the CLOCK reference of the frame it borrows
    /// for as long as `pin` lives.
    #[inline]
    fn resolve<'session>(
        &self,
        pin: &'session SessionPin<'_>,
        granule_idx: u32,
    ) -> Result<SessionRead<'session>, GetError>
    where
        'pool: 'session,
    {
        let Some(entry) = self.entries.get(granule_idx as usize) else {
            return Err(GetError::BeyondFileBound {
                page: PageId::new(self.file, granule_idx),
            });
        };
        let Some(frame) = entry_frame(entry) else {
            return Ok(SessionRead::Absent);
        };
        let (reference_bit, bytes) = self.arena.frame(pin, frame);
        self.clock
            .reference_session(reference_bit, frame, self.reader);
        Ok(SessionRead::Hit(bytes))
    }
}
