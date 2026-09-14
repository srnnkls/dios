use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

use io_uring::{IoUring, opcode, squeue, types};
use serde::Serialize;
use serde_json::json;

use super::catalog::{Arm, GRANULE, Lane, PAGES, POLLS_MAX, PageNumber, consume, word};
use super::fixture::{self, error};
use super::observe::{allocations_begin, allocations_end, nanoseconds};
use super::os;

mod clock;

const GROUP_PAGES: u32 = 32;
const GROUP_BYTES: u32 = GROUP_PAGES * GRANULE;
const GROUPS: u32 = 3 * PAGES / GROUP_PAGES;
const BUFFER_PAGES: u32 = 3 * GROUP_PAGES;
pub(super) const METHODS: [&str; 4] = [
    "read_scattered",
    "read_vectored",
    "read_contiguous",
    "read_vectored_adjacent",
];

#[derive(Clone, Copy)]
struct Depth(u32);

impl Depth {
    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "1" => Ok(Self(1)),
            "2" => Ok(Self(2)),
            "4" => Ok(Self(4)),
            "8" => Ok(Self(8)),
            "16" => Ok(Self(16)),
            _ => Err("probe depth must be 1, 2, 4, 8 or 16 groups".to_owned()),
        }
    }

    const fn ring_entries(self) -> u32 {
        self.0 * GROUP_PAGES * 2
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    Scattered,
    Vectored,
    Contiguous,
    VectoredAdjacent,
}

impl Method {
    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "read_scattered" => Ok(Self::Scattered),
            "read_vectored" => Ok(Self::Vectored),
            "read_contiguous" => Ok(Self::Contiguous),
            "read_vectored_adjacent" => Ok(Self::VectoredAdjacent),
            _ => Err("unknown unregistered read probe".to_owned()),
        }
    }

    const fn requests(self) -> u32 {
        match self {
            Self::Scattered => GROUP_PAGES,
            Self::Vectored | Self::Contiguous | Self::VectoredAdjacent => 1,
        }
    }
}

#[repr(align(4096))]
struct Page([u8; GRANULE as usize]);

struct Buffers {
    pages: Vec<Page>,
    vectors: Vec<libc::iovec>,
    adjacent: Vec<libc::iovec>,
}

impl Buffers {
    fn new() -> Result<Self, String> {
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(BUFFER_PAGES as usize)
            .map_err(error)?;
        pages.resize_with(BUFFER_PAGES as usize, || Page([0; GRANULE as usize]));
        assert_eq!(size_of::<Page>(), GRANULE as usize);
        let mut vectors = Vec::new();
        let mut adjacent = Vec::new();
        vectors
            .try_reserve_exact(GROUP_PAGES as usize)
            .map_err(error)?;
        adjacent
            .try_reserve_exact(GROUP_PAGES as usize)
            .map_err(error)?;
        let base = pages.as_mut_ptr().cast::<u8>();
        for index in 0..GROUP_PAGES as usize {
            let pointer = base.wrapping_add((GROUP_PAGES as usize + index * 2) * GRANULE as usize);
            assert_eq!(pointer.addr() % GRANULE as usize, 0);
            vectors.push(libc::iovec {
                iov_base: pointer.cast(),
                iov_len: GRANULE as usize,
            });
            adjacent.push(libc::iovec {
                iov_base: base.wrapping_add(index * GRANULE as usize).cast(),
                iov_len: GRANULE as usize,
            });
        }
        Ok(Self {
            pages,
            vectors,
            adjacent,
        })
    }

    fn entry(&mut self, method: Method, offset: u64, slot: u32) -> squeue::Entry {
        assert!(slot < method.requests());
        match method {
            Method::Scattered => opcode::Read::new(
                types::Fixed(0),
                self.vectors[slot as usize].iov_base.cast(),
                GRANULE,
            )
            .offset(offset + u64::from(slot * GRANULE))
            .build(),
            Method::Vectored => {
                opcode::Readv::new(types::Fixed(0), self.vectors.as_ptr(), GROUP_PAGES)
                    .offset(offset)
                    .build()
            }
            Method::Contiguous => {
                opcode::Read::new(types::Fixed(0), self.pages.as_mut_ptr().cast(), GROUP_BYTES)
                    .offset(offset)
                    .build()
            }
            Method::VectoredAdjacent => {
                opcode::Readv::new(types::Fixed(0), self.adjacent.as_ptr(), GROUP_PAGES)
                    .offset(offset)
                    .build()
            }
        }
    }

