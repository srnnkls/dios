//! `mmap_tlb_pressure`: the `mmap_warm_path` bracket at a 1024× larger working
//! set (65,536 pages / 256 MiB) with uniform random access across the whole set,
//! so both arms run under real dTLB pressure. Base arm reads a 4 KiB granule from
//! a file-backed `PROT_READ`/`MAP_SHARED` mapping (one dTLB entry per 4 KiB page);
//! candidate arm reads through a `FrameGuard` from a warm `Pool::get` hit over a
//! contiguous anonymous arena the kernel may back with transparent hugepages.
//! Identical granule scan, lock-step random indices — the ratio isolates the
//! residency machinery plus whatever TLB-reach difference the two mappings carry.
//! Characterization, not a binding gate; the in-bench ci95 ≤ 3.0 is the same
//! SANITY bound as `mmap_warm_path`. The THP state makes any Linux number
//! interpretable — see the plan. All platforms — the mock backend composes
//! anywhere.

use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::hint::black_box;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use dios::testing::{MockDriver, PoolBuilderTestingExt, PoolTestingExt};
use dios::{DirectIo, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult};

const GRANULE: usize = 4096;
const RESIDENT_PAGES: u32 = 65_536;
const INDEX_POOL: usize = 16_384;
const REPS: u32 = 40;
const ITERS_PER_REP: u32 = 256;
const BOOTSTRAP_RESAMPLES: u32 = 10_000;
const RESIDENCY_RATIO_MAX: f64 = 3.0;
const QUEUE_CAPACITY: u32 = 8;
const MAX_INFLIGHT: u32 = 1;
const MISS_HEADROOM: u32 = 3;
const READY_POLLS_MAX: u32 = 64;

// `PROT_READ` (0x1) and `MAP_SHARED` (0x1) hold on both linux and darwin uapi;
// `off_t` is a signed 64-bit file offset on both. A file-backed shared read
// mapping avoids the per-OS `MAP_ANON`/`MAP_ANONYMOUS` split entirely.
const PROT_READ: c_int = 0x1;
const MAP_SHARED: c_int = 0x1;

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

/// XOR-folds a granule as `u64` words — the identical scan both arms run, so
/// they differ only in how the granule bytes are reached.
fn scan_granule(bytes: &[u8]) -> u64 {
    assert_eq!(bytes.len(), GRANULE, "a scanned granule is a full granule");
    bytes.chunks_exact(8).fold(0u64, |acc, chunk| {
        acc ^ u64::from_ne_bytes(chunk.try_into().expect("an 8-byte chunk"))
    })
}

/// The next replayed index, wrapping every `indices.len()` calls. Base and
/// candidate keep separate cursors that stay lock-step (both are called the same
/// number of times up to any rep boundary), so each arm reads the same index at
/// the same logical iteration. The pool is longer than one rep so consecutive
/// reps walk fresh random pages across the whole set (dTLB pressure), not one
/// replayed 256-page slice.
fn next_index(cursor: &Cell<usize>, indices: &[u32]) -> u32 {
    let k = cursor.get();
    cursor.set(k + 1);
    indices[k % indices.len()]
}

fn build_indices() -> Vec<u32> {
    let mut indices = Vec::with_capacity(INDEX_POOL);
    let mut state = 0x71B0_0C1A_9E37_79B9_u64;
    let span = u64::from(RESIDENT_PAGES);
    for _ in 0..INDEX_POOL {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        indices.push(u32::try_from(z % span).expect("index below the resident-page span"));
    }
    indices
}

/// Owns the backing file and temp path so the mapping outlives the bench; the
/// mapping is pre-faulted resident before any measurement so page faults never
/// pollute the timed region.
struct MmapRegion {
    base: *mut c_void,
    len: usize,
    _file: std::fs::File,
    path: PathBuf,
}

impl MmapRegion {
    fn resident() -> Self {
        let len = RESIDENT_PAGES as usize * GRANULE;
        let path = temp_path("mmap-tlb");
        write_backing_file(&path);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open the mmap-backing file");
        // SAFETY: `fd` is live for the call (owned by `file`); a `PROT_READ`
        // `MAP_SHARED` map of `len` bytes at offset 0 reads only the file's own
        // extent, which was just written to `len`.
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert!(
            !base.is_null() && base.addr() != usize::MAX,
            "mmap of the resident region must not return MAP_FAILED"
        );
        let region = Self {
            base,
            len,
            _file: file,
            path,
        };
        region.prefault();
        region
    }

    fn prefault(&self) {
        for idx in 0..RESIDENT_PAGES {
            black_box(scan_granule(self.granule(idx)));
        }
    }

