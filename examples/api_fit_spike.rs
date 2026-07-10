//! API-fit spike (T016): drives two consumer shapes against an in-example stub
//! pool, falsifying the `Get`/`Pending`/`ready` contract before the real pool
//! (T006) is built. The stub — a fixed frame array plus a `HashMap` backing
//! store, no CLOCK/EBR/singleflight — pins the call surface by using it.
//!
//! The two shapes exercise the external consumer contracts this API must serve:
//!  1. nmnm Gateway loop — a faulted worker takes other resident work rather
//!     than blocking; and the waiter-interest drop, where dropping a
//!     `PendingToken` cancels interest only, yet the in-flight read still
//!     completes and the page becomes resident.
//!  2. sira k-way-merge suspend — cursors hold pinned `FrameGuard`s across a
//!     suspend on a non-resident next block, and an unrelated `poll()` must
//!     leave the held bytes stable.
//!
//! Two ADT decisions mirror the scope contract at stub level:
//!  A. Residency is a three-variant sum `Get::{Hit(FrameGuard), Pending(token),
//!     Busy}`, not an `Option<Guard>` beside a status flag.
//!  B. A readiness re-check is `ReadyResult::{Ready(guard), NotYet(token),
//!     Err(io)}`, handing the token back on `NotYet` (a non-consuming
//!     poll-again) and freeing the frame on `Err`.

use dios::mock::MockDriver;
use dios::{FrameGuard, Get, PageId, PendingToken, ReaderCtx, ReadyResult};

const FRAME_BYTES: u32 = 4096;

fn main() {
    gateway_loop_shape();
    k_way_merge_suspend_shape();
    println!("api-fit spike: both consumer shapes drove the stub pool");
}

fn stub_pool(seed: u64, frames: u32) -> StubPool {
    let driver = MockDriver::builder()
        .seed(seed)
        .queue_capacity(16)
        .frames(frames)
        .frame_bytes(FRAME_BYTES)
        .retry_bound(0)
        .build();
    StubPool::new(driver, frames, FRAME_BYTES)
}

fn assert_frame_fill(guard: &FrameGuard<'_>, fill: u8, frame_bytes: usize) {
    assert_eq!(
        guard.len(),
        frame_bytes,
        "a frame guard borrows the whole granule"
    );
    assert!(
        guard.iter().all(|&byte| byte == fill),
        "the borrowed bytes are this page's seeded content, not another frame's"
    );
    assert_eq!(guard[0], fill, "leading byte identifies the page");
    assert_eq!(
        guard[frame_bytes - 1],
        fill,
        "trailing byte identifies the page"
    );
}

fn drive_ready<'pool>(
    pool: &'pool StubPool,
    reader: &ReaderCtx<'pool>,
    token: PendingToken,
) -> FrameGuard<'pool> {
    let mut token = token;
    for _ in 0..64u32 {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => return guard,
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(err) => panic!("the spike injects no faults, got io error: {err}"),
        }
    }
    panic!("token never readied under bounded polling");
}

fn warm(pool: &StubPool, reader: &ReaderCtx<'_>, page: PageId, fill: u8) {
    match pool.get(reader, page) {
        Get::Pending(token) => {
            let guard = drive_ready(pool, reader, token);
            assert_frame_fill(&guard, fill, FRAME_BYTES as usize);
        }
        Get::Hit(_) => panic!("first touch of a cold page cannot be a warm hit"),
        Get::Busy => panic!("a fresh pool has spare frames; warm-up must submit"),
    }
}

fn gateway_loop_shape() {
    let pool = stub_pool(0x0060_0D5E, 8);
    let file = pool.file_id();
    let reader = pool
        .register_reader()
        .expect("first reader slot is available");

    let page_resident = PageId::new(file, 1);
    let page_miss = PageId::new(file, 2);
    let page_dropped = PageId::new(file, 3);
    pool.seed(page_resident, 0xBB);
    pool.seed(page_miss, 0xAA);
    pool.seed(page_dropped, 0xCC);

    warm(&pool, &reader, page_resident, 0xBB);

    let pending = match pool.get(&reader, page_miss) {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a cold page cannot hit"),
        Get::Busy => panic!("spare frames exist; a miss submits, it does not backpressure"),
    };

    match pool.get(&reader, page_resident) {
        Get::Hit(guard) => assert_frame_fill(&guard, 0xBB, FRAME_BYTES as usize),
        Get::Pending(_) => {
            panic!("a resident page must hit — re-submitting would block the worker")
        }
        Get::Busy => panic!("a resident hit never backpressures"),
    }

    let resumed = drive_ready(&pool, &reader, pending);
    assert_frame_fill(&resumed, 0xAA, FRAME_BYTES as usize);
    drop(resumed);

    let interest = match pool.get(&reader, page_dropped) {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("a cold page cannot hit"),
        Get::Busy => panic!("spare frames exist; a miss submits"),
    };
    drop(interest);
    pool.poll();
    match pool.get(&reader, page_dropped) {
        Get::Hit(guard) => assert_frame_fill(&guard, 0xCC, FRAME_BYTES as usize),
        Get::Pending(_) => {
            panic!(
                "dropping a PendingToken cancels waiter interest only — the read still completed and made the page resident"
            )
        }
        Get::Busy => panic!("a resident page is never Busy"),
    }
}