    fn consume(&self, method: Method, first_page: u32) -> u64 {
        assert!(first_page + GROUP_PAGES <= PAGES);
        (0..GROUP_PAGES).fold(0_u64, |sum, index| {
            let frame = match method {
                Method::Scattered | Method::Vectored => GROUP_PAGES + index * 2,
                Method::Contiguous | Method::VectoredAdjacent => index,
            };
            let bytes = &self.pages[frame as usize].0;
            let first = u64::from_le_bytes(bytes[..8].try_into().expect("first word"));
            assert_eq!(first, word(PageNumber(first_page + index), 0));
            sum.wrapping_add(black_box(consume(black_box(bytes))))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Free,
    Reading { group: u32, seen: u32 },
    Ready { group: u32 },
}

impl SlotState {
    fn start(&mut self, group: u32) {
        assert_eq!(*self, Self::Free, "consume the previous group before reuse");
        assert!(group < GROUPS);
        *self = Self::Reading { group, seen: 0 };
    }

    fn complete(&mut self, group: u32, request: u32, requests: u32) -> bool {
        assert!(matches!(requests, 1 | GROUP_PAGES));
        let Self::Reading {
            group: expected,
            seen,
        } = self
        else {
            panic!("completion requires an active slot");
        };
        assert_eq!(
            group, *expected,
            "completion belongs to this slot generation"
        );
        assert!(request < requests);
        let bit = 1_u32 << request;
        assert_eq!(*seen & bit, 0, "completion observed once");
        *seen |= bit;
        if seen.count_ones() == requests {
            *self = Self::Ready { group };
            true
        } else {
            false
        }
    }

    fn release(&mut self) {
        assert!(matches!(*self, Self::Ready { .. }));
        *self = Self::Free;
    }
}

struct GroupSlot {
    buffers: Buffers,
    state: SlotState,
}

#[derive(Default, Serialize)]
struct Counters {
    submitted: u64,
    completed: u64,
    read_bytes: u64,
    enter_calls: u64,
    poll_calls: u64,
    outstanding_max: u32,
    groups_max: u32,
    groups_completed: u32,
    groups_consumed: u32,
    refills_while_pending: u32,
    groups_at_poll: [u64; 17],
    allocations: u64,
    checksum: u64,
}

struct Probe {
    ring: IoUring,
    slots: Vec<GroupSlot>,
    outstanding: u32,
    groups_pending: u32,
    failure: Option<i32>,
    counters: Counters,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.outstanding != 0 {
            // Returning kernel-visible storage to the allocator would violate ownership.
            std::process::abort();
        }
    }
}

impl Probe {
    fn new(file: &fs::File, depth: Depth) -> Result<Self, String> {
        let mut slots = Vec::new();
        slots.try_reserve_exact(depth.0 as usize).map_err(error)?;
        for _ in 0..depth.0 {
            slots.push(GroupSlot {
                buffers: Buffers::new()?,
                state: SlotState::Free,
            });
        }
        let ring = IoUring::new(depth.ring_entries()).map_err(error)?;
        ring.submitter()
            .register_files(&[file.as_raw_fd()])
            .map_err(error)?;
        Ok(Self {
            ring,
            slots,
            outstanding: 0,
            groups_pending: 0,
            failure: None,
            counters: Counters::default(),
        })
    }

    fn stage(&mut self, method: Method, group: u32, index: usize) -> io::Result<()> {
        assert!(index < self.slots.len());
        let slot = &mut self.slots[index];
        slot.state.start(group);
        let count = method.requests();
        let mut submission = self.ring.submission();
        assert!(submission.capacity() - submission.len() >= count as usize);
        self.outstanding += count;
        self.groups_pending += 1;
        self.counters.outstanding_max = self.counters.outstanding_max.max(self.outstanding);
        self.counters.groups_max = self.counters.groups_max.max(self.groups_pending);
        let first_page = group * GROUP_PAGES % PAGES;
        for request in 0..count {
            let token = (u64::from(group) << 32)
                | (u64::try_from(index).expect("bounded group slot") << 5)
                | u64::from(request);
            let entry = slot
                .buffers
                .entry(method, u64::from(first_page) * u64::from(GRANULE), request)
                .user_data(token);
            // SAFETY: destinations and iovecs have separate, stable allocations
            // untouched by mutable slot bookkeeping until every CQE is reaped.
            // Probe aborts instead of freeing outstanding I/O.
            unsafe { submission.push(&entry) }.map_err(|_| io::Error::other("probe SQ full"))?;
        }
        Ok(())
    }

    fn poll(&mut self, method: Method) -> io::Result<()> {
        let expected = i32::try_from(GROUP_BYTES / method.requests()).expect("bounded read");
        self.counters.enter_calls += 1;
        match self.ring.submit() {
            Ok(count) => self.counters.submitted += u64::try_from(count).expect("SQ count"),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(error) => return Err(error),
        }
        self.counters.poll_calls += 1;
        self.counters.groups_at_poll[self.groups_pending as usize] += 1;
        for completion in self.ring.completion() {
            let token = completion.user_data();
            let group = u32::try_from(token >> 32).expect("group token");
            let index = usize::try_from((token & u64::from(u32::MAX)) >> 5).expect("slot token");
            let request = u32::try_from(token & 31).expect("request token");
            let finished = self.slots[index]
                .state
                .complete(group, request, method.requests());
            if finished {
                self.groups_pending -= 1;
                self.counters.groups_completed += 1;
            }
            assert!(self.outstanding > 0);
            self.outstanding -= 1;
            self.counters.completed += 1;
            let result = completion.result();
            if result >= 0 {
                self.counters.read_bytes += u64::try_from(result).expect("positive bytes");
            }
            if result != expected {
                self.failure = Some(if result < 0 { -result } else { libc::EIO });
            }
        }
        Ok(())
    }

    fn refill(&mut self, method: Method, next: &mut u32) -> io::Result<()> {
        let pending = self.groups_pending;
        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            if let SlotState::Ready { group } = slot.state {
                if self.failure.is_none() {
                    let first_page = group * GROUP_PAGES % PAGES;
                    self.counters.checksum = self
                        .counters
                        .checksum
                        .wrapping_add(slot.buffers.consume(method, first_page));
                    self.counters.groups_consumed += 1;
                }
                slot.state.release();
            }
            if slot.state != SlotState::Free {
                continue;
            }
            if self.failure.is_some() {
                continue;
            }
            if *next == GROUPS {
                continue;
            }
            assert!(*next < GROUPS);
            if pending > 0 {
                assert!(*next >= u32::try_from(self.slots.len()).expect("bounded depth"));
                self.counters.refills_while_pending += 1;
            }
            self.stage(method, *next, index)?;
            *next += 1;
        }
        Ok(())
    }

    #[inline(never)]
    fn run(&mut self, method: Method) -> io::Result<()> {
        let mut next = 0;
        let mut stalled = 0;
        self.refill(method, &mut next)?;
        let limit = u64::from(GROUPS * method.requests()) * u64::from(POLLS_MAX);
        for _ in 0..limit {
            let completed = self.counters.completed;
            let groups = self.counters.groups_completed;
            self.poll(method)?;
            if completed == self.counters.completed {
                stalled += 1;
                if stalled == POLLS_MAX {
                    break;
                }
            } else {
                stalled = 0;
            }
            if groups != self.counters.groups_completed {
                self.refill(method, &mut next)?;
            }
            if self.outstanding == 0 {
                assert_eq!(self.groups_pending, 0);
                assert!(self.ring.submission().is_empty());
                assert!(self.ring.completion().is_empty());
                if let Some(code) = self.failure {
                    return Err(io::Error::from_raw_os_error(code));
                }
                assert_eq!(next, GROUPS);
                assert_eq!(self.counters.groups_consumed, GROUPS);
                return Ok(());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "bounded probe drain exhausted",
        ))
    }
}

fn alignment(file: &fs::File) -> Result<(u32, u32), String> {
    let mut value = std::mem::MaybeUninit::<libc::statx>::uninit();
    // SAFETY: the fd is live, the empty path is NUL-terminated and value is writable.
    let status = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_DIOALIGN,
            value.as_mut_ptr(),
        )
    };
    if status != 0 {
        return Err(error(io::Error::last_os_error()));
    }
    // SAFETY: successful statx initialized its output.
    let value = unsafe { value.assume_init() };
    let memory = value.stx_dio_mem_align;
    let offset = value.stx_dio_offset_align;
    if value.stx_mask & libc::STATX_DIOALIGN == 0 || memory == 0 || offset == 0 {
        return Err("probe needs a statx direct-I/O alignment witness".to_owned());
    }
    if !GRANULE.is_multiple_of(memory) || !GRANULE.is_multiple_of(offset) {
        return Err("probe pages do not satisfy direct-I/O alignment".to_owned());
    }
    Ok((memory, offset))
}

