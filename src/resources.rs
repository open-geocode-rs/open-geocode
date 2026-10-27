//! What this process can actually use: CPUs, memory and disk.
//!
//! Installed hardware is the wrong number. A process may be held to fewer
//! CPUs by its affinity mask, a cgroup quota or a Windows Job object, and to
//! less memory by a cgroup, a Job object, an rlimit or the commit limit,
//! and memory in use by others is not free. [`Resources::probe`] reads the
//! limits that bind:
//!
//! - CPUs: on Linux `available_parallelism`, which reads the affinity mask
//!   and the cgroup CPU quota; on Windows the process affinity mask (across
//!   processor groups) and a Job object's hard CPU rate cap, which the
//!   standard library does not read.
//! - Memory: on Linux `MemAvailable`, a cgroup v2 `memory.max` or v1
//!   `memory.limit_in_bytes` less what the cgroup uses, and `RLIMIT_AS` and
//!   `RLIMIT_DATA`; on Windows the available physical memory, the commit the
//!   process can still charge (allocations fail when it runs out, however
//!   much RAM looks free) and a Job object's memory limit.
//!
//! [`free_disk`] reads the free space of the volume a path is on. A long
//! build probes again at a stage boundary, since by then it has used memory
//! and page cache a fresh process would have.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The CPUs and memory this process can use, and what limited them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resources {
    pub usable_cpus: usize,
    /// The most memory the process could have: installed RAM, or less under
    /// a cgroup, Job object or rlimit.
    pub total_memory_bytes: u64,
    /// The memory the process can take now without paging or failing a
    /// commit.
    pub available_memory_bytes: u64,
    /// What bounds `usable_cpus` and `available_memory_bytes`.
    pub cpu_limit: String,
    pub memory_limit: String,
}

impl Resources {
    pub fn probe() -> Self {
        let mut resources = platform::probe();
        resources.usable_cpus = resources.usable_cpus.max(1);
        resources.available_memory_bytes = resources
            .available_memory_bytes
            .min(resources.total_memory_bytes);
        // Tests run many small builds side by side, and each would count the
        // others' memory as taken: a build's plan then refused or warned
        // depending on what its neighbours held at that instant. A test build
        // sizes itself to the machine instead.
        #[cfg(test)]
        {
            resources.available_memory_bytes = resources.total_memory_bytes;
        }
        resources
    }
}

/// Free bytes the caller may use on the volume `path` is on, from its
/// nearest existing ancestor, or `None` when it cannot be read.
pub fn free_disk(path: &Path) -> Option<u64> {
    let path = std::path::absolute(path).ok()?;
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    platform::free_disk(existing)
}

/// Whether `left` and `right` are on one volume, so their disk needs add up.
pub fn same_volume(left: &Path, right: &Path) -> bool {
    let existing = |path: &Path| {
        let path = std::path::absolute(path).ok()?;
        path.ancestors()
            .find(|ancestor| ancestor.exists())
            .map(Path::to_path_buf)
    };
    match (existing(left), existing(right)) {
        (Some(left), Some(right)) => platform::same_volume(&left, &right),
        _ => true,
    }
}

/// CPU time this process has used, user and kernel, over every thread.
/// The build divides it by wall time and threads to tell a stage that kept
/// its cores busy from one that waited.
pub fn process_cpu_time() -> Option<std::time::Duration> {
    platform::process_cpu_time()
}

/// The CPU clock of one thread, readable from any thread: the build reads
/// the thread that drives a stage, whose sink is often its serial path.
#[derive(Debug)]
pub struct ThreadClock(platform::ThreadClock);

impl ThreadClock {
    /// The clock of the calling thread, where it can be read.
    pub fn current() -> Option<Self> {
        platform::ThreadClock::current().map(Self)
    }

    /// CPU time the thread has used, user and kernel.
    pub fn cpu_time(&self) -> Option<std::time::Duration> {
        self.0.cpu_time()
    }
}

/// Memory this process holds in RAM now: the working set on Windows, the
/// resident set on Linux. `None` where it cannot be read.
pub fn resident_bytes() -> Option<u64> {
    platform::resident_bytes()
}

