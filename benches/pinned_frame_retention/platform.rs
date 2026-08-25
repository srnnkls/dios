#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::mem::size_of_val;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use crate::common::{ArenaEvidence, FaultDelta, Lane};
#[cfg(target_os = "linux")]
use crate::common::{display_error, parse_number};

#[cfg(target_os = "linux")]
const RUNNER_HOST: &str = "nix";
#[cfg(target_os = "linux")]
const OS_HOSTNAME: &str = "nixos";
const SAME_FRAME_CPU_TOPOLOGY: &str = concat!(
    "cpu0:package0:die0:core0:siblings0,32:llc0:shared0-3,32-35",
    ";cpu1:package0:die0:core1:siblings1,33:llc0:shared0-3,32-35",
    ";cpu2:package0:die0:core2:siblings2,34:llc0:shared0-3,32-35",
    ";cpu3:package0:die0:core3:siblings3,35:llc0:shared0-3,32-35",
    ";cpu32:package0:die0:core0:siblings0,32:llc0:shared0-3,32-35",
    ";cpu33:package0:die0:core1:siblings1,33:llc0:shared0-3,32-35",
    ";cpu34:package0:die0:core2:siblings2,34:llc0:shared0-3,32-35",
    ";cpu35:package0:die0:core3:siblings3,35:llc0:shared0-3,32-35",
);

