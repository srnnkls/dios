//! `synced_write`: the time to make one 4 KiB pool write durable, QD1
//! overwrites of a preallocated `O_DIRECT` file through the shipping `io_uring`
//! pool. Base arm: `submit_write`, wait, `submit_fsync(Data)`, wait, the
//! sequence sira's commit log issues today. Candidate arm: one
//! `submit_write_synced(Data)`, whose completion covers its durability. Before
//! timing, one rep of each arm runs between two reads of the backing disk's
//! completed-flush counter: a candidate whose flag never reached the kernel
//! would time as a bare write and show no flushes on a disk with a volatile
//! cache and no FUA. The gate (ci95 upper <= 0.85) is asserted by the shared
//! compare harness, never in-bench. Linux-only: the ring backend exists only
//! there.

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    synced::run();
}

#[cfg(target_os = "linux")]
mod synced {
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::{FileExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use dios::{DirectIo, FileId, Pool, PoolCompletion, PoolCompletionBatch, PoolToken, SyncMode};

    const GRANULE: u32 = 4096;
    const FILE_PAGES: u32 = 64;
    const REPS: u32 = 40;
    const WRITES_PER_REP: u32 = 64;
    const BOOTSTRAP_RESAMPLES: u32 = 10_000;
    const WAIT: Duration = Duration::from_millis(100);
    const WAITS_MAX: u32 = 1_000;
    /// `/sys/block/<disk>/stat` field 16 (zero-based 15): flush requests
    /// completed, present since Linux 5.5.
    const STAT_FLUSHES_FIELD: usize = 15;

    fn temp_path() -> PathBuf {
        let mut path =
            std::option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        std::fs::create_dir_all(&path).expect("target tmp dir");
        path.push(format!("dios-synced-write-{}", std::process::id()));
        path
    }

    fn preallocated_file(path: &Path) {
        let page = vec![0xA5_u8; GRANULE as usize];
        let file = std::fs::File::create(path).expect("create the synced-write file");
        for index in 0..FILE_PAGES {
            file.write_all_at(&page, u64::from(index) * u64::from(GRANULE))
                .expect("write a page");
        }
        file.sync_all().expect("fsync the preallocated file");
    }

    /// The sysfs directory of the whole disk under `path`'s filesystem, or
    /// `None` when that filesystem has no block device.
    fn backing_disk(path: &Path) -> Option<PathBuf> {
        let device = std::fs::metadata(path).ok()?.dev();
        let major = ((device >> 32) & 0xFFFF_F000) | ((device >> 8) & 0x0000_0FFF);
        let minor = ((device >> 12) & 0xFFFF_FF00) | (device & 0x0000_00FF);
        let node = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
        if node.join("partition").exists() {
            node.parent().map(Path::to_path_buf)
        } else {
            Some(node)
        }
    }

    fn disk_flushes(disk: &Path) -> Option<u64> {
        let stat = std::fs::read_to_string(disk.join("stat")).ok()?;
        stat.split_whitespace()
            .nth(STAT_FLUSHES_FIELD)?
            .parse()
            .ok()
    }

    fn disk_queue(disk: &Path, attribute: &str) -> String {
        std::fs::read_to_string(disk.join("queue").join(attribute))
            .map_or_else(|_| "unknown".to_owned(), |value| value.trim().to_owned())
    }

    struct Arm<'pool> {
        pool: &'pool Pool,
        file: FileId,
        next_page: Cell<u32>,
        completions: RefCell<PoolCompletionBatch>,
    }

    impl Arm<'_> {
        /// The next page's offset and fill byte: writes cycle through the
        /// preallocated pages, so every write is a pure overwrite.
        fn next_write(&self) -> (u64, u8) {
            let count = self.next_page.get();
            self.next_page.set(count.wrapping_add(1));
            let page = count % FILE_PAGES;
            let fill = u8::try_from(count % 251).expect("a residue below 251 fits u8");
            (u64::from(page) * u64::from(GRANULE), fill)
        }

