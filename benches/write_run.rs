//! `write_run`: the time to write a 64 MiB file sequentially through the
//! shipping `io_uring` pool, every write extending the file, 16 writes in
//! flight. Base arm: one 4 KiB slot per write (16,384 writes), the way sira's
//! segment sink writes a draft. Candidate arm: one 8-slot run per write (2,048
//! writes of 32 KiB). Both arms share one open `O_DIRECT` file and truncate it
//! to zero as each rep starts, so every write extends it again; the truncation
//! costs the same in both. The gate (ci95 upper <= 0.60) is asserted by the
//! shared compare harness, never in-bench. Linux-only: the ring backend exists
//! only there.

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    run::run();
}

#[cfg(target_os = "linux")]
mod run {
    use std::cell::RefCell;
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use dios::{DirectIo, FileId, Pool, PoolCompletion, PoolCompletionBatch};

    const GRANULE: u32 = 4096;
    const FILE_BYTES: u64 = 64 << 20;
    const IN_FLIGHT: u32 = 16;
    const RUN_SLOTS: u32 = 8;
    const REPS: u32 = 30;
    const BOOTSTRAP_RESAMPLES: u32 = 10_000;
    const WAIT: Duration = Duration::from_millis(100);
    const WAITS_MAX: u32 = 1_000_000;

    fn temp_path() -> PathBuf {
        let mut path =
            std::option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        std::fs::create_dir_all(&path).expect("target tmp dir");
        path.push(format!("dios-write-run-{}", std::process::id()));
        path
    }

    struct Arm<'pool> {
        pool: &'pool Pool,
        file: FileId,
        truncate: &'pool File,
        slots_per_write: u32,
        completions: RefCell<PoolCompletionBatch>,
    }

    impl Arm<'_> {
        fn write_bytes(&self) -> u32 {
            self.slots_per_write * GRANULE
        }

        /// Writes the whole file in order with at most `IN_FLIGHT` writes
        /// admitted, returning once every write has completed.
        fn write_file(&self) {
            self.truncate
                .set_len(0)
                .expect("truncate the bench file to zero");
            let writes = FILE_BYTES / u64::from(self.write_bytes());
            let mut submitted = 0u64;
            let mut completed = 0u64;
            for _ in 0..WAITS_MAX {
                if completed == writes {
                    return;
                }
                submitted = self.fill_window(submitted, completed, writes);
                completed += self.reap();
            }
            panic!("the file did not complete within the wait bound");
        }

        fn fill_window(&self, mut submitted: u64, completed: u64, writes: u64) -> u64 {
            while submitted < writes && submitted - completed < u64::from(IN_FLIGHT) {
                let Some(mut slot) = self.pool.write_arena().alloc_run(self.slots_per_write) else {
                    break;
                };
                slot.fill(u8::try_from(submitted % 251).expect("a residue below 251 fits u8"));
                self.pool
                    .submit_write(self.file, slot, submitted * u64::from(self.write_bytes()))
                    .map_err(|(error, _)| error)
                    .expect("the window stays within the product capacity");
                submitted += 1;
            }
            submitted
        }

        fn reap(&self) -> u64 {
            let mut completions = self.completions.borrow_mut();
            self.pool.poll_wait(&mut completions, WAIT);
            let mut reaped = 0u64;
            for completion in completions.iter() {
                match completion {
                    PoolCompletion::Write {
                        result: Ok(bytes), ..
                    } => {
                        assert_eq!(*bytes, self.write_bytes(), "every write lands whole");
                        reaped += 1;
                    }
                    PoolCompletion::Write {
                        result: Err(error), ..
                    } => panic!("a bench write failed: {error}"),
                    PoolCompletion::Fsync { .. } => panic!("the bench admits no fsync"),
                }
            }
            reaped
        }
    }

    pub(crate) fn run() {
        let path = temp_path();
        File::create(&path).expect("create the bench file");
        let truncate = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open the truncation handle");
        let pool = Pool::builder()
            .frame_count(4)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .write_slots(IN_FLIGHT * RUN_SLOTS)
            .max_inflight_product_ops(IN_FLIGHT)
            .build()
            .expect("a product-write pool over the shipping backend");
        let file = pool
            .open(&path, DirectIo::Required)
            .expect("open the bench file for direct IO");
        let arm = |slots_per_write| Arm {
            pool: &pool,
            file,
            truncate: &truncate,
            slots_per_write,
            completions: RefCell::new(PoolCompletionBatch::with_capacity(
                usize::try_from(IN_FLIGHT).expect("the window fits usize"),
            )),
        };
        let base = arm(1);
        let candidate = arm(RUN_SLOTS);

        let samples = dios::bench::run_paired(
            "write_run",
            REPS,
            1,
            || base.write_file(),
            || candidate.write_file(),
        );

        let gate = dios::bench::ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
        let out = dios::bench::write_samples(Path::new("target/bench-samples"), &samples)
            .expect("write samples CSV");
        println!(
            "write_run: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
            gate.ratio_geomean,
            gate.ratio_ci95_upper,
            out.display()
        );
        drop(base);
        drop(candidate);
        drop(pool);
        let _ = std::fs::remove_file(&path);
    }
}