#[derive(Clone, Debug)]
pub(crate) struct HostEvidence {
    pub(crate) cpu_set: String,
    pub(crate) governor: String,
    pub(crate) kernel: String,
    pub(crate) numa_nodes: String,
    pub(crate) numa_balancing: String,
    pub(crate) storage: String,
    pub(crate) runner_host: String,
    pub(crate) os_hostname: String,
    pub(crate) cpu_model: String,
    pub(crate) cpu_topology: String,
    pub(crate) memlock_soft: u64,
    pub(crate) memlock_hard: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct TimeVal {
    seconds: std::ffi::c_long,
    microseconds: std::ffi::c_long,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct RUsage {
    user: TimeVal,
    system: TimeVal,
    max_rss: std::ffi::c_long,
    shared_text: std::ffi::c_long,
    unshared_data: std::ffi::c_long,
    unshared_stack: std::ffi::c_long,
    minor_faults: std::ffi::c_long,
    major_faults: std::ffi::c_long,
    swaps: std::ffi::c_long,
    input_blocks: std::ffi::c_long,
    output_blocks: std::ffi::c_long,
    messages_sent: std::ffi::c_long,
    messages_received: std::ffi::c_long,
    signals: std::ffi::c_long,
    voluntary_switches: std::ffi::c_long,
    involuntary_switches: std::ffi::c_long,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn getrusage(who: std::ffi::c_int, usage: *mut RUsage) -> std::ffi::c_int;
    fn sched_setaffinity(
        pid: std::ffi::c_int,
        set_size: usize,
        mask: *const std::ffi::c_void,
    ) -> std::ffi::c_int;
}

#[cfg(target_os = "linux")]
pub(crate) fn thread_faults() -> Result<FaultDelta, String> {
    let mut usage = std::mem::MaybeUninit::<RUsage>::uninit();
    // SAFETY: getrusage initializes the complete Linux RUsage layout on success.
    let result = unsafe { getrusage(1, usage.as_mut_ptr()) };
    if result != 0 {
        return Err(format!(
            "getrusage RUSAGE_THREAD: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: a successful getrusage initialized the value.
    let usage = unsafe { usage.assume_init() };
    Ok(FaultDelta {
        minor: u64::try_from(usage.minor_faults).map_err(display_error)?,
        major: u64::try_from(usage.major_faults).map_err(display_error)?,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn thread_faults() -> Result<FaultDelta, String> {
    let _ = std::thread::available_parallelism().map_err(|error| error.to_string())?;
    Ok(FaultDelta::default())
}

pub(crate) fn fault_delta(before: FaultDelta, after: FaultDelta) -> Result<FaultDelta, String> {
    Ok(FaultDelta {
        minor: after
            .minor
            .checked_sub(before.minor)
            .ok_or_else(|| "thread minor-fault counter moved backwards".to_owned())?,
        major: after
            .major
            .checked_sub(before.major)
            .ok_or_else(|| "thread major-fault counter moved backwards".to_owned())?,
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn pin_current(cpu: u32) -> Result<(), String> {
    let mut mask = [0_u64; 16];
    let word = usize::try_from(cpu / 64).map_err(display_error)?;
    if word >= mask.len() {
        return Err(format!("CPU {cpu} exceeds the fixed affinity mask"));
    }
    mask[word] = 1_u64 << (cpu % 64);
    // SAFETY: mask addresses its full initialized byte range for the syscall.
    let result = unsafe {
        sched_setaffinity(
            0,
            size_of_val(&mask),
            mask.as_ptr().cast::<std::ffi::c_void>(),
        )
    };
    if result != 0 {
        return Err(format!(
            "pin current thread to CPU {cpu}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let observed = observed_affinity()?;
    if observed != cpu.to_string() {
        return Err(format!(
            "thread affinity is {observed:?}, expected CPU {cpu}"
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pin_current(_cpu: u32) -> Result<(), String> {
    let _ = std::thread::available_parallelism().map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn observed_affinity() -> Result<String, String> {
    fs::read_to_string("/proc/thread-self/status")
        .map_err(display_error)?
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:").map(str::trim))
        .map(str::to_owned)
        .ok_or_else(|| "Linux did not report Cpus_allowed_list".to_owned())
}

#[cfg(target_os = "linux")]
pub(crate) fn binding_host(lane: Lane, input: &Path) -> Result<HostEvidence, String> {
    let expected = std::env::var("DIOS_PFR_CPU_SET")
        .map_err(|_| "binding runner requires DIOS_PFR_CPU_SET".to_owned())?;
    if expected != lane.cpu_set() || observed_affinity()? != expected {
        return Err(format!(
            "process affinity does not match frozen set {:?}",
            lane.cpu_set()
        ));
    }
    let governor = governor_for_set(&expected)?;
    let (memlock_soft, memlock_hard) = memlock_limits()?;
    let evidence = HostEvidence {
        cpu_set: expected,
        governor,
        kernel: read_trimmed("/proc/sys/kernel/osrelease")?,
        numa_nodes: read_trimmed("/sys/devices/system/node/online")?,
        numa_balancing: read_trimmed("/proc/sys/kernel/numa_balancing")?,
        storage: storage_identity(input)?,
        runner_host: RUNNER_HOST.to_owned(),
        os_hostname: read_trimmed("/etc/hostname")?,
        cpu_model: cpu_model()?,
        cpu_topology: cpu_topology(lane)?,
        memlock_soft,
        memlock_hard,
    };
    validate_binding_host(lane, &evidence)?;
    Ok(evidence)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn binding_host(_lane: Lane, _input: &Path) -> Result<HostEvidence, String> {
    Err("binding PFR runs require Linux host nix".to_owned())
}

pub(crate) fn validate_recorded_topology(lane: Lane, topology: &str) -> Result<(), String> {
    if topology.is_empty() {
        return Err("binding CPU topology is empty".to_owned());
    }
    if lane == Lane::SameFramePromotion && topology != SAME_FRAME_CPU_TOPOLOGY {
        return Err("same-frame CPUs do not share the frozen SMT and LLC topology".to_owned());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_binding_host(lane: Lane, host: &HostEvidence) -> Result<(), String> {
    validate_recorded_topology(lane, &host.cpu_topology)?;
    if host.governor != "performance"
        || host.kernel != "6.6.64"
        || host.memlock_soft != 8_388_608
        || host.memlock_hard != 8_388_608
        || !host.cpu_model.contains("AMD Ryzen Threadripper 3970X")
        || host.cpu_topology.is_empty()
        || !host.storage.contains("Samsung SSD 970 PRO")
        || host.runner_host != RUNNER_HOST
        || host.os_hostname != OS_HOSTNAME
    {
        return Err(
            "host, kernel, storage, CPU, or memlock identity is not the frozen nix profile"
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn governor_for_set(cpu_set: &str) -> Result<String, String> {
    let cpus = expand_cpu_set(cpu_set)?;
    for cpu in cpus {
        let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor");
        let governor = read_trimmed(&path)?;
        if governor != "performance" {
            return Ok(governor);
        }
    }
    Ok("performance".to_owned())
}

#[cfg(target_os = "linux")]
fn expand_cpu_set(value: &str) -> Result<Vec<u32>, String> {
    let mut cpus = Vec::new();
    for group in value.split(',') {
        if let Some((first, last)) = group.split_once('-') {
            let first: u32 = parse_number(first, "CPU-set start")?;
            let last: u32 = parse_number(last, "CPU-set end")?;
            if first > last || last > 1023 {
                return Err(format!("invalid CPU-set range {group:?}"));
            }
            cpus.extend(first..=last);
        } else {
            cpus.push(parse_number(group, "CPU-set member")?);
        }
    }
    Ok(cpus)
}

#[cfg(target_os = "linux")]
fn cpu_topology(lane: Lane) -> Result<String, String> {
    let cpus = expand_cpu_set(lane.cpu_set())?;
    let mut records = Vec::with_capacity(cpus.len());
    for cpu in cpus {
        let root = format!("/sys/devices/system/cpu/cpu{cpu}");
        let package = read_trimmed(&format!("{root}/topology/physical_package_id"))?;
        let die = read_trimmed(&format!("{root}/topology/die_id"))?;
        let core = read_trimmed(&format!("{root}/topology/core_id"))?;
        let siblings = read_trimmed(&format!("{root}/topology/thread_siblings_list"))?;
        let last_level_cache = read_trimmed(&format!("{root}/cache/index3/id"))?;
        let shared_cache = read_trimmed(&format!("{root}/cache/index3/shared_cpu_list"))?;
        if lane == Lane::SameFramePromotion {
            validate_same_frame_cpu(
                cpu,
                &package,
                &die,
                &core,
                &siblings,
                &last_level_cache,
                &shared_cache,
            )?;
        }
        records.push(format!(
            "cpu{cpu}:package{package}:die{die}:core{core}:siblings{siblings}:llc{last_level_cache}:shared{shared_cache}"
        ));
    }
    Ok(records.join(";"))
}

#[cfg(target_os = "linux")]
fn validate_same_frame_cpu(
    cpu: u32,
    package: &str,
    die: &str,
    core: &str,
    siblings: &str,
    last_level_cache: &str,
    shared_cache: &str,
) -> Result<(), String> {
    let core_expected = cpu % 32;
    let siblings_expected = format!("{core_expected},{}", core_expected + 32);
    if package != "0"
        || die != "0"
        || core != core_expected.to_string()
        || siblings != siblings_expected
        || last_level_cache != "0"
        || shared_cache != Lane::SameFramePromotion.cpu_set()
    {
        return Err(format!(
            "CPU {cpu} is outside the frozen same-frame SMT and shared-LLC topology"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn memlock_limits() -> Result<(u64, u64), String> {
    let limits = fs::read_to_string("/proc/self/limits").map_err(display_error)?;
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max locked memory"))
        .ok_or_else(|| "Linux did not report Max locked memory".to_owned())?;
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 6 {
        return Err("Max locked memory row is malformed".to_owned());
    }
    Ok((
        parse_number(fields[3], "soft memlock")?,
        parse_number(fields[4], "hard memlock")?,
    ))
}

#[cfg(target_os = "linux")]
fn cpu_model() -> Result<String, String> {
    fs::read_to_string("/proc/cpuinfo")
        .map_err(display_error)?
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .map(str::to_owned)
        .ok_or_else(|| "Linux did not report a CPU model".to_owned())
}

#[cfg(target_os = "linux")]
fn storage_identity(input: &Path) -> Result<String, String> {
    let device = fs::metadata(input).map_err(display_error)?.dev();
    let major = ((device >> 8) & 0x0fff) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0x00ff) | ((device >> 12) & 0xffff_ff00);
    let mut block =
        fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).map_err(display_error)?;
    for _ in 0..8 {
        let model = block.join("device/model");
        if let Ok(value) = fs::read_to_string(model) {
            return Ok(value.trim().to_owned());
        }
        if !block.pop() {
            break;
        }
    }
    Err(format!(
        "input backing device {major}:{minor} has no storage model"
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn arena_evidence(base: usize, span: u64) -> Result<ArenaEvidence, String> {
    let smaps = fs::read_to_string("/proc/self/smaps").map_err(display_error)?;
    let (mapping_start, section) = smaps_section(&smaps, base)?;
    let kernel_page_bytes = smaps_kib(section, "KernelPageSize:")? * 1024;
    let mmu_page_bytes = smaps_kib(section, "MMUPageSize:")? * 1024;
    let anon_huge_bytes = smaps_kib(section, "AnonHugePages:")? * 1024;
    let numa_policy = numa_policy(mapping_start)?;
    Ok(ArenaEvidence {
        base,
        span,
        kernel_page_bytes,
        mmu_page_bytes,
        anon_huge_bytes,
        numa_policy,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn arena_evidence(base: usize, span: u64) -> Result<ArenaEvidence, String> {
    let _ = std::thread::available_parallelism().map_err(|error| error.to_string())?;
    Ok(ArenaEvidence {
        base,
        span,
        kernel_page_bytes: 4096,
        mmu_page_bytes: 4096,
        anon_huge_bytes: 0,
        numa_policy: "non-linux-smoke".to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn smaps_section(smaps: &str, address: usize) -> Result<(usize, &str), String> {
    let mut offset = 0_usize;
    while offset < smaps.len() {
        let rest = &smaps[offset..];
        let end = rest.find('\n').unwrap_or(rest.len());
        let header = &rest[..end];
        if let Some((start, stop)) = mapping_range(header)?
            && (start..stop).contains(&address)
        {
            return Ok((start, rest));
        }
        offset += end.saturating_add(1);
    }
    Err(format!("arena address 0x{address:x} is absent from smaps"))
}

#[cfg(target_os = "linux")]
fn mapping_range(header: &str) -> Result<Option<(usize, usize)>, String> {
    let Some(range) = header.split_whitespace().next() else {
        return Ok(None);
    };
    let Some((start, end)) = range.split_once('-') else {
        return Ok(None);
    };
    if start.len() < 8 || end.len() < 8 {
        return Ok(None);
    }
    let start = usize::from_str_radix(start, 16).map_err(display_error)?;
    let end = usize::from_str_radix(end, 16).map_err(display_error)?;
    Ok(Some((start, end)))
}

#[cfg(target_os = "linux")]
fn smaps_kib(section: &str, field: &str) -> Result<u64, String> {
    let value = section
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|tail| tail.split_whitespace().next())
        .ok_or_else(|| format!("smaps section has no {field}"))?;
    parse_number(value, field)
}

#[cfg(target_os = "linux")]
fn numa_policy(mapping_start: usize) -> Result<String, String> {
    let prefix = format!("{mapping_start:x} ");
    fs::read_to_string("/proc/self/numa_maps")
        .map_err(display_error)?
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|line| line.replace(',', ";"))
        .ok_or_else(|| "arena VMA is absent from numa_maps".to_owned())
}

#[cfg(all(target_os = "linux", pfr_product_retention))]
pub(crate) fn current_thread_id() -> Result<u32, String> {
    let target = fs::read_link("/proc/thread-self").map_err(display_error)?;
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "thread-self symlink has no thread id".to_owned())?;
    parse_number(name, "Linux thread id")
}

#[cfg(all(not(target_os = "linux"), pfr_product_retention))]
pub(crate) fn current_thread_id() -> Result<u32, String> {
    let _ = std::thread::available_parallelism().map_err(|error| error.to_string())?;
    Ok(0)
}

#[cfg(all(target_os = "linux", pfr_product_retention))]
pub(crate) fn wait_until_parked(thread_id: u32) -> Result<(), String> {
    let path = format!("/proc/self/task/{thread_id}/wchan");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        let wait = read_trimmed(&path)?;
        if wait.contains("io_uring") || wait.contains("ep_poll") || wait.contains("schedule") {
            return Ok(());
        }
        std::thread::yield_now();
    }
    Err(format!(
        "poller thread {thread_id} did not enter a kernel wait"
    ))
}

#[cfg(all(not(target_os = "linux"), pfr_product_retention))]
pub(crate) fn wait_until_parked(_thread_id: u32) -> Result<(), String> {
    let _ = std::thread::available_parallelism().map_err(|error| error.to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(1));
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &str) -> Result<String, String> {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|error| format!("read {path}: {error}"))
}
