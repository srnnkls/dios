//! Minimal product path: a pool owns opened files and lends resident bytes.

use std::path::Path;

use dios::{DirectIo, Get, IoError, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const POLLS_MAX: u32 = 64;

enum ReadState {
    Lookup,
    Waiting(PendingToken),
}

enum ReadError {
    Io(IoError),
    PollLimit,
}

fn main() {
    let pool = match Pool::builder()
        .frame_count(16)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(1)
        .miss_headroom(3)
        .build()
    {
        Ok(pool) => pool,
        Err(error) => {
            eprintln!("pool initialization failed: {error}");
            return;
        }
    };
    let file = match pool.open(Path::new("segment.data"), DirectIo::Preferred) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("file open failed: {error}");
            return;
        }
    };
    let reader = match pool.register_reader() {
        Ok(reader) => reader,
        Err(error) => {
            eprintln!("reader registration failed: {error}");
            return;
        }
    };
    let page = PageId::new(file, 0);

    match read_page(&pool, &reader, page) {
        Ok(()) => {}
        Err(ReadError::Io(error)) => eprintln!("page read failed: {error}"),
        Err(ReadError::PollLimit) => eprintln!("page did not become ready within the poll bound"),
    }

    drop(reader);
    drop(pool);
}

fn read_page(pool: &Pool, reader: &ReaderCtx<'_>, page: PageId) -> Result<(), ReadError> {
    let mut state = ReadState::Lookup;
    for _ in 0..POLLS_MAX {
        state = match state {
            ReadState::Lookup => match pool.get(reader, page) {
                Get::Hit(frame) => {
                    std::hint::black_box(&*frame);
                    return Ok(());
                }
                Get::Pending(token) => ReadState::Waiting(token),
                Get::Busy => {
                    pool.poll();
                    ReadState::Lookup
                }
            },
            ReadState::Waiting(token) => {
                pool.poll();
                match pool.ready(reader, token) {
                    ReadyResult::Ready(frame) => {
                        std::hint::black_box(&*frame);
                        return Ok(());
                    }
                    ReadyResult::NotYet(token) => ReadState::Waiting(token),
                    ReadyResult::Err(error) => return Err(ReadError::Io(error)),
                }
            }
        };
    }
    Err(ReadError::PollLimit)
}