fn k_way_merge_suspend_shape() {
    let pool = stub_pool(0x00D5_7EED, 8);
    let file = pool.file_id();
    let reader = pool.register_reader().expect("reader slot is available");

    let sources = [
        (PageId::new(file, 10), 0x10u8),
        (PageId::new(file, 11), 0x11u8),
        (PageId::new(file, 12), 0x12u8),
    ];
    for (page, fill) in sources {
        pool.seed(page, fill);
    }

    let mut pending = Vec::with_capacity(sources.len());
    for (page, _) in sources {
        match pool.get(&reader, page) {
            Get::Pending(token) => pending.push(token),
            Get::Hit(_) => panic!("a cold source cannot hit"),
            Get::Busy => panic!("k < frame count; every source must submit"),
        }
    }

    let mut guards = Vec::with_capacity(sources.len());
    for ((_, fill), token) in sources.iter().zip(pending) {
        let guard = drive_ready(&pool, &reader, token);
        assert_frame_fill(&guard, *fill, FRAME_BYTES as usize);
        guards.push(guard);
    }

    let next_block = PageId::new(file, 20);
    pool.seed(next_block, 0x20);
    let suspended = match pool.get(&reader, next_block) {
        Get::Pending(token) => token,
        Get::Hit(_) => panic!("the next block is cold"),
        Get::Busy => panic!("still within the frame budget"),
    };

    let held = guards[1][0];
    assert_eq!(held, 0x11, "sibling guard's content before the poll");
    pool.poll();
    assert_eq!(
        guards[1][0], held,
        "a held guard's bytes stay stable across an unrelated poll"
    );
    assert_frame_fill(&guards[2], 0x12, FRAME_BYTES as usize);

    let resumed = drive_ready(&pool, &reader, suspended);
    assert_frame_fill(&resumed, 0x20, FRAME_BYTES as usize);
    drop(resumed);
    drop(guards);
}

use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::path::Path;

use dios::{CompletionBatch, FileHandle, FileId, OpToken, OpenHow, ReadFrameIdx};

const READER_SLOT_COUNT: u32 = 16;

/// A fixed frame array plus a `HashMap` backing store — no CLOCK, EBR, or
/// singleflight. It pins the pool call surface the spike drives; T006 replaces
/// it with the real pool.
struct StubPool {
    driver: MockDriver,
    file: FileHandle,
    frame_bytes: u32,
    frames: Box<[RefCell<Box<[u8]>>]>,
    metadata: RefCell<PoolMeta>,
    batch: RefCell<CompletionBatch>,
    readers: Cell<u32>,
}

struct PoolMeta {
    free: Vec<u32>,
    residents: HashMap<PageId, u32>,
    inflight: HashMap<OpToken, Miss>,
    backing: HashMap<PageId, u8>,
}

struct Miss {
    page: PageId,
    frame: u32,
}

impl StubPool {
    fn new(driver: MockDriver, frames: u32, frame_bytes: u32) -> Self {
        let file = driver
            .open(Path::new("stub"), OpenHow::read_write())
            .expect("stub file opens");
        let mut buffers = Vec::with_capacity(frames as usize);
        for _ in 0..frames {
            buffers.push(RefCell::new(
                vec![0u8; frame_bytes as usize].into_boxed_slice(),
            ));
        }
        let metadata = PoolMeta {
            free: (0..frames).rev().collect(),
            residents: HashMap::new(),
            inflight: HashMap::new(),
            backing: HashMap::new(),
        };
        Self {
            driver,
            file,
            frame_bytes,
            frames: buffers.into_boxed_slice(),
            metadata: RefCell::new(metadata),
            batch: RefCell::new(CompletionBatch::with_capacity(frames as usize)),
            readers: Cell::new(0),
        }
    }

    fn file_id(&self) -> FileId {
        self.file.file_id()
    }

    fn seed(&self, page: PageId, fill: u8) {
        self.metadata.borrow_mut().backing.insert(page, fill);
    }

    fn register_reader(&self) -> Option<ReaderCtx<'_>> {
        let taken = self.readers.get();
        if taken < READER_SLOT_COUNT {
            self.readers.set(taken + 1);
            Some(ReaderCtx::new())
        } else {
            None
        }
    }

    fn get(&self, _reader: &ReaderCtx<'_>, page: PageId) -> Get<'_> {
        if let Some(&frame) = self.metadata.borrow().residents.get(&page) {
            return Get::Hit(self.frame_guard(frame));
        }
        let Some(frame) = self.metadata.borrow_mut().free.pop() else {
            return Get::Busy;
        };
        let offset = u64::from(page.granule_idx()) * u64::from(self.frame_bytes);
        let Ok(token) = self
            .driver
            .submit_read(&self.file, ReadFrameIdx::new(frame), offset)
        else {
            self.metadata.borrow_mut().free.push(frame);
            return Get::Busy;
        };
        self.metadata
            .borrow_mut()
            .inflight
            .insert(token, Miss { page, frame });
        Get::Pending(PendingToken::new(page))
    }

    fn poll(&self) {
        let mut batch = self.batch.borrow_mut();
        self.driver.poll(&mut batch);
        let mut metadata = self.metadata.borrow_mut();
        for completion in batch.iter() {
            let Some(miss) = metadata.inflight.remove(&completion.token()) else {
                continue;
            };
            let fill = metadata
                .backing
                .get(&miss.page)
                .copied()
                .expect("a submitted read targets a seeded page");
            self.frames[miss.frame as usize].borrow_mut().fill(fill);
            metadata.residents.insert(miss.page, miss.frame);
        }
    }

    fn ready(&self, _reader: &ReaderCtx<'_>, token: PendingToken) -> ReadyResult<'_> {
        if let Some(&frame) = self.metadata.borrow().residents.get(&token.page()) {
            ReadyResult::Ready(self.frame_guard(frame))
        } else {
            ReadyResult::NotYet(token)
        }
    }

    fn frame_guard(&self, frame: u32) -> FrameGuard<'_> {
        let bytes = Ref::map(self.frames[frame as usize].borrow(), |buffer| &buffer[..]);
        FrameGuard::new(bytes)
    }
}