/// Ask the OS to run this process at full speed. Windows otherwise may move
/// a long, unattended process to efficiency cores and lower clocks
/// (EcoQoS) once it decides no user waits on it, which is what a build
/// looks like. Nothing to do elsewhere.
pub fn opt_out_of_power_throttling() {
    platform::opt_out_of_power_throttling();
}

/// Run below the programs a person works in, so a build left running in
/// the background takes only the CPU they leave idle. Their typing and
/// browsing come first; the build still gets every idle core.
pub fn yield_to_interactive_programs() {
    platform::yield_to_interactive_programs();
}

/// Run the calling thread below the process's other threads, so its work
/// takes only the CPU they leave idle.
pub fn lower_this_thread() {
    platform::lower_this_thread();
}

#[cfg(windows)]
mod platform {
    use std::{os::windows::ffi::OsStrExt, path::Path};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE},
        Storage::FileSystem::GetDiskFreeSpaceExW,
        System::{
            JobObjects::{
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
                JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject,
            },
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
            Threading::{
                ALL_PROCESSOR_GROUPS, BELOW_NORMAL_PRIORITY_CLASS, GetActiveProcessorCount,
                GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetProcessAffinityMask,
                GetProcessTimes, GetThreadTimes, OpenThread,
                PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetPriorityClass,
                SetProcessInformation, SetThreadPriority, THREAD_PRIORITY_LOWEST,
                THREAD_QUERY_LIMITED_INFORMATION,
            },
        },
    };

    use super::Resources;

    pub(super) fn probe() -> Resources {
        let (usable_cpus, cpu_limit) = cpus();
        let mut resources = Resources {
            usable_cpus,
            cpu_limit,
            ..Resources::default()
        };
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            // SAFETY: MEMORYSTATUSEX is plain integers, valid all zero.
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: `status` is a MEMORYSTATUSEX with its length set.
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            resources.total_memory_bytes = status.ullTotalPhys;
            resources.available_memory_bytes = status.ullAvailPhys;
            resources.memory_limit = "available physical memory".to_string();
            if status.ullAvailPageFile < resources.available_memory_bytes {
                resources.available_memory_bytes = status.ullAvailPageFile;
                resources.memory_limit = "commit limit".to_string();
            }
        }
        if let Some(limit) = job_memory_limit() {
            resources.total_memory_bytes = resources.total_memory_bytes.min(limit);
            let left = limit.saturating_sub(private_bytes());
            if left < resources.available_memory_bytes {
                resources.available_memory_bytes = left;
                resources.memory_limit = "Job object memory limit".to_string();
            }
        }
        resources
    }

    /// The CPUs of the process affinity mask, or of every processor group
    /// when the process may run on all of them (Windows 11 lets a process
    /// span groups), lowered by a Job object's hard CPU rate cap.
    fn cpus() -> (usize, String) {
        // SAFETY: plain queries of the current process.
        let all = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: both out-pointers are valid for the call.
        let masked =
            unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) } != 0;
        let (mut cpus, mut limit) = if masked && process != system {
            (process.count_ones() as usize, "affinity mask".to_string())
        } else {
            (all.max(1), "online processors".to_string())
        };
        let mut rate = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
        // SAFETY: a null job handle queries the job of this process; the
        // buffer is the size passed.
        let queried = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectCpuRateControlInformation,
                (&mut rate as *mut JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } != 0;
        let capped = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        if queried && rate.ControlFlags & capped == capped {
            // In hundredths of a percent of every processor.
            // SAFETY: with a hard cap the union holds `CpuRate`.
            let share = unsafe { rate.Anonymous.CpuRate } as usize;
            let allowed = (all * share).div_ceil(10_000).max(1);
            if allowed < cpus {
                cpus = allowed;
                limit = "Job object CPU rate".to_string();
            }
        }
        (cpus, limit)
    }

    /// The memory limit of the Job object this process is in, if any.
    fn job_memory_limit() -> Option<u64> {
        // SAFETY: the limit information is plain integers, valid all zero.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: as in `cpus`.
        let queried = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } != 0;
        if !queried {
            return None;
        }
        let flags = info.BasicLimitInformation.LimitFlags;
        [
            (JOB_OBJECT_LIMIT_PROCESS_MEMORY, info.ProcessMemoryLimit),
            (JOB_OBJECT_LIMIT_JOB_MEMORY, info.JobMemoryLimit),
        ]
        .into_iter()
        .filter(|(flag, _)| flags & flag != 0)
        .map(|(_, limit)| limit as u64)
        .min()
    }

    /// Bytes this process has committed.
    fn private_bytes() -> u64 {
        // SAFETY: the counters are plain integers, valid all zero.
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        // SAFETY: the EX counters start with the plain ones, and `cb` says
        // which were passed.
        let read = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                counters.cb,
            )
        } != 0;
        if read {
            counters.PrivateUsage as u64
        } else {
            0
        }
    }

    /// Kernel plus user time, from FILETIMEs in 100 ns ticks.
    fn busy(kernel: &FILETIME, user: &FILETIME) -> std::time::Duration {
        let ticks =
            |time: &FILETIME| u64::from(time.dwHighDateTime) << 32 | u64::from(time.dwLowDateTime);
        std::time::Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
    }

    pub(super) fn process_cpu_time() -> Option<std::time::Duration> {
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        // SAFETY: four valid FILETIME out-pointers for the current process.
        let read =
            unsafe { GetProcessTimes(GetCurrentProcess(), created, exited, kernel, user) } != 0;
        read.then(|| busy(kernel, user))
    }

    /// A handle to a thread, opened to read its times.
    #[derive(Debug)]
    pub(super) struct ThreadClock(HANDLE);

    // SAFETY: the handle is a kernel object reference, valid on any thread
    // until closed, and only ever used to query times.
    unsafe impl Send for ThreadClock {}
    // SAFETY: as for `Send`; queries do not mutate the handle.
    unsafe impl Sync for ThreadClock {}

    impl ThreadClock {
        pub(super) fn current() -> Option<Self> {
            // SAFETY: opens the calling thread, which exists, for queries.
            let handle =
                unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
            (!handle.is_null()).then_some(Self(handle))
        }

        pub(super) fn cpu_time(&self) -> Option<std::time::Duration> {
            let mut times = [FILETIME::default(); 4];
            let [created, exited, kernel, user] = &mut times;
            // SAFETY: an open thread handle and four valid out-pointers.
            let read = unsafe { GetThreadTimes(self.0, created, exited, kernel, user) } != 0;
            read.then(|| busy(kernel, user))
        }
    }

    impl Drop for ThreadClock {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by `current` and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub(super) fn resident_bytes() -> Option<u64> {
        // SAFETY: the counters are plain integers, valid all zero.
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        // SAFETY: as in `private_bytes`.
        let read = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                counters.cb,
            )
        } != 0;
        read.then_some(counters.WorkingSetSize as u64)
    }

    pub(super) fn free_disk(path: &Path) -> Option<u64> {
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>();
        let mut free = 0u64;
        // SAFETY: `wide` is a NUL-terminated path; the totals may be null.
        let read = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0;
        read.then_some(free)
    }

    pub(super) fn same_volume(left: &Path, right: &Path) -> bool {
        // The drive or share each path starts with.
        let root = |path: &Path| {
            path.components()
                .next()
                .map(|part| part.as_os_str().to_ascii_lowercase())
        };
        root(left) == root(right)
    }

    pub(super) fn yield_to_interactive_programs() {
        // SAFETY: the pseudo handle of this process, and a priority class
        // the call takes. Failure leaves the normal priority.
        unsafe {
            SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
        }
    }

    pub(super) fn lower_this_thread() {
        // SAFETY: the pseudo handle of the calling thread, and a priority the
        // call takes. Failure leaves the normal priority.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
        }
    }

    pub(super) fn opt_out_of_power_throttling() {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            // Controlled and not set: never throttle execution speed.
            StateMask: 0,
        };
        // SAFETY: `state` is the structure this information class takes.
        // Failure (an older Windows) leaves the default policy.
        unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

    use super::Resources;

    pub(super) fn probe() -> Resources {
        // On Linux this reads the affinity mask and the cgroup CPU quota.
        let usable_cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let mut resources = Resources {
            usable_cpus,
            cpu_limit: "affinity mask and CPU quota".to_string(),
            ..Resources::default()
        };
        memory(&mut resources);
        for (resource, name) in [
            (libc::RLIMIT_AS, "RLIMIT_AS"),
            (libc::RLIMIT_DATA, "RLIMIT_DATA"),
        ] {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `limit` is valid for the call.
            if unsafe { libc::getrlimit(resource, &mut limit) } == 0
                && limit.rlim_cur != libc::RLIM_INFINITY
            {
                // `rlim_t` is 32 bits on some targets.
                #[allow(clippy::unnecessary_cast)]
                let limit = limit.rlim_cur as u64;
                resources.total_memory_bytes = resources.total_memory_bytes.min(limit);
                if limit < resources.available_memory_bytes {
                    resources.available_memory_bytes = limit;
                    resources.memory_limit = name.to_string();
                }
            }
        }
        resources
    }

    #[cfg(target_os = "linux")]
    fn memory(resources: &mut Resources) {
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let field = |name: &str| {
            meminfo.lines().find_map(|line| {
                let kilobytes = line.strip_prefix(name)?.trim().strip_suffix("kB")?;
                kilobytes
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .map(|value| value << 10)
            })
        };
        resources.total_memory_bytes = field("MemTotal:").unwrap_or(0);
        resources.available_memory_bytes = field("MemAvailable:").unwrap_or(0);
        resources.memory_limit = "MemAvailable".to_string();
        if let Some((limit, left)) = cgroup_memory() {
            resources.total_memory_bytes = resources.total_memory_bytes.min(limit);
            if left < resources.available_memory_bytes {
                resources.available_memory_bytes = left;
                resources.memory_limit = "cgroup memory limit".to_string();
            }
        }
    }

    /// The tightest cgroup memory limit over this process's cgroup and its
    /// ancestors, and what is left under it: v2 `memory.max` less
    /// `memory.current`, or v1 `memory.limit_in_bytes` less
    /// `memory.usage_in_bytes`.
    #[cfg(target_os = "linux")]
    fn cgroup_memory() -> Option<(u64, u64)> {
        let groups = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let read = |path: &Path| -> Option<u64> {
            std::fs::read_to_string(path).ok()?.trim().parse().ok()
        };
        let mut tightest: Option<(u64, u64)> = None;
        let mut consider = |limit: u64, used: u64| {
            let left = limit.saturating_sub(used);
            if tightest.is_none_or(|(_, best)| left < best) {
                tightest = Some((limit, left));
            }
        };
        for line in groups.lines() {
            let mut parts = line.splitn(3, ':');
            let (Some(_), Some(controllers), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let relative = path.trim_start_matches('/');
            if controllers.is_empty() {
                let mut directory = Path::new("/sys/fs/cgroup").join(relative);
                loop {
                    // "max" does not parse, which is no limit.
                    if let Some(limit) = read(&directory.join("memory.max")) {
                        consider(limit, read(&directory.join("memory.current")).unwrap_or(0));
                    }
                    if !directory.pop() || !directory.starts_with("/sys/fs/cgroup") {
                        break;
                    }
                }
            } else if controllers
                .split(',')
                .any(|controller| controller == "memory")
            {
                let directory = Path::new("/sys/fs/cgroup/memory").join(relative);
                // Without a limit v1 reports a value near the largest page
                // aligned i64.
                if let Some(limit) =
                    read(&directory.join("memory.limit_in_bytes")).filter(|limit| *limit < 1 << 62)
                {
                    consider(
                        limit,
                        read(&directory.join("memory.usage_in_bytes")).unwrap_or(0),
                    );
                }
            }
        }
        tightest
    }

    #[cfg(not(target_os = "linux"))]
    fn memory(resources: &mut Resources) {
        // SAFETY: plain queries.
        let (pages, page) = unsafe {
            (
                libc::sysconf(libc::_SC_PHYS_PAGES),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        let total = (pages.max(0) as u64).saturating_mul(page.max(0) as u64);
        resources.total_memory_bytes = total;
        // No portable figure of the memory in use: take half.
        resources.available_memory_bytes = total / 2;
        resources.memory_limit = "half of installed memory".to_string();
    }

    pub(super) fn process_cpu_time() -> Option<std::time::Duration> {
        // SAFETY: `rusage` is plain integers, valid all zero, and filled by
        // the call.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `usage` is valid for the call.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return None;
        }
        let time = |value: libc::timeval| {
            std::time::Duration::from_secs(value.tv_sec.max(0) as u64)
                + std::time::Duration::from_micros(value.tv_usec.max(0) as u64)
        };
        Some(time(usage.ru_utime) + time(usage.ru_stime))
    }

    /// A thread's CPU clock, which POSIX lets any thread read.
    #[derive(Debug)]
    pub(super) struct ThreadClock(libc::clockid_t);

    impl ThreadClock {
        #[cfg(target_os = "linux")]
        pub(super) fn current() -> Option<Self> {
            let mut clock: libc::clockid_t = 0;
            // SAFETY: the calling thread exists; `clock` is valid for the call.
            let found = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut clock) };
            (found == 0).then_some(Self(clock))
        }

        #[cfg(not(target_os = "linux"))]
        pub(super) fn current() -> Option<Self> {
            None
        }

        pub(super) fn cpu_time(&self) -> Option<std::time::Duration> {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: a clock id from `pthread_getcpuclockid` and a valid
            // out-pointer.
            (unsafe { libc::clock_gettime(self.0, &mut time) } == 0).then(|| {
                std::time::Duration::new(time.tv_sec.max(0) as u64, time.tv_nsec.max(0) as u32)
            })
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn resident_bytes() -> Option<u64> {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        // SAFETY: a plain query.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Some(pages.saturating_mul(page.max(0) as u64))
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn resident_bytes() -> Option<u64> {
        None
    }

    pub(super) fn free_disk(path: &Path) -> Option<u64> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `statvfs` is a plain C struct of integers, for which an
        // all-zero bit pattern is a valid value; `statvfs` overwrites it.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is NUL-terminated and `stat` valid for the call.
        (unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0)
            .then(|| (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }

    pub(super) fn same_volume(left: &Path, right: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(left), std::fs::metadata(right)) {
            (Ok(left), Ok(right)) => left.dev() == right.dev(),
            _ => true,
        }
    }

    pub(super) fn opt_out_of_power_throttling() {}

    // Linux keeps a niceness per thread, so this lowers only the caller.
    #[cfg(target_os = "linux")]
    pub(super) fn lower_this_thread() {
        // SAFETY: gettid and setpriority read no memory. Failure leaves the
        // niceness the thread started with.
        unsafe {
            let thread = libc::syscall(libc::SYS_gettid) as libc::id_t;
            libc::setpriority(libc::PRIO_PROCESS, thread, 10);
        }
    }

    // Elsewhere niceness belongs to the whole process, and lowering it would
    // slow single lookups too.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn lower_this_thread() {}

    pub(super) fn yield_to_interactive_programs() {
        // SAFETY: setpriority reads no memory. Failure leaves the niceness
        // the process started with.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_finds_cpus_memory_and_disk() {
        let resources = Resources::probe();
        assert!(resources.usable_cpus >= 1);
        assert!(resources.total_memory_bytes > 0, "{resources:?}");
        assert!(resources.available_memory_bytes > 0, "{resources:?}");
        assert!(resources.available_memory_bytes <= resources.total_memory_bytes);
        let missing = std::env::temp_dir().join("open-geocode-no-such-directory/below");
        assert!(free_disk(&missing).is_some_and(|free| free > 0));
        assert!(same_volume(&missing, &std::env::temp_dir()));
        assert!(process_cpu_time().is_some());
        #[cfg(any(windows, target_os = "linux"))]
        {
            let clock = ThreadClock::current().expect("thread clock");
            let before = clock.cpu_time().expect("thread time");
            // Spin until the clock moves rather than for a fixed time: a
            // thread clock ticks in scheduler quanta, and a loaded machine can
            // leave a short spin with less CPU time than one quantum.
            let spin = std::time::Instant::now();
            let mut spun = before;
            while spun <= before && spin.elapsed() < std::time::Duration::from_secs(10) {
                spun = clock.cpu_time().expect("thread time");
            }
            assert!(spun > before, "the thread clock moves as the thread runs");
            let read = std::thread::scope(|scope| scope.spawn(|| clock.cpu_time()).join());
            assert!(read.expect("joined").expect("thread time") >= spun);
        }
        #[cfg(any(windows, target_os = "linux"))]
        assert!(resident_bytes().is_some_and(|bytes| bytes > 0));
    }
}
