//! The built-in process collector (see
//! [`Registry::register_process_collector`](super::Registry::register_process_collector)).
//!
//! Start time and uptime are available everywhere. The remaining values are
//! read from `/proc/self` and are therefore Linux-only; the parsers below are
//! plain string functions so they are compiled and tested on every platform.

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

/// Refreshes the process metrics in `reg`.
pub(super) fn collect(reg: &Registry) {
    let (first_wall, first_mono) = *first_seen();
    let mut start_secs = unix_secs(first_wall);
    let mut uptime_secs = first_mono.elapsed().as_secs_f64();

    if cfg!(target_os = "linux") {
        let stat = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|text| parse_stat(&text));
        if let Some(stat) = stat {
            reg.float_counter(
                "process_cpu_seconds_total",
                "Total user and system CPU time spent, in seconds",
                &[],
            )
            .raise_to(stat.cpu_ticks as f64 / CLOCK_TICKS_PER_SEC);
            reg.gauge("process_threads", "Number of OS threads", &[])
                .set(stat.threads);
            let boot = std::fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|text| parse_boot_time(&text));
            if let Some(boot) = boot {
                start_secs = boot as f64 + stat.start_ticks as f64 / CLOCK_TICKS_PER_SEC;
                uptime_secs = (unix_secs(SystemTime::now()) - start_secs).max(0.0);
            }
        }
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(rss) = parse_status_kb(&status, "VmRSS") {
                reg.gauge(
                    "process_resident_memory_bytes",
                    "Resident memory size in bytes",
                    &[],
                )
                .set(rss);
            }
            if let Some(virt) = parse_status_kb(&status, "VmSize") {
                reg.gauge(
                    "process_virtual_memory_bytes",
                    "Virtual memory size in bytes",
                    &[],
                )
                .set(virt);
            }
        }
        if let Ok(dir) = std::fs::read_dir("/proc/self/fd") {
            // The directory handle used for the listing is itself one entry.
            let open = (dir.count() as i64 - 1).max(0);
            reg.gauge("process_open_fds", "Number of open file descriptors", &[])
                .set(open);
        }
        let max_fds = std::fs::read_to_string("/proc/self/limits")
            .ok()
            .and_then(|text| parse_max_fds(&text));
        if let Some(max) = max_fds {
            reg.gauge(
                "process_max_fds",
                "Maximum number of open file descriptors",
                &[],
            )
            .set(max);
        }
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
}