pub(super) fn sample(input: &Path, method: &str, depth: &str, output: &Path) -> Result<(), String> {
    sample_using(input, method, depth, output, |probe, method| {
        probe.run(method).map(|()| None)
    })
}

pub(super) fn clock_sample(input: &Path, method: &str, output: &Path) -> Result<(), String> {
    clock::sample(input, method, output)
}

fn sample_using(
    input: &Path,
    method: &str,
    depth: &str,
    output: &Path,
    run: impl FnOnce(&mut Probe, Method) -> io::Result<Option<clock::Timings>>,
) -> Result<(), String> {
    let parsed = Method::parse(method)?;
    let depth = Depth::parse(depth)?;
    os::pin(0).map_err(error)?;
    let mapping = os::Mapping::open(&input.join("pages.bin")).map_err(error)?;
    let cache =
        fixture::prepare(&mapping, Lane::PressureScan, Arm::MmapSequential, 0).map_err(error)?;
    let expected = fixture::expected(Lane::PressureScan, 0);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(input.join("pages.bin"))
        .map_err(error)?;
    let (memory_alignment, offset_alignment) = alignment(&file)?;
    let mut probe = Probe::new(&file, depth)?;
    os::cpu_ns().map_err(error)?;
    allocations_begin();
    assert_eq!(allocations_end(), 0);
    let before = super::snapshot();
    let usage = os::usage().map_err(error)?;
    let cpu = os::cpu_ns().map_err(error)?;
    let started = Instant::now();
    allocations_begin();
    let result = run(&mut probe, parsed);
    probe.counters.allocations = allocations_end();
    let elapsed_ns = nanoseconds(started.elapsed());
    let cpu_ns = os::cpu_ns().map_err(error)? - cpu;
    let usage = os::usage().map_err(error)?.since(usage);
    if probe.outstanding != 0 {
        eprintln!("probe cannot return kernel-visible storage: {result:?}");
        std::process::abort();
    }
    let diagnostic = result.map_err(error)?;
    assert_eq!(probe.counters.checksum, expected);
    assert_eq!(probe.counters.allocations, 0);
    assert_eq!(probe.counters.submitted, probe.counters.completed);
    let row = json!({"schema": 2, "kind": "readv_probe", "method": method, "depth": depth.0,
        "runner_sha256": super::runner_hash(), "debug_assertions": cfg!(debug_assertions),
        "groups": GROUPS, "group_bytes": GROUP_BYTES, "pages": GROUPS * GROUP_PAGES,
        "useful_bytes": u64::from(GROUPS) * u64::from(GROUP_BYTES),
        "elapsed_ns": elapsed_ns, "cpu_ns": cpu_ns, "usage": usage, "diagnostic": diagnostic,
        "counters": probe.counters, "outstanding_end": probe.outstanding,
        "buffer_bytes": depth.0 * BUFFER_PAGES * GRANULE, "registration": "Unregistered",
        "group_bytes_max": depth.0 * GROUP_BYTES,
        "file_registration": "Fixed", "io_mode": "Direct", "ring_entries": depth.ring_entries(),
        "memory_alignment": memory_alignment, "offset_alignment": offset_alignment,
        "expected_checksum": expected, "cache_before": cache,
        "system_before": before, "system_after": super::snapshot()});
    let destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(error)?;
    serde_json::to_writer(destination, &row).map_err(error)
}

