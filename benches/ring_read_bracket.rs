//! `ring_read_bracket` (T014-shaped bracket): the driver ring read against the
//! classic blocking `pread`, both `O_DIRECT`, QD1, at identical random 4 KiB-aligned
//! offsets over a 64 MiB file. Base arm `pread`s into a 4 KiB-aligned user buffer;
//! candidate arm `submit_read` -> `poll` -> completion, asserting a full-granule
//! CQE and probing one landed byte (no whole-granule copy-out — sira borrows the
//! frame in place). Device-bound: the ratio isolates the
//! ring's submit/reap overhead against the shared ~70 µs `NVMe` floor. Linux-only —
//! the ring backend exists only there. The gate (ci95 upper <= 1.25) is asserted by
//! the shared compare harness, never in-bench; run on the pinned host, not here.

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    bracket::run();
}

#[cfg(target_os = "linux")]
mod bracket {
    use std::alloc::{alloc, dealloc, Layout};
    use std::cell::Cell;
    use std::hint::black_box;
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use dios::driver::{CompletionBatch, Driver};
    use dios::testing::{DriverObservation, DriverReadTestingExt, ReadFrameIdx};
    use dios::DirectIo;

    const GRANULE: usize = 4096;
    const FILE_GRANULES: u32 = 16_384;
    const FRAMES: u32 = 64;
    const QUEUE_CAPACITY: u32 = 4;
    const REPS: u32 = 40;
    const ITERS_PER_REP: u32 = 8;
    const BOOTSTRAP_RESAMPLES: u32 = 10_000;
    const RETRY_BOUND: u32 = 3;
    const POLL_REAP_MAX: u32 = 1_000_000;

    // O_DIRECT is arch-specific in the linux uapi asm/fcntl.h: 0x4000 on x86_64,
    // 0x10000 on aarch64 (where 0x4000 is O_DIRECTORY). Re-declared bench-locally
    // rather than exporting the crate-internal const in src/open.rs.
    #[cfg(target_arch = "x86_64")]
    const O_DIRECT: i32 = 0x4000;
    #[cfg(target_arch = "aarch64")]
    const O_DIRECT: i32 = 0x10000;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!(
        "O_DIRECT is arch-specific; add this target_arch's value from linux asm/fcntl.h"
    );

    fn build_offsets() -> Vec<u64> {
        let count = ITERS_PER_REP as usize;
        let mut offsets = Vec::with_capacity(count);
        let mut state = 0x2114_9E37_79B9_7F4A_u64;
        let span = u64::from(FILE_GRANULES);
        for _ in 0..count {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            offsets.push((z % span) * GRANULE as u64);
        }
        offsets
    }

    fn next_offset(cursor: &Cell<usize>, offsets: &[u64]) -> u64 {
        let k = cursor.get();
        cursor.set(k + 1);
        offsets[k % offsets.len()]
    }

    fn temp_path(tag: &str) -> PathBuf {
        let mut path =
            std::option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        std::fs::create_dir_all(&path).expect("target tmp dir");
        path.push(format!("dios-{tag}-{}", std::process::id()));
        path
    }

    fn preallocated_file(path: &Path) {
        let granule = vec![0xD1_u8; GRANULE];
        let file = std::fs::File::create(path).expect("create the ring-bracket file");
        let mut offset = 0u64;
        for _ in 0..FILE_GRANULES {
            file.write_all_at(&granule, offset)
                .expect("write a granule");
            offset += GRANULE as u64;
        }
        file.sync_all().expect("fsync the preallocated file");
    }

    /// A 4 KiB-aligned heap buffer for the `O_DIRECT` `pread` arm, freed on drop.
    struct AlignedBuffer {
        ptr: *mut u8,
        layout: Layout,
    }

    impl AlignedBuffer {
        fn granule() -> Self {
            let layout = Layout::from_size_align(GRANULE, GRANULE).expect("valid aligned layout");
            // SAFETY: `layout` has a non-zero size (`GRANULE`); a null return aborts
            // below, so `ptr` is a live `GRANULE`-byte, `GRANULE`-aligned allocation.
            let ptr = unsafe { alloc(layout) };
            assert!(!ptr.is_null(), "aligned granule buffer allocation");
            Self { ptr, layout }
        }

        fn as_mut_slice(&mut self) -> &mut [u8] {
            // SAFETY: `ptr` addresses `GRANULE` live, aligned bytes owned solely by
            // this buffer for the borrow's duration.
            unsafe { std::slice::from_raw_parts_mut(self.ptr, GRANULE) }
        }
    }

    impl Drop for AlignedBuffer {
        fn drop(&mut self) {
            // SAFETY: `ptr`/`layout` are exactly the pair returned by `alloc` in
            // `granule`, freed once here at end of life.
            unsafe { dealloc(self.ptr, self.layout) }
        }
    }

    fn reap_one(drv: &Driver, batch: &mut CompletionBatch) {
        for _ in 0..POLL_REAP_MAX {
            if drv.poll(batch) > 0 {
                let completion = batch.iter().next().expect("one reaped completion");
                let bytes = completion
                    .result()
                    .expect("the ring read completes without error");
                assert_eq!(
                    usize::try_from(bytes).expect("byte count fits usize"),
                    GRANULE,
                    "the ring read landed a full granule"
                );
                let mut landed = [0u8; 1];
                let copied = drv.copy_frame(ReadFrameIdx::new(0), &mut landed);
                assert_eq!(copied, 1, "one landed frame byte is observable");
                black_box(landed[0]);
                return;
            }
        }
        panic!("a QD1 ring read never reaped within the poll budget");
    }

    pub(crate) fn run() {
        let offsets = build_offsets();
        let path = temp_path("ring-bracket");
        preallocated_file(&path);

        let pread_file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECT)
            .open(&path)
            .expect("open the O_DIRECT pread fd");
        let mut buffer = AlignedBuffer::granule();

        let drv = Driver::builder()
            .queue_capacity(QUEUE_CAPACITY)
            .frames(FRAMES)
            .frame_bytes(u32::try_from(GRANULE).expect("granule fits u32"))
            .retry_bound(RETRY_BOUND)
            .build()
            .expect("the io_uring driver initializes");
        let fd = drv
            .open(&path, DirectIo::Preferred)
            .expect("open the ring O_DIRECT fd");
        let mut batch = CompletionBatch::with_capacity(1);

        let pread_cursor = Cell::new(0usize);
        let ring_cursor = Cell::new(0usize);

        let samples = dios::bench::run_paired(
            "ring_read_bracket",
            REPS,
            ITERS_PER_REP,
            || {
                let offset = next_offset(&pread_cursor, &offsets);
                let slice = buffer.as_mut_slice();
                pread_file
                    .read_exact_at(slice, offset)
                    .expect("O_DIRECT pread of a full granule");
                black_box(slice.as_ptr());
            },
            || {
                let offset = next_offset(&ring_cursor, &offsets);
                drv.submit_read(&fd, ReadFrameIdx::new(0), offset)
                    .expect("submit within capacity");
                reap_one(&drv, &mut batch);
            },
        );

        let gate = dios::bench::ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
        let out = dios::bench::write_samples(Path::new("target/bench-samples"), &samples)
            .expect("write samples CSV");
        println!(
            "ring_read_bracket: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
            gate.ratio_geomean,
            gate.ratio_ci95_upper,
            out.display()
        );

        drv.close(fd);
        let _ = std::fs::remove_file(&path);
    }
}