        fn wait(&self, expected: PoolToken) {
            let mut completions = self.completions.borrow_mut();
            for _ in 0..WAITS_MAX {
                self.pool.poll_wait(&mut completions, WAIT);
                let mut delivered = completions.iter();
                let Some(completion) = delivered.next() else {
                    continue;
                };
                assert!(
                    delivered.next().is_none(),
                    "one operation is in flight at QD1"
                );
                match completion {
                    PoolCompletion::Write { token, result } => {
                        assert_eq!(*token, expected, "only the awaited write is in flight");
                        let bytes = result.as_ref().expect("the durable write succeeds");
                        assert_eq!(*bytes, GRANULE, "the write covers its whole page");
                    }
                    PoolCompletion::Fsync { token, result } => {
                        assert_eq!(*token, expected, "only the awaited sync is in flight");
                        result.as_ref().expect("the data sync succeeds");
                    }
                }
                return;
            }
            panic!("the awaited operation did not complete within the wait bound");
        }

        fn write_then_sync(&self) {
            let (offset, fill) = self.next_write();
            let mut slot = self
                .pool
                .write_arena()
                .alloc()
                .expect("a free staging slot");
            slot.fill(fill);
            let write = self
                .pool
                .submit_write(self.file, slot, offset)
                .map_err(|(error, _)| error)
                .expect("the write admits at QD1");
            self.wait(write);
            let sync = self
                .pool
                .submit_fsync(self.file, SyncMode::Data)
                .expect("the sync admits at QD1");
            self.wait(sync);
        }

        fn synced_write(&self) {
            let (offset, fill) = self.next_write();
            let mut slot = self
                .pool
                .write_arena()
                .alloc()
                .expect("a free staging slot");
            slot.fill(fill);
            let write = self
                .pool
                .submit_write_synced(self.file, slot, offset, SyncMode::Data)
                .map_err(|(error, _)| error)
                .expect("the synced write admits at QD1");
            self.wait(write);
        }
    }

    fn flushes_per_write(disk: &Path, mut durable_write: impl FnMut()) -> Option<f64> {
        let before = disk_flushes(disk)?;
        for _ in 0..WRITES_PER_REP {
            durable_write();
        }
        let after = disk_flushes(disk)?;
        let flushes = u32::try_from(after.saturating_sub(before)).unwrap_or(u32::MAX);
        Some(f64::from(flushes) / f64::from(WRITES_PER_REP))
    }

    fn report_flush_witness(path: &Path, base: &Arm<'_>, candidate: &Arm<'_>) {
        let Some(disk) = backing_disk(path) else {
            println!("synced_write: flush witness unavailable (no backing block device)");
            return;
        };
        let base_flushes = flushes_per_write(&disk, || base.write_then_sync());
        let candidate_flushes = flushes_per_write(&disk, || candidate.synced_write());
        match (base_flushes, candidate_flushes) {
            (Some(base_flushes), Some(candidate_flushes)) => println!(
                "synced_write: flushes per durable write on {}: base {base_flushes:.2}, \
                 candidate {candidate_flushes:.2} (write_cache {}, fua {})",
                disk.display(),
                disk_queue(&disk, "write_cache"),
                disk_queue(&disk, "fua"),
            ),
            _ => println!(
                "synced_write: flush witness unavailable ({} reports no flush counter)",
                disk.display()
            ),
        }
    }

    pub(crate) fn run() {
        let path = temp_path();
        preallocated_file(&path);
        let pool = Pool::builder()
            .frame_count(4)
            .granule(GRANULE)
            .max_concurrent_readers(1)
            .peak_guards_per_reader(1)
            .max_inflight_reads(1)
            .miss_headroom(3)
            .write_slots(2)
            .max_inflight_product_ops(2)
            .build()
            .expect("a product-write pool over the shipping backend");
        let file = pool
            .open(&path, DirectIo::Required)
            .expect("open the preallocated file for direct IO");
        let arm = || Arm {
            pool: &pool,
            file,
            next_page: Cell::new(0),
            completions: RefCell::new(PoolCompletionBatch::with_capacity(2)),
        };
        let base = arm();
        let candidate = arm();

        report_flush_witness(&path, &base, &candidate);
        let samples = dios::bench::run_paired(
            "synced_write",
            REPS,
            WRITES_PER_REP,
            || base.write_then_sync(),
            || candidate.synced_write(),
        );

        let gate = dios::bench::ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
        let out = dios::bench::write_samples(Path::new("target/bench-samples"), &samples)
            .expect("write samples CSV");
        println!(
            "synced_write: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, samples {}",
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
