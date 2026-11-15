//! The built-in process collector (see
//! [`Registry::register_process_collector`](super::Registry::register_process_collector)).
//!
//! Start time and uptime are available everywhere. The other values come
//! from `/proc/self` on Linux, from `getrusage`, `proc_pidinfo`, `getrlimit`
//! and `/dev/fd` on macOS, and from kernel32 on Windows; the `/proc` parsers
//! are plain string functions so they are compiled and tested on every
//! platform.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::Registry;

/// Kernel clock ticks per second for the `/proc` time fields. `USER_HZ` is 100
/// on every Linux architecture Rust targets; reading the real value needs
/// `sysconf`, which is not reachable without `unsafe` or a dependency.
const CLOCK_TICKS_PER_SEC: f64 = 100.0;

/// The first moment this module was asked about the process: the fallback
/// start time, and the monotonic origin for uptime.
fn first_seen() -> &'static (SystemTime, Instant) {
    static FIRST: OnceLock<(SystemTime, Instant)> = OnceLock::new();
    FIRST.get_or_init(|| (SystemTime::now(), Instant::now()))
}

/// Captures the fallback start time as early as possible.
pub(super) fn touch_start() {
    let _ = first_seen();
}

fn unix_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The fields of `/proc/self/stat` this collector uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Stat {
    /// User + system CPU time, in clock ticks.
    pub cpu_ticks: u64,
    /// The number of threads.
    pub threads: i64,
    /// Process start, in clock ticks since boot.
    pub start_ticks: u64,
}

/// Parses `/proc/<pid>/stat`. The second field (the command name) is wrapped
/// in parentheses and may itself contain spaces and parentheses, so fields are
/// counted from the **last** `)`.
pub(super) fn parse_stat(text: &str) -> Option<Stat> {
    let rest = &text[text.rfind(')')? + 1..];
    // `rest` starts at field 3 (state); field N is at index N - 3.
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    let field = |n: usize| fields.get(n - 3).copied();
    let utime: u64 = field(14)?.parse().ok()?;
    let stime: u64 = field(15)?.parse().ok()?;
    Some(Stat {
        cpu_ticks: utime.saturating_add(stime),
        threads: field(20)?.parse().ok()?,
        start_ticks: field(22)?.parse().ok()?,
    })
}

/// Reads a `Key:   <n> kB` line from `/proc/<pid>/status`, in bytes.
pub(super) fn parse_status_kb(text: &str, key: &str) -> Option<i64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.strip_prefix(':')?;
        let kb: i64 = rest.split_ascii_whitespace().next()?.parse().ok()?;
        kb.checked_mul(1024)
    })
}

/// Reads the boot time (`btime <secs>`) from `/proc/stat`.
pub(super) fn parse_boot_time(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix("btime ")
            .and_then(|rest| rest.trim().parse().ok())
    })
}

/// Reads the soft `Max open files` limit from `/proc/<pid>/limits`; `None`
/// when it is `unlimited` or absent.
pub(super) fn parse_max_fds(text: &str) -> Option<i64> {
    text.lines().find_map(|line| {
        line.strip_prefix("Max open files")?
            .split_ascii_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

/// What the operating system reported about this process; a value it could
/// not report is `None` and is left out of the output.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(super) struct Sample {
    /// User + system CPU time, in seconds.
    pub cpu_seconds: Option<f64>,
    /// Resident set size, in bytes.
    pub resident_bytes: Option<i64>,
    /// Virtual memory size, in bytes.
    pub virtual_bytes: Option<i64>,
    /// The number of OS threads.
    pub threads: Option<i64>,
    /// Open file descriptors (handles on Windows).
    pub open_fds: Option<i64>,
    /// The soft limit on open file descriptors.
    pub max_fds: Option<i64>,
    /// Process start, in seconds since the Unix epoch.
    pub start_unix_secs: Option<f64>,
}

/// Counts the entries of a per-process descriptor directory (`/proc/self/fd`
/// on Linux, `/dev/fd` on macOS). The directory handle used for the listing
/// is itself one entry, so it is not counted.
fn count_fd_dir(path: &str) -> Option<i64> {
    let dir = std::fs::read_dir(path).ok()?;
    Some((dir.count() as i64 - 1).max(0))
}

/// Everything from `/proc` (Linux).
fn proc_sample() -> Sample {
    let mut out = Sample::default();
    let stat = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|text| parse_stat(&text));
    if let Some(stat) = stat {
        out.cpu_seconds = Some(stat.cpu_ticks as f64 / CLOCK_TICKS_PER_SEC);
        out.threads = Some(stat.threads);
        let boot = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| parse_boot_time(&text));
        if let Some(boot) = boot {
            out.start_unix_secs = Some(boot as f64 + stat.start_ticks as f64 / CLOCK_TICKS_PER_SEC);
        }
    }
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        out.resident_bytes = parse_status_kb(&status, "VmRSS");
        out.virtual_bytes = parse_status_kb(&status, "VmSize");
    }
    out.open_fds = count_fd_dir("/proc/self/fd");
    out.max_fds = std::fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|text| parse_max_fds(&text));
    out
}

