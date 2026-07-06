//! Health / readiness probes — a small registry of named checks that a service
//! can expose (e.g. at `/health` + `/ready`). Decoupled from [`Component`] so
//! any subsystem can register a check without implementing the lifecycle trait.
//!
//! [`Component`]: crate::lifecycle::Component

use std::sync::{Arc, Mutex};

/// The health of a single check or the aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Fully healthy.
    Up,
    /// Serving but impaired.
    Degraded,
    /// Not healthy.
    Down,
}

impl HealthStatus {
    /// The lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            HealthStatus::Up => "up",
            HealthStatus::Degraded => "degraded",
            HealthStatus::Down => "down",
        }
    }
}

type Check = Arc<dyn Fn() -> HealthStatus + Send + Sync>;

/// A registry of named health checks. Cheaply cloneable (checks are shared).
#[derive(Default, Clone)]
pub struct HealthRegistry {
    checks: Arc<Mutex<Vec<(String, Check)>>>,
}

impl HealthRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a named check. Checks should be fast and non-blocking.
    pub fn register<F>(&self, name: impl Into<String>, check: F)
    where
        F: Fn() -> HealthStatus + Send + Sync + 'static,
    {
        self.checks
            .lock()
            .unwrap()
            .push((name.into(), Arc::new(check)));
    }

    /// Runs every check, returning `(name, status)` pairs. Checks are snapshotted
    /// under the lock (they're `Arc`, so cloning is cheap) and then run **without
    /// holding it**, so one slow check can't block registration or other reports.
    /// (Checks must still be non-blocking — there is no per-check timeout.)
    pub fn report(&self) -> Vec<(String, HealthStatus)> {
        let snapshot: Vec<(String, Check)> = self.checks.lock().unwrap().clone();
        snapshot
            .into_iter()
            .map(|(name, check)| (name, check()))
            .collect()
    }

    /// The aggregate status: `Down` if any check is down, else `Degraded` if any
    /// is degraded, else `Up` (also `Up` when empty).
    pub fn overall(&self) -> HealthStatus {
        let mut worst = HealthStatus::Up;
        for (_, status) in self.report() {
            match status {
                HealthStatus::Down => return HealthStatus::Down,
                HealthStatus::Degraded => worst = HealthStatus::Degraded,
                HealthStatus::Up => {}
            }
        }
        worst
    }

    /// Whether every check is fully [`Up`](HealthStatus::Up) — the readiness signal.
    pub fn is_ready(&self) -> bool {
        self.report().iter().all(|(_, s)| *s == HealthStatus::Up)
    }

    /// The report as a JSON object `{ "overall": "...", "checks": { name: "..." } }`.
    ///
    /// Every check runs **exactly once** (the aggregate is derived from the same
    /// snapshot, so side-effecting checks aren't double-invoked), and check names
    /// are JSON-escaped.
    pub fn to_json(&self) -> String {
        let report = self.report(); // single snapshot: checks run once
                                    // Aggregate from the snapshot rather than re-running the checks.
        let mut overall = HealthStatus::Up;
        for (_, status) in &report {
            match (overall, status) {
                (_, HealthStatus::Down) => overall = HealthStatus::Down,
                (HealthStatus::Up, HealthStatus::Degraded) => overall = HealthStatus::Degraded,
                _ => {}
            }
        }
        let mut s = format!("{{\"overall\":\"{}\",\"checks\":{{", overall.as_str());
        for (i, (name, status)) in report.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('"');
            escape_json_into(&mut s, name);
            s.push_str(&format!("\":\"{}\"", status.as_str()));
        }
        s.push_str("}}");
        s
    }
}

/// Appends `s` to `out` with JSON string escaping (dependency-free — lifecycle
/// does not pull in `codec`).
fn escape_json_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn aggregates_and_reports() {
        let reg = HealthRegistry::new();
        reg.register("db", || HealthStatus::Up);
        reg.register("cache", || HealthStatus::Degraded);
        assert_eq!(reg.overall(), HealthStatus::Degraded);
        assert!(!reg.is_ready());

        let down = Arc::new(AtomicBool::new(false));
        let d = down.clone();
        reg.register("queue", move || {
            if d.load(Ordering::Relaxed) {
                HealthStatus::Down
            } else {
                HealthStatus::Up
            }
        });
        assert_eq!(reg.overall(), HealthStatus::Degraded);
        down.store(true, Ordering::Relaxed);
        assert_eq!(reg.overall(), HealthStatus::Down);

        let json = reg.to_json();
        assert!(json.contains("\"overall\":\"down\""));
        assert!(json.contains("\"db\":\"up\""));
    }

    #[test]
    fn empty_is_up_and_ready() {
        let reg = HealthRegistry::new();
        assert_eq!(reg.overall(), HealthStatus::Up);
        assert!(reg.is_ready());
    }

    #[test]
    fn to_json_runs_each_check_once_and_escapes_names() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let reg = HealthRegistry::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        // A check name with characters that require JSON escaping.
        reg.register("db\"\n", move || {
            c.fetch_add(1, Ordering::Relaxed);
            HealthStatus::Up
        });
        let json = reg.to_json();
        // Runs once total (aggregate reuses the snapshot, not a second run).
        assert_eq!(calls.load(Ordering::Relaxed), 1, "json={json}");
        // The name is escaped, keeping the JSON valid.
        assert!(json.contains(r#""db\"\n":"up""#), "json={json}");
    }
}
