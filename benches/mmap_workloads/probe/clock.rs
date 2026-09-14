use super::{GROUPS, Method, POLLS_MAX, Probe, error, io, nanoseconds};
use serde::Serialize;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Default, Serialize)]
struct GroupTiming {
    group: u32,
    submit_ns: u64,
    ready_wait_ns: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct Timings {
    groups: Vec<GroupTiming>,
}

impl Timings {
    fn new() -> Result<Self, String> {
        let mut groups = Vec::new();
        groups.try_reserve_exact(GROUPS as usize).map_err(error)?;
        groups.resize_with(GROUPS as usize, GroupTiming::default);
        assert_eq!(groups.len(), GROUPS as usize);
        Ok(Self { groups })
    }

    fn run(&mut self, probe: &mut Probe, method: Method) -> io::Result<()> {
        assert_eq!(probe.slots.len(), 1);
        assert_eq!(method.requests(), 1);
        let mut next = 0;
        probe.refill(method, &mut next)?;
        for group in 0..GROUPS {
            assert_eq!(next, group + 1);
            assert_eq!(probe.ring.submission().len(), 1);
            let started = Instant::now();
            let submitted = probe.ring.submit();
            let returned = Instant::now();
            probe.counters.enter_calls += 1;
            let submitted = submitted?;
            probe.counters.submitted += u64::try_from(submitted).expect("SQ count");
            if submitted != 1 {
                return Err(io::Error::other("clock split requires one accepted SQE"));
            }
            assert!(probe.ring.submission().is_empty());
            Self::run_wait(probe, method)?;
            let ready = Instant::now();
            self.groups[group as usize] = GroupTiming {
                group,
                submit_ns: nanoseconds(returned.duration_since(started)),
                ready_wait_ns: nanoseconds(ready.duration_since(returned)),
            };
            assert_eq!(probe.counters.groups_completed, group + 1);
            if let Some(code) = probe.failure {
                return Err(io::Error::from_raw_os_error(code));
            }
            probe.refill(method, &mut next)?;
        }
        assert_eq!(probe.counters.groups_consumed, GROUPS);
        assert_eq!(probe.groups_pending, 0);
        assert!(probe.ring.submission().is_empty());
        assert!(probe.ring.completion().is_empty());
        Ok(())
    }

    fn run_wait(probe: &mut Probe, method: Method) -> io::Result<()> {
        assert_eq!(probe.outstanding, 1);
        for _ in 0..POLLS_MAX {
            probe.poll(method)?;
            if probe.outstanding == 0 {
                return Ok(());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "clock split wait exhausted",
        ))
    }
}

pub(super) fn sample(input: &Path, method: &str, output: &Path) -> Result<(), String> {
    match Method::parse(method)? {
        Method::Vectored | Method::Contiguous => {}
        Method::Scattered | Method::VectoredAdjacent => {
            return Err("clock split requires scattered READV or contiguous READ".to_owned());
        }
    }
    let mut timings = Timings::new()?;
    super::sample_using(input, method, "1", output, move |probe, parsed| {
        timings.run(probe, parsed)?;
        Ok(Some(timings))
    })
}