/// What this platform reports: `/proc` on Linux, libSystem plus `/dev/fd` on
/// macOS, kernel32 on Windows, nothing elsewhere.
#[allow(unreachable_code)]
pub(super) fn sample() -> Sample {
    if cfg!(target_os = "linux") {
        return proc_sample();
    }
    #[cfg(target_os = "macos")]
    {
        let mut out = ffi::macos::sample();
        out.open_fds = count_fd_dir("/dev/fd");
        return out;
    }
    #[cfg(windows)]
    {
        return ffi::windows::sample();
    }
    Sample::default()
}

/// Refreshes the process metrics in `reg`.
pub(super) fn collect(reg: &Registry) {
    let (first_wall, first_mono) = *first_seen();
    let sample = sample();
    let (start_secs, uptime_secs) = match sample.start_unix_secs {
        Some(start) => (start, (unix_secs(SystemTime::now()) - start).max(0.0)),
        None => (unix_secs(first_wall), first_mono.elapsed().as_secs_f64()),
    };

    if let Some(cpu) = sample.cpu_seconds {
        reg.float_counter(
            "process_cpu_seconds_total",
            "Total user and system CPU time spent, in seconds",
            &[],
        )
        .raise_to(cpu);
    }
    if let Some(threads) = sample.threads {
        reg.gauge("process_threads", "Number of OS threads", &[])
            .set(threads);
    }
    if let Some(rss) = sample.resident_bytes {
        reg.gauge(
            "process_resident_memory_bytes",
            "Resident memory size in bytes",
            &[],
        )
        .set(rss);
    }
    if let Some(virt) = sample.virtual_bytes {
        reg.gauge(
            "process_virtual_memory_bytes",
            "Virtual memory size in bytes",
            &[],
        )
        .set(virt);
    }
    if let Some(open) = sample.open_fds {
        reg.gauge("process_open_fds", "Number of open file descriptors", &[])
            .set(open);
    }
    if let Some(max) = sample.max_fds {
        reg.gauge(
            "process_max_fds",
            "Maximum number of open file descriptors",
            &[],
        )
        .set(max);
    }
    reg.float_gauge(
        "process_start_time_seconds",
        "Start time of the process since the Unix epoch, in seconds",
        &[],
    )
    .set(start_secs);
    reg.float_gauge(
        "process_uptime_seconds",
        "Time since the process started, in seconds",
        &[],
    )
    .set(uptime_secs);
}

/// The operating-system calls behind the macOS and Windows collectors.
///
/// This is one of the crate's four audited `unsafe` regions (the others are
/// `fsutils/mmap.rs`, `fsutils/oslock.rs` and `hash/random/windows.rs`). Every call passes pointers to correctly sized,
/// `#[repr(C)]`, zero-initialised locals that outlive the call, and every
/// result is checked before the output is read.
#[cfg(any(target_os = "macos", windows))]
#[allow(unsafe_code)]
mod ffi {
    #[cfg(target_os = "macos")]
    pub(super) mod macos {
        use std::ffi::c_void;
        use std::mem::size_of;

        use super::super::Sample;

        const RUSAGE_SELF: i32 = 0;
        const RLIMIT_NOFILE: i32 = 8;
        /// `RLIM_INFINITY`: `(1 << 63) - 1`.
        const RLIM_INFINITY: u64 = i64::MAX as u64;
        const PROC_PIDTBSDINFO: i32 = 3;
        const PROC_PIDTASKINFO: i32 = 4;

        #[repr(C)]
        #[derive(Default)]
        struct Timeval {
            tv_sec: i64,
            tv_usec: i32,
        }

        /// `struct rusage`: two `timeval`s, then fourteen `long`s.
        #[repr(C)]
        #[derive(Default)]
        struct Rusage {
            ru_utime: Timeval,
            ru_stime: Timeval,
            ru_rest: [i64; 14],
        }

        #[repr(C)]
        #[derive(Default)]
        struct Rlimit {
            rlim_cur: u64,
            rlim_max: u64,
        }

        /// `struct proc_taskinfo` from `<sys/proc_info.h>`.
        #[repr(C)]
        #[derive(Default)]
        struct ProcTaskInfo {
            pti_virtual_size: u64,
            pti_resident_size: u64,
            pti_total_user: u64,
            pti_total_system: u64,
            pti_threads_user: u64,
            pti_threads_system: u64,
            pti_policy: i32,
            pti_faults: i32,
            pti_pageins: i32,
            pti_cow_faults: i32,
            pti_messages_sent: i32,
            pti_messages_received: i32,
            pti_syscalls_mach: i32,
            pti_syscalls_unix: i32,
            pti_csw: i32,
            pti_threadnum: i32,
            pti_numrunning: i32,
            pti_priority: i32,
        }

