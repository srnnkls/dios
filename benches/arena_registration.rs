//! `arena_registration` (pinned-frame-retention T14): the pool read under the
//! `Unregistered` posture against the same read under `Registered`, both
//! `O_DIRECT`, QD1, at identical random 4 KiB-aligned offsets over a 64 MiB
//! file (working set ≫ the 64-frame pool, so every read is a miss). Base arm
//! `Registered` (`READ_FIXED`), candidate arm `Unregistered` (plain `READ` by
//! pointer); the geomean is the recorded per-op cost of not registering.
//! Linux-only — the eager backend has one posture. The gate (ci95 upper <= 1.15)
//! is asserted by the shared compare harness, never in-bench; run on the pinned
//! host, not here. A refused `Registered` build fails the bench rather than
//! silently comparing two unregistered arms.

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    posture::run();
}

#[cfg(target_os = "linux")]
mod posture {
    use std::cell::Cell;
    use std::hint::black_box;
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};

    use dios::{
        DirectIo, FileId, Get, PageId, PendingToken, Pool, ReaderCtx, ReadyResult,
        RegistrationPolicy, RegistrationPosture,
    };

    const GRANULE: u32 = 4096;
    const FILE_GRANULES: u32 = 16_384;
    const FRAMES: u32 = 64;
    const REPS: u32 = 40;
    const ITERS_PER_REP: u32 = 8;
    const BOOTSTRAP_RESAMPLES: u32 = 10_000;
    const POLLS_MAX: u32 = 1_000_000;

    fn build_granules() -> Vec<u32> {
        let warmup_reps = REPS.div_ceil(10);
        let count = ((REPS + warmup_reps) * ITERS_PER_REP) as usize;
        let mut granules = Vec::with_capacity(count);
        let mut state = 0x2114_9E37_79B9_7F4A_u64;
        for _ in 0..count {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            granules
                .push(u32::try_from(z % u64::from(FILE_GRANULES)).expect("granule index fits u32"));
        }
        granules
    }

    fn next_granule(cursor: &Cell<usize>, granules: &[u32]) -> u32 {
        let k = cursor.get();
        cursor.set(k + 1);
        granules[k % granules.len()]
    }

    fn temp_path(tag: &str) -> PathBuf {
        let mut path =
            std::option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        std::fs::create_dir_all(&path).expect("target tmp dir");
        path.push(format!("dios-{tag}-{}", std::process::id()));
        path
    }

    fn preallocated_file(path: &Path) {
        let granule = vec![0xD1_u8; GRANULE as usize];
        let file = std::fs::File::create(path).expect("create the posture-bench file");
        let mut offset = 0u64;
        for _ in 0..FILE_GRANULES {
            file.write_all_at(&granule, offset)
                .expect("write a granule");
            offset += u64::from(GRANULE);
        }
        file.sync_all().expect("fsync the preallocated file");
    }

    struct Arm {
        pool: Pool,
        file: FileId,
        cursor: Cell<usize>,
        hits: Cell<u32>,
    }

    impl Arm {
        fn build(policy: RegistrationPolicy, expected: RegistrationPosture, path: &Path) -> Self {
            let pool = Pool::builder()
                .frame_count(FRAMES)
                .granule(GRANULE)
                .max_concurrent_readers(1)
                .peak_guards_per_reader(1)
                .max_inflight_reads(1)
                .miss_headroom(3)
                .registration_posture(policy)
                .build()
                .unwrap_or_else(|error| panic!("the {policy:?} pool builds: {error}"));
            assert_eq!(
                pool.registration_posture(),
                expected,
                "the {policy:?} arm runs the posture it names"
            );
            let file = pool
                .open(path, DirectIo::Required)
                .expect("open the O_DIRECT input");
            Self {
                pool,
                file,
                cursor: Cell::new(0),
                hits: Cell::new(0),
            }
        }

        /// Admits the miss, polling through `Busy` while CLOCK reclaims a frame
        /// past the watermark; `None` is a hit already probed.
        fn admit(&self, reader: &ReaderCtx, page: PageId) -> Option<PendingToken> {
            for _ in 0..POLLS_MAX {
                match self.pool.get(reader, page).expect("the input stays live") {
                    Get::Hit(guard) => {
                        self.hits.set(self.hits.get() + 1);
                        black_box(guard[0]);
                        return None;
                    }
                    Get::Pending(token) => return Some(token),
                    Get::Busy => {
                        self.pool.poll();
                    }
                }
            }
            panic!("bounded CLOCK reclamation never admitted the miss");
        }

        fn read_one(&self, reader: &ReaderCtx, granules: &[u32]) {
            let granule = next_granule(&self.cursor, granules);
            let page = PageId::new(self.file, granule);
            let mut token = match self.admit(reader, page) {
                Some(token) => token,
                None => return,
            };
            for _ in 0..POLLS_MAX {
                match self.pool.ready(reader, token) {
                    ReadyResult::Ready(guard) => {
                        black_box(guard[0]);
                        return;
                    }
                    ReadyResult::NotYet(handed_back) => {
                        token = handed_back;
                        self.pool.poll();
                    }
                    ReadyResult::Err(error) => panic!("a full-granule read completes: {error}"),
                }
            }
            panic!("a QD1 pool read never completed within the poll budget");
        }
    }

    pub(crate) fn run() {
        let granules = build_granules();
        let path = temp_path("arena-registration");
        preallocated_file(&path);

        let registered = Arm::build(
            RegistrationPolicy::Registered,
            RegistrationPosture::Registered,
            &path,
        );
        let unregistered = Arm::build(
            RegistrationPolicy::Unregistered,
            RegistrationPosture::Unregistered,
            &path,
        );
        let registered_reader = registered.pool.register_reader().expect("one reader slot");
        let unregistered_reader = unregistered
            .pool
            .register_reader()
            .expect("one reader slot");

        let samples = dios::bench::run_paired(
            "arena_registration",
            REPS,
            ITERS_PER_REP,
            || registered.read_one(&registered_reader, &granules),
            || unregistered.read_one(&unregistered_reader, &granules),
        );

        let gate = dios::bench::ratio_gate(&samples, BOOTSTRAP_RESAMPLES);
        let out = dios::bench::write_samples(Path::new("target/bench-samples"), &samples)
            .expect("write samples CSV");
        println!(
            "arena_registration: pairs {REPS}, ratio geomean {:.4}, ci95 upper {:.4}, hits registered {} unregistered {}, arena locked registered {} unregistered {}, samples {}",
            gate.ratio_geomean,
            gate.ratio_ci95_upper,
            registered.hits.get(),
            unregistered.hits.get(),
            registered.pool.arena_locked(),
            unregistered.pool.arena_locked(),
            out.display()
        );

        drop(registered_reader);
        drop(unregistered_reader);
        drop(registered);
        drop(unregistered);
        let _ = std::fs::remove_file(&path);
    }
}
