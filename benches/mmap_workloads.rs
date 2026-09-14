#[cfg(target_os = "linux")]
#[path = "mmap_workloads/mod.rs"]
mod mmap_workloads;

fn main() -> std::process::ExitCode {
    let arguments: Vec<_> = std::env::args()
        .skip(1)
        .filter(|argument| argument != "--bench")
        .collect();
    if arguments
        .first()
        .is_some_and(|value| value == "mechanism-capture")
    {
        return match mechanisms::run(&arguments[1..]) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("mechanism capture: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    #[cfg(target_os = "linux")]
    {
        match mmap_workloads::run(&arguments) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("mmap workloads: {error}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "mmap fault/cache-pressure comparisons require Linux; see benches/plans/mmap_workloads.md"
        );
        std::process::ExitCode::FAILURE
    }
}

mod mechanisms {
    use std::fs::{File, OpenOptions};
    use std::path::Path;
    use std::sync::Arc;

    use dios::testing::{
        PoolBuilderObservationExt, PoolTestingExt, ReadObservation, ReadObservationConfig,
    };
    use dios::{
        DirectIo, FileId, Get, PageId, PendingToken, Pool, PrefetchReport, PrefetchStats,
        Readahead, ReaderCtx, ReadyResult, RegistrationPolicy, RetireStatus,
    };
    use serde_json::{Value, json};

    const GRANULE: u32 = 4096;
    const INPUT_PAGES: u32 = 4096;
    const POLLS_MAX: u32 = 16_384;
    const CONSUMED_PAGES_MAX: usize = 2048;
    const STAGES_MAX: usize = 8;

    struct Scenario {
        pool: Pool,
        file: FileId,
        readers: Vec<ReaderCtx>,
        capture: Arc<ReadObservation>,
        consumed_pages: Vec<u32>,
        checksum: u64,
        polls: u32,
        stages: Vec<Value>,
        credits: u32,
        read_limit: u32,
        frame_count: u32,
        io_mode: String,
        registration: String,
    }

    impl Scenario {
        fn new(
            input: &Path,
            credits: u32,
            read_limit: u32,
            readers: u32,
            frame_count: u32,
            mode: Readahead,
        ) -> Result<Self, String> {
            assert!((1..=256).contains(&credits));
            assert!((1..=2).contains(&readers));
            let pool = Pool::builder()
                .granule(GRANULE)
                .frame_count(frame_count)
                .max_concurrent_readers(readers)
                .peak_guards_per_reader(1)
                .max_inflight_reads(read_limit)
                .miss_headroom(3 * read_limit)
                .max_retained_frames(0)
                .registered_file_capacity(1)
                .write_slots(1)
                .registration_posture(RegistrationPolicy::Unregistered)
                .prefetch_headroom(credits)
                .readahead(mode)
                .read_observation(ReadObservationConfig {
                    event_capacity: 65_536,
                    interval_start_page: 0,
                    interval_pages: 0,
                    consumer_stop_bytes: None,
                })
                .build()
                .map_err(error)?;
            let file = pool.open(input, DirectIo::Required).map_err(error)?;
            let io_mode = format!("{:?}", pool.io_mode(file).expect("registered file"));
            let registration = format!("{:?}", pool.registration_posture());
            let capture = pool.read_observation().expect("configured capture");
            let readers = (0..readers)
                .map(|_| pool.register_reader().map_err(error))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Self {
                pool,
                file,
                readers,
                capture,
                credits,
                read_limit,
                frame_count,
                io_mode,
                registration,
                consumed_pages: Vec::with_capacity(CONSUMED_PAGES_MAX),
                checksum: 0,
                polls: 0,
                stages: Vec::with_capacity(STAGES_MAX),
            })
        }

        fn stage(&mut self, name: &str) -> PrefetchStats {
            assert!(self.stages.len() < STAGES_MAX);
            let stats = self.pool.prefetch_stats();
            assert_eq!(stats.capacity, self.credits);
            self.stages.push(json!({
                "stage": name, "polls": self.polls, "prefetch": prefetch_stats(stats),
                "observation": self.capture.snapshot(),
            }));
            stats
        }

        fn consume(&mut self, reader: u32, page: u32) -> Result<bool, String> {
            assert!(page < INPUT_PAGES);
            let reader = &self.readers[reader as usize];
            let (sum, hit) = match self
                .pool
                .get(reader, PageId::new(self.file, page))
                .map_err(error)?
            {
                Get::Hit(guard) => (checksum(&guard), true),
                Get::Pending(token) => (
                    consume_pending(&self.pool, reader, token, &mut self.polls)?,
                    false,
                ),
                Get::Busy => return Err("mechanism demand unexpectedly busy".to_owned()),
            };
            self.record_consumption(page, sum);
            Ok(hit)
        }

        fn start(&self, reader: u32, page: u32) -> Result<PendingToken, String> {
            assert!(page < INPUT_PAGES);
            match self
                .pool
                .get(&self.readers[reader as usize], PageId::new(self.file, page))
                .map_err(error)?
            {
                Get::Pending(token) => Ok(token),
                Get::Hit(_) => Err("confirmation demand unexpectedly resident".to_owned()),
                Get::Busy => Err("confirmation demand unexpectedly busy".to_owned()),
            }
        }

        fn finish(&mut self, reader: u32, token: PendingToken) -> Result<(), String> {
            let page = token.page().granule_idx();
            let sum = consume_pending(
                &self.pool,
                &self.readers[reader as usize],
                token,
                &mut self.polls,
            )?;
            self.record_consumption(page, sum);
            Ok(())
        }

        fn record_consumption(&mut self, page: u32, sum: u64) {
            assert!(self.consumed_pages.len() < CONSUMED_PAGES_MAX);
            assert!(page < INPUT_PAGES);
            self.consumed_pages.push(page);
            self.checksum = self.checksum.wrapping_add(sum);
        }

        fn poll(&mut self) {
            poll(&self.pool, &mut self.polls);
        }

        fn drain(&mut self) -> Result<(), String> {
            for _ in 0..POLLS_MAX {
                if self.pool.prefetch_stats().reads_in_flight == 0 {
                    return Ok(());
                }
                self.poll();
            }
            Err("mechanism reads exceeded bounded drain".to_owned())
        }

        fn hint(&self, pages: &[u32]) -> PrefetchReport {
            assert!(pages.len() <= 256);
            let mut hints = [PageId::new(self.file, 0); 256];
            for (&page, destination) in pages.iter().zip(&mut hints) {
                assert!(page < INPUT_PAGES);
                *destination = PageId::new(self.file, page);
            }
            self.pool.prefetch(&hints[..pages.len()])
        }

        fn prefill(&mut self, pages: &[u32]) -> Result<(), String> {
            assert!(pages.len() <= self.credits as usize);
            for _ in 0..=self.credits {
                let report = self.hint(pages);
                if report.rejected > 0 {
                    return Err("mechanism setup hint rejected".to_owned());
                }
                self.drain()?;
                if self.pool.prefetch_stats().occupied as usize == pages.len() {
                    return Ok(());
                }
            }
            Err("mechanism setup did not fill its bounded credits".to_owned())
        }

        fn document(&self, name: &str) -> Value {
            assert!(!self.consumed_pages.is_empty());
            assert!(self.stages.len() >= 2);
            json!({
                "scenario": name, "credits": self.credits, "read_limit": self.read_limit,
                "granule": GRANULE, "reader_count": self.readers.len(), "frame_count": self.frame_count,
                "io_mode": self.io_mode, "registration": self.registration,
                "consumed_pages": self.consumed_pages,
                "consumed_bytes": self.consumed_pages.len() as u64 * u64::from(GRANULE),
                "checksum": self.checksum, "stages": self.stages,
            })
        }
    }

    fn error(value: impl std::fmt::Display) -> String {
        value.to_string()
    }

    fn checksum(bytes: &[u8]) -> u64 {
        assert_eq!(bytes.len(), GRANULE as usize);
        assert!(bytes.len().is_multiple_of(size_of::<u64>()));
        bytes.chunks_exact(8).fold(0_u64, |sum, word| {
            sum.wrapping_add(u64::from_le_bytes(word.try_into().expect("whole u64")))
        })
    }

    fn poll(pool: &Pool, polls: &mut u32) {
        pool.poll();
        *polls = polls.checked_add(1).expect("bounded mechanism polls");
        std::thread::yield_now();
    }

    fn consume_pending(
        pool: &Pool,
        reader: &ReaderCtx,
        mut token: PendingToken,
        polls: &mut u32,
    ) -> Result<u64, String> {
        for _ in 0..POLLS_MAX {
            match pool.ready(reader, token) {
                ReadyResult::Ready(guard) => return Ok(checksum(&guard)),
                ReadyResult::NotYet(next) => token = next,
                ReadyResult::Err(failure) => return Err(error(failure)),
            }
            poll(pool, polls);
        }
        Err("mechanism demand exceeded bounded progress".to_owned())
    }

    fn prefetch_stats(stats: PrefetchStats) -> Value {
        json!({
            "admitted": stats.admitted, "automatic_admitted": stats.automatic_admitted,
            "demand_promoted": stats.demand_promoted, "evicted_unused": stats.evicted_unused,
            "failed": stats.failed, "deferred": stats.deferred,
            "submission_refused": stats.submission_refused, "occupied": stats.occupied,
            "capacity": stats.capacity, "reserve_free": stats.reserve_free,
            "reads_in_flight": stats.reads_in_flight,
        })
    }

    fn small_capacity(
        input: &Path,
        credits: u32,
        read_limit: u32,
    ) -> Result<(Value, Value), String> {
        let mut scenario =
            Scenario::new(input, credits, read_limit, 1, 1024, Readahead::Automatic)?;
        let setup: Vec<_> = (1024..1024 + credits - 1).collect();
        scenario.prefill(&setup)?;
        scenario.stage("prefilled");
        scenario.consume(0, 0)?;
        scenario.consume(0, 1)?;
        let pending = scenario.start(0, 2)?;
        let before = scenario.stage("before_opportunity");
        assert_eq!(before.occupied + 1, credits);
        assert!(before.reads_in_flight < read_limit);
        assert!(before.reserve_free > 0);
        scenario.finish(0, pending)?;
        scenario.drain()?;
        let after = scenario.stage("after_opportunity");
        let observation_before = &scenario.stages[1]["observation"];
        let observation_after = &scenario.stages[2]["observation"];
        let (point_reads, vector_reads) =
            small_capacity_reads(observation_before, observation_after);
        let admitted = after.automatic_admitted - before.automatic_admitted;
        let row = json!({
            "credits": credits, "read_limit": read_limit, "granule": GRANULE,
            "admitted_pages": admitted, "point_reads": point_reads, "vector_reads": vector_reads,
            "deferred_with_available_credit": u32::from(admitted == 0),
            "available_speculative_credits": before.capacity - before.occupied,
            "available_read_credits": read_limit - before.reads_in_flight,
            "before_stage": "before_opportunity", "after_stage": "after_opportunity",
        });
        let name = format!("small_capacity_{credits}_{read_limit}");
        Ok((row, scenario.document(&name)))
    }

    fn small_capacity_reads(before: &Value, after: &Value) -> (u64, u64) {
        let begin = before["read_events"].as_array().expect("read events").len();
        let events = after["read_events"].as_array().expect("read events");
        assert!(begin <= events.len());
        let mut points = 0;
        let mut vectors = 0;
        for event in &events[begin..] {
            if event["event"] == "attempt" && event["kind"] == "speculative" {
                if event["pages"] == 1 {
                    points += 1;
                } else {
                    vectors += 1;
                }
            }
        }
        (points, vectors)
    }

    fn shared_readers(input: &Path) -> Result<(Value, Value), String> {
        let mut scenario = Scenario::new(input, 4, 64, 2, 1024, Readahead::Automatic)?;
        let setup = [2048, 2049, 2050, 2051];
        scenario.prefill(&setup)?;
        scenario.stage("prefilled");
        for reader in 0..2 {
            for page in 0..2 {
                scenario.consume(reader, reader * 512 + page)?;
            }
        }
        let first = scenario.start(0, 2)?;
        let second = scenario.start(1, 514)?;
        let before = scenario.stage("both_ready_full");
        assert_eq!(before.occupied, 4);
        scenario.poll();
        scenario.finish(0, first)?;
        scenario.finish(1, second)?;
        scenario.stage("credit_deferred");
        for page in setup {
            scenario.consume(0, page)?;
        }
        let released = scenario.stage("credits_released");
        assert_eq!(released.occupied, 0);
        scenario.poll();
        scenario.drain()?;
        scenario.stage("drained");
        let turns =
            scenario.stages.last().expect("final stage")["observation"]["shared_reader_turns"]
                .clone();
        Ok((turns, scenario.document("shared_readers")))
    }

    fn control(input: &Path, credits: u32) -> Result<(Value, Value), String> {
        let mut scenario = Scenario::new(input, credits, 64, 1, 1024, Readahead::Disabled)?;
        let pages: Vec<_> = (1024..1056).collect();
        scenario.prefill(&pages)?;
        let before = scenario.stage("after_last_completion");
        assert_eq!(before.occupied, 32);
        assert_eq!(before.reads_in_flight, 0);
        for page in pages {
            scenario.consume(0, page)?;
        }
        scenario.poll();
        scenario.poll();
        let after = scenario.stage("consumed_and_idle");
        assert_eq!(after.occupied, 0);
        assert_eq!(
            scenario.stages[0]["observation"]["io"]["cqes"],
            scenario.stages[1]["observation"]["io"]["cqes"]
        );
        let events = scenario.stages[1]["observation"]["control"].clone();
        Ok((events, scenario.document(&format!("control_{credits}"))))
    }

    fn explicit_pages(name: &str) -> Vec<u32> {
        match name {
            "duplicates" => vec![1024, 1024, 1025, 1025, 2000, 2000, 2001, 2001],
            "multiple_runs" => vec![1024, 2000, 2001, 1025, 2100, 2101, 1026, 2200, 2201],
            "newly_admitted_protected" => {
                (2000..2032).chain(2200..2232).chain(2000..2032).collect()
            }
            _ => unreachable!("three explicit scenarios"),
        }
    }

    fn explicit(input: &Path, name: &str) -> Result<(Value, Value), String> {
        let mut scenario = Scenario::new(input, 32, 64, 1, 1024, Readahead::Disabled)?;
        let setup: Vec<_> = (1024..1056).collect();
        let pages = explicit_pages(name);
        scenario.prefill(&setup)?;
        let before = scenario.stage("before_call");
        assert_eq!(before.occupied, before.capacity);
        assert_eq!(before.reads_in_flight, 0);
        let report = scenario.hint(&pages);
        scenario.stage("after_call");
        scenario.drain()?;
        for &page in &pages {
            scenario.consume(0, page)?;
        }
        scenario.drain()?;
        scenario.stage("drained");
        let calls = scenario.stages.last().expect("final stage")["observation"]["explicit_calls"]
            .as_array()
            .expect("explicit calls");
        let mut call = calls.last().expect("measured hint").clone();
        call["scenario"] = json!(name);
        let mut raw = scenario.document(name);
        raw["hint_pages"] = json!(pages);
        raw["hint_report"] = json!({
            "requested": report.requested, "resident": report.resident, "pending": report.pending,
            "admitted": report.admitted, "deferred": report.deferred, "rejected": report.rejected,
        });
        Ok((call, raw))
    }

    #[derive(serde::Serialize)]
    struct CanaryFlight {
        polls: u32,
        speculative: u32,
        reads: u32,
    }

    fn canary_flight(scenario: &Scenario, flights: &mut Vec<CanaryFlight>) {
        assert!(flights.len() < flights.capacity());
        let stats = scenario.pool.prefetch_stats();
        assert!(stats.occupied <= scenario.credits);
        assert!(stats.reads_in_flight <= scenario.read_limit);
        flights.push(CanaryFlight {
            polls: scenario.polls,
            speculative: stats.occupied,
            reads: stats.reads_in_flight,
        });
    }

    fn canary_hint(
        scenario: &mut Scenario,
        start: u32,
        flights: &mut Vec<CanaryFlight>,
    ) -> Result<(), String> {
        let pages = std::array::from_fn::<_, 4, _>(|index| {
            start + u32::try_from(index).expect("bounded hint index")
        });
        for _ in 0..128 {
            let report = scenario.hint(&pages);
            canary_flight(scenario, flights);
            if report.rejected > 0 {
                return Err("wrong-hint canary rejected an in-file page".to_owned());
            }
            scenario.poll();
            canary_flight(scenario, flights);
            if report.deferred == 0 {
                return Ok(());
            }
        }
        Err("wrong-hint canary exceeded bounded hint progress".to_owned())
    }

    fn canary_retire(
        scenario: &mut Scenario,
        flights: &mut Vec<CanaryFlight>,
    ) -> Result<(), String> {
        assert_eq!(
            scenario.pool.retire_file(scenario.file),
            RetireStatus::Retiring
        );
        for _ in 0..POLLS_MAX {
            scenario.poll();
            canary_flight(scenario, flights);
            if scenario.pool.retire_file(scenario.file) == RetireStatus::Retired {
                return Ok(());
            }
        }
        Err("wrong-hint canary exceeded bounded retirement".to_owned())
    }

    fn canary(input: &Path) -> Result<(Value, Value), String> {
        let mut scenario = Scenario::new(input, 4, 16, 1, 64, Readahead::Disabled)?;
        let mut flights = Vec::with_capacity(25 * (2 * 128 + 1) + POLLS_MAX as usize);
        for page in 0..60 {
            scenario.consume(0, page)?;
        }
        scenario.stage("hot_set_warmed");
        let mut hits = 0_u32;
        let mut misses = 0_u32;
        for start in (100..200).step_by(4) {
            canary_hint(&mut scenario, start, &mut flights)?;
            for page in 0..60 {
                if scenario.consume(0, page)? {
                    hits += 1;
                } else {
                    misses += 1;
                }
            }
            canary_flight(&scenario, &mut flights);
        }
        scenario.drain()?;
        let abandoned = scenario.stage("wrong_hints_abandoned");
        canary_retire(&mut scenario, &mut flights)?;
        let recovered = scenario.stage("file_retired");
        let observation = &scenario.stages[2]["observation"];
        let protected_evictions: u64 = observation["explicit_calls"]
            .as_array()
            .expect("explicit calls")
            .iter()
            .map(|call| {
                call["protected_evictions"]
                    .as_u64()
                    .expect("protected evictions")
            })
            .sum();
        let row = json!({
            "scenario": "wrong_hint_canary", "hot_pages": 60, "wrong_windows": 25,
            "demand_hot_hits": hits, "demand_hot_misses": misses,
            "protected_evictions": protected_evictions, "credits": 4, "read_limit": 16,
            "occupied_before_recovery": abandoned.occupied, "occupied_after_recovery": recovered.occupied,
            "reads_after_recovery": recovered.reads_in_flight, "file_retired": true,
            "admitted": recovered.admitted, "demand_promoted": recovered.demand_promoted,
            "evicted_unused": recovered.evicted_unused, "failed": recovered.failed,
            "flights": flights,
        });
        Ok((row, scenario.document("wrong_hint_canary")))
    }

    fn capture(input: &Path) -> Result<Value, String> {
        let mut small = Vec::with_capacity(5);
        let mut raw = Vec::with_capacity(13);
        for (credits, read_limit) in [(1, 64), (8, 64), (32, 64), (32, 33), (128, 33)] {
            let (row, scenario) = small_capacity(input, credits, read_limit)?;
            small.push(row);
            raw.push(scenario);
        }
        let (wrong_hint_canary, scenario) = canary(input)?;
        raw.push(scenario);
        let (turns, scenario) = shared_readers(input)?;
        raw.push(scenario);
        let mut controls = Vec::with_capacity(3);
        for credits in [32, 128, 256] {
            let (events, scenario) = control(input, credits)?;
            controls.push(events);
            raw.push(scenario);
        }
        let mut explicit_calls = Vec::with_capacity(3);
        for name in ["duplicates", "multiple_runs", "newly_admitted_protected"] {
            let (call, scenario) = explicit(input, name)?;
            explicit_calls.push(call);
            raw.push(scenario);
        }
        let control: Vec<_> = controls
            .iter()
            .flat_map(|events| events.as_array().expect("control events"))
            .collect();
        let mut overflow = 0_u64;
        let mut dropped = 0_u64;
        for scenario in &raw {
            let observation = &scenario["stages"]
                .as_array()
                .expect("stages")
                .last()
                .expect("final stage")["observation"];
            overflow += observation["overflow"].as_u64().expect("overflow count");
            dropped += observation["dropped_events"].as_u64().expect("loss count");
        }
        Ok(json!({
            "schema": 1, "overflow": overflow, "dropped_events": dropped,
            "small_capacity": small, "shared_reader_turns": turns, "control": control,
            "explicit_calls": explicit_calls, "raw_scenarios": raw,
            "wrong_hint_canary": wrong_hint_canary,
        }))
    }

    pub(super) fn run(arguments: &[String]) -> Result<(), String> {
        let [input, output] = arguments else {
            return Err("usage: mechanism-capture INPUT_FILE NEW_JSON".to_owned());
        };
        let input = Path::new(input);
        let bytes = File::open(input)
            .and_then(|file| file.metadata())
            .map_err(error)?
            .len();
        if bytes < u64::from(INPUT_PAGES) * u64::from(GRANULE) {
            return Err("mechanism capture needs at least 4096 pages of 4096 bytes".to_owned());
        }
        let document = capture(input)?;
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .map_err(error)?;
        serde_json::to_writer(output, &document).map_err(error)
    }
}