        /// `struct proc_bsdinfo` from `<sys/proc_info.h>`.
        #[repr(C)]
        #[derive(Default)]
        struct ProcBsdInfo {
            pbi_flags: u32,
            pbi_status: u32,
            pbi_xstatus: u32,
            pbi_pid: u32,
            pbi_ppid: u32,
            pbi_uid: u32,
            pbi_gid: u32,
            pbi_ruid: u32,
            pbi_rgid: u32,
            pbi_svuid: u32,
            pbi_svgid: u32,
            rfu_1: u32,
            pbi_comm: [u8; 16],
            pbi_name: [u8; 32],
            pbi_nfiles: u32,
            pbi_pgid: u32,
            pbi_pjobc: u32,
            e_tdev: u32,
            e_tpgid: u32,
            pbi_nice: i32,
            pbi_start_tvsec: u64,
            pbi_start_tvusec: u64,
        }

        extern "C" {
            fn getrusage(who: i32, usage: *mut Rusage) -> i32;
            fn getrlimit(resource: i32, rlp: *mut Rlimit) -> i32;
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut c_void,
                buffersize: i32,
            ) -> i32;
        }

        /// Fills `out` with `proc_pidinfo(flavor)`; `false` unless the kernel
        /// wrote exactly the whole structure.
        fn pidinfo<T>(flavor: i32, out: &mut T) -> bool {
            let Ok(pid) = i32::try_from(std::process::id()) else {
                return false;
            };
            let size = size_of::<T>() as i32;
            let buffer = out as *mut T as *mut c_void;
            // SAFETY: `buffer` points to an exclusively borrowed `T` of
            // exactly `size` bytes that outlives the call, and every `T`
            // passed here is a `#[repr(C)]` mirror of the structure the
            // kernel writes for `flavor`, made only of integers (so any bytes
            // it writes form a valid value).
            let written = unsafe { proc_pidinfo(pid, flavor, 0, buffer, size) };
            written == size
        }