    fn granule(&self, idx: u32) -> &[u8] {
        let offset = idx as usize * GRANULE;
        assert!(offset + GRANULE <= self.len, "granule within the mapping");
        // SAFETY: the assert bounds `offset` within the mapping.
        let start = unsafe { self.base.cast::<u8>().add(offset) };
        // SAFETY: `start` addresses `GRANULE` resident, initialised bytes that
        // live as long as `&self`.
        unsafe { std::slice::from_raw_parts(start, GRANULE) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are exactly the mapping returned by `mmap` in
        // `resident`, unmapped once here at end of life.
        let status = unsafe { munmap(self.base, self.len) };
        assert_eq!(status, 0, "munmap of the resident region");
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_backing_file(path: &Path) {
    let granule = vec![0xA5_u8; GRANULE];
    let file = std::fs::File::create(path).expect("create the mmap-backing file");
    let mut offset = 0u64;
    for _ in 0..RESIDENT_PAGES {
        file.write_all_at(&granule, offset)
            .expect("write a granule");
        offset += GRANULE as u64;
    }
    file.sync_all().expect("fsync the backing file");
}

fn temp_path(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("dios-{tag}-{}", std::process::id()));
    path
}

/// Every measured `get` is a warm hit, so the composed driver stays untouched
/// after the warmup and the mock composition is fair.
fn resident_pool() -> (Pool<MockDriver>, dios::FileId) {
    let granule = u32::try_from(GRANULE).expect("granule fits u32");
    let mock = MockDriver::builder()
        .seed(0x71B0_0C1A)
        .queue_capacity(QUEUE_CAPACITY)
        .frames(RESIDENT_PAGES)
        .frame_bytes(granule)
        .retry_bound(0)
        .build();
    let file = mock
        .open(Path::new("mmap-tlb-pool"), DirectIo::Disabled)
        .expect("mock file opens");
    let file_id = file.file_id();
    for idx in 0..RESIDENT_PAGES {
        mock.seed_page(
            &file,
            idx,
            u8::try_from(0xA0 | (idx & 0x1F)).expect("fill fits u8"),
        );
    }
    let pool = Pool::builder()
        .frame_count(RESIDENT_PAGES)
        .granule(granule)
        .max_concurrent_readers(1)
        .peak_guards_per_reader(1)
        .max_inflight_reads(MAX_INFLIGHT)
        .miss_headroom(MISS_HEADROOM)
        .build_on(mock)
        .expect("watermark-satisfying warm pool composes over the mock at 65,536 frames");
    pool.register_file(file);
    (pool, file_id)
}

fn warm_all_pages(pool: &Pool<MockDriver>, reader: &ReaderCtx, file_id: dios::FileId) {
    for idx in 0..RESIDENT_PAGES {
        let page = PageId::new(file_id, idx);
        match pool.get(reader, page).expect("the registered file is live") {
            Get::Pending(token) => {
                drive_ready(pool, reader, token);
            }
            Get::Hit(_) => panic!("a cold warmup page cannot already hit"),
            Get::Busy => panic!("within the watermark a warmup miss submits, never Busy"),
        }
    }
}

fn drive_ready<'pool>(
    pool: &'pool Pool<MockDriver>,
    reader: &'pool ReaderCtx,
    token: PendingToken,
) {
    let mut token = token;
    for _ in 0..READY_POLLS_MAX {
        match pool.ready(reader, token) {
            ReadyResult::Ready(guard) => {
                black_box(guard.len());
                return;
            }
            ReadyResult::NotYet(handed_back) => {
                token = handed_back;
                pool.poll();
            }
            ReadyResult::Err(err) => panic!("a fault-free warmup miss must not error: {err:?}"),
        }
    }
    panic!("a warmup miss never readied within the bounded poll budget");
}

fn main() {
    let indices = build_indices();
    let region = MmapRegion::resident();
    let (pool, file_id) = resident_pool();
    let reader = pool.register_reader().expect("first reader slot");
    warm_all_pages(&pool, &reader, file_id);

    let mmap_cursor = Cell::new(0usize);
    let pool_cursor = Cell::new(0usize);

    let samples = dios::bench::run_paired(
        "mmap_tlb_pressure",
        REPS,
        ITERS_PER_REP,
        || {
            let idx = next_index(&mmap_cursor, &indices);
            black_box(scan_granule(region.granule(idx)));
        },
        || {
            let idx = next_index(&pool_cursor, &indices);
            match pool
                .get(&reader, PageId::new(file_id, idx))
                .expect("the registered file is live")
            {
                Get::Hit(guard) => {
                    black_box(scan_granule(&guard));
                }
                Get::Pending(_) => panic!("a resident page must hit, not submit a miss"),
                Get::Busy => panic!("a resident page is never Busy"),
            }
        },
    );

    let gate = dios::bench::ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
    let path = dios::bench::write_samples(Path::new("target/bench-samples"), &samples)
        .expect("write samples CSV");
    println!(
        "mmap_tlb_pressure: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
        gate.ratio_geomean,
        gate.ratio_ci95_upper,
        path.display()
    );
    assert!(
        gate.ratio_ci95_upper <= RESIDENCY_RATIO_MAX,
        "pool residency machinery must stay within {RESIDENCY_RATIO_MAX}x of a bare mmap read, got ci95 upper {:.4}",
        gate.ratio_ci95_upper
    );
}