pub(super) fn profile(
    input: &Path,
    method: &str,
    depth: &str,
    repetitions: &str,
    output: &Path,
) -> Result<(), String> {
    let repetitions: u32 = repetitions.parse().map_err(error)?;
    if !(1..=32).contains(&repetitions) {
        return Err("probe profiles require 1..32 repetitions".to_owned());
    }
    fs::create_dir(output).map_err(error)?;
    for index in 0..repetitions {
        sample(
            input,
            method,
            depth,
            &output.join(format!("{index:04}.json")),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn adjacent_iovecs_are_a_distinct_probe_arm() {
        use super::Method;

        let method = Method::parse("read_vectored_adjacent").expect("adjacent READV arm");
        assert_eq!(method.requests(), 1);
    }

    #[test]
    fn slot_waits_for_every_subrequest_before_reuse() {
        use super::{GROUP_PAGES, SlotState};

        let mut state = SlotState::Free;
        state.start(5);
        for request in (1..GROUP_PAGES).rev() {
            assert!(!state.complete(5, request, GROUP_PAGES));
        }
        assert!(state.complete(5, 0, GROUP_PAGES));
        assert_eq!(state, SlotState::Ready { group: 5 });
        state.release();
        state.start(19);
        assert!(state.complete(19, 0, 1));
    }

    #[test]
    #[should_panic(expected = "completion observed once")]
    fn duplicate_subrequest_cannot_publish_a_group() {
        use super::{GROUP_PAGES, SlotState};

        let mut state = SlotState::Free;
        state.start(5);
        assert!(!state.complete(5, 7, GROUP_PAGES));
        state.complete(5, 7, GROUP_PAGES);
    }

    #[test]
    #[should_panic(expected = "slot generation")]
    fn old_completion_cannot_complete_a_reused_slot() {
        use super::SlotState;

        let mut state = SlotState::Free;
        state.start(5);
        assert!(state.complete(5, 0, 1));
        state.release();
        state.start(19);
        state.complete(5, 0, 1);
    }

    #[test]
    #[should_panic(expected = "before reuse")]
    fn completed_data_cannot_be_overwritten_before_consumption() {
        use super::SlotState;

        let mut state = SlotState::Free;
        state.start(5);
        assert!(state.complete(5, 0, 1));
        state.start(19);
    }

    #[test]
    fn layout_and_depth_have_fixed_bounds() {
        use super::{Buffers, Depth, GROUP_PAGES};

        assert!(Depth::parse("0").is_err());
        assert!(Depth::parse("3").is_err());
        assert!(Depth::parse("17").is_err());
        for (depth, entries) in [("2", 128), ("4", 256)] {
            assert_eq!(
                Depth::parse(depth)
                    .expect("owner budget probe depth")
                    .ring_entries(),
                entries
            );
        }
        assert_eq!(
            Depth::parse("16").expect("bounded depth").ring_entries(),
            1024
        );
        let first = Buffers::new().expect("first slot");
        let second = Buffers::new().expect("second slot");
        for index in 0..GROUP_PAGES as usize {
            assert_eq!(
                first.adjacent[index].iov_base.addr(),
                first.pages.as_ptr().addr() + index * 4096
            );
            assert_eq!(
                first.vectors[index].iov_base.addr(),
                first.pages.as_ptr().addr() + (32 + index * 2) * 4096
            );
            assert_ne!(
                first.vectors[index].iov_base,
                second.vectors[index].iov_base
            );
        }
    }

    #[test]
    fn stored_destinations_survive_consumption_and_reuse() {
        use super::{Buffers, GROUP_PAGES, Method, PageNumber, word};

        for method in [Method::Vectored, Method::VectoredAdjacent] {
            let buffers = Buffers::new().expect("bounded buffers");
            let vectors = match method {
                Method::Vectored => &buffers.vectors,
                Method::VectoredAdjacent => &buffers.adjacent,
                _ => unreachable!("test covers both iovec layouts"),
            };
            for first_page in [0, GROUP_PAGES] {
                let mut expected = 0_u64;
                for (index, vector) in vectors.iter().enumerate() {
                    let value = word(
                        PageNumber(first_page + u32::try_from(index).expect("page")),
                        0,
                    );
                    // SAFETY: each stored destination belongs to the live buffer
                    // allocation, has room for a u64 and no I/O is in flight.
                    unsafe { vector.iov_base.cast::<u64>().write(value.to_le()) };
                    expected = expected.wrapping_add(value);
                }
                assert_eq!(buffers.consume(method, first_page), expected);
            }
        }
    }
}