        pub(in super::super) fn sample() -> Sample {
            let mut out = Sample::default();

            let mut usage = Rusage::default();
            // SAFETY: `usage` is a valid, exclusively borrowed `struct rusage`
            // that outlives the call.
            if unsafe { getrusage(RUSAGE_SELF, &mut usage) } == 0 {
                let secs = |t: &Timeval| t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6;
                out.cpu_seconds = Some(secs(&usage.ru_utime) + secs(&usage.ru_stime));
            }

            let mut task = ProcTaskInfo::default();
            if pidinfo(PROC_PIDTASKINFO, &mut task) {
                out.resident_bytes = i64::try_from(task.pti_resident_size).ok();
                out.virtual_bytes = i64::try_from(task.pti_virtual_size).ok();
                out.threads = Some(i64::from(task.pti_threadnum));
            }

            let mut bsd = ProcBsdInfo::default();
            if pidinfo(PROC_PIDTBSDINFO, &mut bsd) && bsd.pbi_start_tvsec > 0 {
                out.start_unix_secs =
                    Some(bsd.pbi_start_tvsec as f64 + bsd.pbi_start_tvusec as f64 / 1e6);
            }

            let mut limit = Rlimit::default();
            // SAFETY: `limit` is a valid, exclusively borrowed `struct rlimit`
            // that outlives the call.
            if unsafe { getrlimit(RLIMIT_NOFILE, &mut limit) } == 0
                && limit.rlim_cur < RLIM_INFINITY
            {
                out.max_fds = i64::try_from(limit.rlim_cur).ok();
            }
            out
        }
    }

    #[cfg(windows)]
    pub(super) mod windows {
        use std::ffi::c_void;
        use std::mem::size_of;

        use super::super::Sample;

        type Handle = *mut c_void;

        /// Seconds between 1601-01-01 (the `FILETIME` epoch) and 1970-01-01.
        const FILETIME_UNIX_OFFSET_SECS: f64 = 11_644_473_600.0;

        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct FileTime {
            low: u32,
            high: u32,
        }

        impl FileTime {
            /// The value in 100-nanosecond units.
            fn ticks(self) -> u64 {
                (u64::from(self.high) << 32) | u64::from(self.low)
            }
        }

        /// `PROCESS_MEMORY_COUNTERS` from `<psapi.h>`.
        #[repr(C)]
        #[derive(Default)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> Handle;
            fn GetProcessTimes(
                process: Handle,
                creation: *mut FileTime,
                exit: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
            fn K32GetProcessMemoryInfo(
                process: Handle,
                counters: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
            fn GetProcessHandleCount(process: Handle, count: *mut u32) -> i32;
        }

        pub(in super::super) fn sample() -> Sample {
            let mut out = Sample::default();
            // SAFETY: takes no arguments; returns the constant pseudo-handle
            // of the current process, which needs no closing.
            let process = unsafe { GetCurrentProcess() };

            let mut creation = FileTime::default();
            let mut exit = FileTime::default();
            let mut kernel = FileTime::default();
            let mut user = FileTime::default();
            // SAFETY: four valid, exclusively borrowed `FILETIME`s that
            // outlive the call; `process` is the current-process handle.
            let ok = unsafe {
                GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user)
            };
            if ok != 0 {
                out.cpu_seconds = Some((kernel.ticks() + user.ticks()) as f64 / 1e7);
                let secs = creation.ticks() as f64 / 1e7 - FILETIME_UNIX_OFFSET_SECS;
                if secs > 0.0 {
                    out.start_unix_secs = Some(secs);
                }
            }

            let mut counters = ProcessMemoryCounters {
                cb: size_of::<ProcessMemoryCounters>() as u32,
                ..ProcessMemoryCounters::default()
            };
            let cb = counters.cb;
            // SAFETY: `counters` is a valid, exclusively borrowed
            // `PROCESS_MEMORY_COUNTERS` of `cb` bytes that outlives the call.
            if unsafe { K32GetProcessMemoryInfo(process, &mut counters, cb) } != 0 {
                out.resident_bytes = i64::try_from(counters.working_set_size).ok();
                out.virtual_bytes = i64::try_from(counters.pagefile_usage).ok();
            }

            let mut handles = 0u32;
            // SAFETY: `handles` is a valid, exclusively borrowed `DWORD` that
            // outlives the call.
            if unsafe { GetProcessHandleCount(process, &mut handles) } != 0 {
                out.open_fds = Some(i64::from(handles));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_fields_are_counted_from_the_last_paren() {
        // A command name containing spaces and parentheses.
        let text = "4242 (my (odd) app) S 1 4242 4242 0 -1 4194560 1500 0 3 0 \
                    250 125 0 0 20 0 7 0 987654 123456789 2048 18446744073709551615 \
                    1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        let stat = parse_stat(text).expect("parses");
        assert_eq!(stat.cpu_ticks, 375);
        assert_eq!(stat.threads, 7);
        assert_eq!(stat.start_ticks, 987_654);
        assert!(parse_stat("garbage").is_none());
        assert!(parse_stat("1 (x) S 1").is_none());
    }

    #[test]
    fn status_boot_time_and_limits() {
        let status = "Name:\tapp\nVmSize:\t  204800 kB\nVmRSS:\t   10240 kB\nThreads:\t7\n";
        assert_eq!(parse_status_kb(status, "VmRSS"), Some(10_240 * 1024));
        assert_eq!(parse_status_kb(status, "VmSize"), Some(204_800 * 1024));
        assert_eq!(parse_status_kb(status, "VmSwap"), None);

        assert_eq!(
            parse_boot_time("cpu 1 2 3\nbtime 1700000000\nprocesses 5\n"),
            Some(1_700_000_000)
        );
        assert_eq!(parse_boot_time("cpu 1 2 3\n"), None);

        let limits = "Limit                     Soft Limit           Hard Limit           Units\n\
                      Max cpu time              unlimited            unlimited            seconds\n\
                      Max open files            1024                 524288               files\n";
        assert_eq!(parse_max_fds(limits), Some(1024));
        assert_eq!(
            parse_max_fds(
                "Max open files            unlimited            unlimited            files\n"
            ),
            None
        );
    }

    #[test]
    fn platform_sample_is_plausible() {
        let sample = sample();
        let now = unix_secs(SystemTime::now());
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            let rss = sample.resident_bytes.expect("rss");
            // Between 1 MiB and 1 TiB: catches a unit or layout mistake.
            assert!((1 << 20..1 << 40).contains(&rss), "rss={rss}");
            let virt = sample.virtual_bytes.expect("virtual memory");
            assert!(virt > 0, "virtual={virt}");
            let cpu = sample.cpu_seconds.expect("cpu");
            assert!((0.0..1.0e6).contains(&cpu), "cpu={cpu}");
            let start = sample.start_unix_secs.expect("start time");
            // A test process started within the last day.
            assert!(start <= now && start > now - 86_400.0, "start={start}");
            let open = sample.open_fds.expect("open fds");
            assert!((1..1_000_000).contains(&open), "open={open}");
        } else {
            assert_eq!(sample, Sample::default());
        }
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let threads = sample.threads.expect("threads");
            assert!((1..100_000).contains(&threads), "threads={threads}");
            let max = sample.max_fds.expect("max fds");
            assert!(max >= sample.open_fds.unwrap_or(0), "max={max}");
        }
    }
}
