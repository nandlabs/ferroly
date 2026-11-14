//! Health / readiness probes — a small registry of named checks that a service
//! can expose (e.g. at `/health` + `/ready`). Any subsystem can register a
//! check without implementing the lifecycle trait, and a
//! [`ComponentManager`] can be attached so component state feeds the same
//! report.
//!
//! [`ComponentManager`]: crate::lifecycle::ComponentManager

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::Instant;

use ferroly::lifecycle::manager::{AbortOnDrop, Shared};
use ferroly::lifecycle::{BoxFuture, ComponentManager, ComponentState};

/// Default bound on one asynchronous health check ([`CheckOptions::timeout`]).
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The health of a single check or the aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// The worse of two statuses (`Down` > `Degraded` > `Up`).
    pub fn worst(self, other: HealthStatus) -> HealthStatus {
        match (self, other) {
            (HealthStatus::Down, _) | (_, HealthStatus::Down) => HealthStatus::Down,
            (HealthStatus::Degraded, _) | (_, HealthStatus::Degraded) => HealthStatus::Degraded,
            _ => HealthStatus::Up,
        }
    }
}

/// The health implied by a lifecycle state alone: `Running` is
/// [`Up`](HealthStatus::Up), `Error` is [`Down`](HealthStatus::Down), and every
/// other state (never started, starting, stopping, stopped) is
/// [`Degraded`](HealthStatus::Degraded) — not serving, but not failed.
impl From<ComponentState> for HealthStatus {
    fn from(state: ComponentState) -> Self {
        match state {
            ComponentState::Running => HealthStatus::Up,
            ComponentState::Error => HealthStatus::Down,
            ComponentState::Unknown
            | ComponentState::Starting
            | ComponentState::Stopping
            | ComponentState::Stopped => HealthStatus::Degraded,
        }
    }
}

/// The probe a check contributes to, mirroring the three Kubernetes probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeKind {
    /// "Is the process alive?" — a failure means it should be restarted.
    Liveness,
    /// "Can it take traffic right now?" — a failure removes it from rotation.
    Readiness,
    /// "Has it finished starting?" — gates the other two while booting.
    Startup,
}

impl ProbeKind {
    fn bit(self) -> u8 {
        match self {
            ProbeKind::Liveness => 0b001,
            ProbeKind::Readiness => 0b010,
            ProbeKind::Startup => 0b100,
        }
    }
}

const ALL_PROBES: u8 = 0b111;

/// How a check is grouped and executed
/// ([`HealthRegistry::register_with`] / [`HealthRegistry::register_async`]).
///
/// ```
/// use std::time::Duration;
/// use ferroly::lifecycle::{CheckOptions, ProbeKind};
///
/// // A readiness-only check, bounded to 2 s, re-run at most every 10 s.
/// let opts = CheckOptions::new()
///     .probes(&[ProbeKind::Readiness])
///     .timeout(Duration::from_secs(2))
///     .cache_for(Duration::from_secs(10));
/// assert!(opts.applies_to(ProbeKind::Readiness));
/// assert!(!opts.applies_to(ProbeKind::Liveness));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOptions {
    probes: u8,
    timeout: Duration,
    cache_ttl: Option<Duration>,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            probes: ALL_PROBES,
            timeout: DEFAULT_CHECK_TIMEOUT,
            cache_ttl: None,
        }
    }
}

impl CheckOptions {
    /// A check in every probe group, bounded by [`DEFAULT_CHECK_TIMEOUT`], with
    /// no result caching.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restricts the check to the given probe groups (an empty slice keeps it
    /// out of every group; it then shows up only in the ungrouped report).
    pub fn probes(mut self, probes: &[ProbeKind]) -> Self {
        self.probes = probes.iter().fold(0, |bits, p| bits | p.bit());
        self
    }

    /// Bounds an asynchronous check: one that has not answered within `timeout`
    /// is cancelled and counted as [`Down`](HealthStatus::Down). Synchronous
    /// checks cannot be interrupted, so the bound does not apply to them.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Reuses a result for `ttl` before running the check again, so a probe
    /// that is polled every second does not hit the dependency every second.
    pub fn cache_for(mut self, ttl: Duration) -> Self {
        self.cache_ttl = Some(ttl);
        self
    }

    /// Whether the check belongs to `probe`.
    pub fn applies_to(&self, probe: ProbeKind) -> bool {
        self.probes & probe.bit() != 0
    }
}

/// A point-in-time health report: `(name, status)` pairs in registration order
/// (component entries, sorted by id, come last).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthReport {
    checks: Vec<(String, HealthStatus)>,
}

impl HealthReport {
    /// Wraps a list of `(name, status)` pairs.
    pub fn new(checks: Vec<(String, HealthStatus)>) -> Self {
        Self { checks }
    }

    /// The individual results.
    pub fn checks(&self) -> &[(String, HealthStatus)] {
        &self.checks
    }

    /// Consumes the report, returning the individual results.
    pub fn into_checks(self) -> Vec<(String, HealthStatus)> {
        self.checks
    }

    /// The status of the check called `name`, if present.
    pub fn get(&self, name: &str) -> Option<HealthStatus> {
        self.checks
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, status)| *status)
    }

    /// The aggregate status: `Down` if any check is down, else `Degraded` if
    /// any is degraded, else `Up` (also `Up` when empty).
    pub fn overall(&self) -> HealthStatus {
        self.checks
            .iter()
            .fold(HealthStatus::Up, |worst, (_, status)| worst.worst(*status))
    }

    /// Whether every check is fully [`Up`](HealthStatus::Up) — the readiness
    /// (and startup) signal.
    pub fn is_ready(&self) -> bool {
        self.checks.iter().all(|(_, s)| *s == HealthStatus::Up)
    }

    /// Whether no check is [`Down`](HealthStatus::Down) — the liveness signal.
    pub fn is_live(&self) -> bool {
        self.overall() != HealthStatus::Down
    }

    /// The report as a JSON object `{ "overall": "...", "checks": { name: "..." } }`,
    /// with check names JSON-escaped.
    pub fn to_json(&self) -> String {
        let mut s = format!(
            "{{\"overall\":\"{}\",\"checks\":{{",
            self.overall().as_str()
        );
        for (i, (name, status)) in self.checks.iter().enumerate() {
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

type SyncCheck = Arc<dyn Fn() -> HealthStatus + Send + Sync>;
type AsyncCheck = Arc<dyn Fn() -> BoxFuture<'static, HealthStatus> + Send + Sync>;

enum CheckFn {
    Sync(SyncCheck),
    Async(AsyncCheck),
}

struct CheckEntry {
    name: String,
    run: CheckFn,
    options: CheckOptions,
    /// The most recent result and when it was produced.
    last: Mutex<Option<(Instant, HealthStatus)>>,
}

impl CheckEntry {
    /// The cached result, if caching is on and it has not expired.
    fn fresh(&self) -> Option<HealthStatus> {
        let ttl = self.options.cache_ttl?;
        let (at, status) = (*self.last.lock().unwrap())?;
        (Instant::now().saturating_duration_since(at) < ttl).then_some(status)
    }

    /// The most recent result, however old.
    fn last_known(&self) -> Option<HealthStatus> {
        (*self.last.lock().unwrap()).map(|(_, status)| status)
    }

    fn store(&self, status: HealthStatus) {
        *self.last.lock().unwrap() = Some((Instant::now(), status));
    }
}

#[derive(Default)]
struct RegistryInner {
    checks: Vec<Arc<CheckEntry>>,
    managers: Vec<Weak<Shared>>,
}

/// A registry of named health checks. Cheaply cloneable (checks are shared).
///
/// There are two ways to evaluate it:
///
/// - **Asynchronously** — [`check`](Self::check) / [`check_probe`](Self::check_probe)
///   run every check, the asynchronous ones concurrently and each bounded by
///   its timeout. This is the API a probe endpoint should use.
/// - **Synchronously** — [`report`](Self::report), [`overall`](Self::overall),
///   [`is_ready`](Self::is_ready) and [`to_json`](Self::to_json) run the
///   synchronous checks inline and, because they cannot await, report each
///   asynchronous check's **most recent** result
///   ([`Down`](HealthStatus::Down) until it has run once).
#[derive(Default, Clone)]
pub struct HealthRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl HealthRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a named synchronous check in every probe group. Checks should
    /// be fast and non-blocking; use [`register_async`](Self::register_async)
    /// for anything that does I/O.
    pub fn register<F>(&self, name: impl Into<String>, check: F)
    where
        F: Fn() -> HealthStatus + Send + Sync + 'static,
    {
        self.register_with(name, CheckOptions::default(), check);
    }

    /// Registers a named synchronous check with explicit [`CheckOptions`]
    /// (probe groups, result caching). The timeout does not apply: a
    /// synchronous check cannot be interrupted.
    pub fn register_with<F>(&self, name: impl Into<String>, options: CheckOptions, check: F)
    where
        F: Fn() -> HealthStatus + Send + Sync + 'static,
    {
        self.push(name.into(), options, CheckFn::Sync(Arc::new(check)));
    }

    /// Registers a named asynchronous check — the place for a database ping or
    /// a downstream call.
    ///
    /// Each evaluation runs on its own task, bounded by
    /// [`CheckOptions::timeout`]; a check that times out or panics counts as
    /// [`Down`](HealthStatus::Down). With [`CheckOptions::cache_for`] the
    /// result is reused until it expires.
    ///
    /// ```
    /// use ferroly::lifecycle::{CheckOptions, HealthRegistry, HealthStatus, ProbeKind};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let registry = HealthRegistry::new();
    /// registry.register_async(
    ///     "db",
    ///     CheckOptions::new().probes(&[ProbeKind::Readiness]),
    ///     || async { HealthStatus::Up }, // e.g. `SELECT 1`
    /// );
    /// assert!(registry.check_probe(ProbeKind::Readiness).await.is_ready());
    /// # }
    /// ```
    pub fn register_async<F, Fut>(&self, name: impl Into<String>, options: CheckOptions, check: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HealthStatus> + Send + 'static,
    {
        let run: AsyncCheck = Arc::new(move || Box::pin(check()));
        self.push(name.into(), options, CheckFn::Async(run));
    }

    /// Adds every component of `manager` to this registry's reports, one entry
    /// per component id with the status described at
    /// [`ComponentManager::health`]. Component entries belong to every probe
    /// group: readiness and startup pass only once all components are
    /// `Running`, and liveness fails when one is in `Error`.
    ///
    /// Only a weak reference is kept, so a registry held by a component (an
    /// HTTP server exposing it, say) does not keep the manager alive; the
    /// entries disappear when the manager is dropped.
    pub fn register_components(&self, manager: &ComponentManager) {
        self.inner
            .lock()
            .unwrap()
            .managers
            .push(Arc::downgrade(manager.shared()));
    }

    fn push(&self, name: String, options: CheckOptions, run: CheckFn) {
        self.inner.lock().unwrap().checks.push(Arc::new(CheckEntry {
            name,
            run,
            options,
            last: Mutex::new(None),
        }));
    }

    /// Snapshots the checks of `probe` (all when `None`) and the live managers,
    /// so nothing runs while the registry lock is held.
    fn snapshot(&self, probe: Option<ProbeKind>) -> (Vec<Arc<CheckEntry>>, Vec<Arc<Shared>>) {
        let mut inner = self.inner.lock().unwrap();
        inner.managers.retain(|m| m.strong_count() > 0);
        let checks = inner
            .checks
            .iter()
            .filter(|c| probe.map_or(true, |p| c.options.applies_to(p)))
            .cloned()
            .collect();
        let managers = inner.managers.iter().filter_map(Weak::upgrade).collect();
        (checks, managers)
    }

    fn report_sync(&self, probe: Option<ProbeKind>) -> HealthReport {
        let (checks, managers) = self.snapshot(probe);
        let mut out: Vec<(String, HealthStatus)> = checks
            .iter()
            .map(|entry| {
                let status = match &entry.run {
                    CheckFn::Sync(check) => match entry.fresh() {
                        Some(cached) => cached,
                        None => {
                            let status = run_sync(check);
                            entry.store(status);
                            status
                        }
                    },
                    CheckFn::Async(_) => entry.last_known().unwrap_or(HealthStatus::Down),
                };
                (entry.name.clone(), status)
            })
            .collect();
        for manager in managers {
            out.extend(manager.health_checks());
        }
        HealthReport::new(out)
    }

    async fn report_async(&self, probe: Option<ProbeKind>) -> HealthReport {
        enum Slot {
            /// Still fresh in the cache: reported without touching its age.
            Cached(HealthStatus),
            Done(HealthStatus),
            Running(AbortOnDrop<HealthStatus>),
        }

        let (checks, managers) = self.snapshot(probe);
        // Launch every asynchronous check first so they overlap, then collect
        // in registration order.
        let slots: Vec<Slot> = checks
            .iter()
            .map(|entry| {
                if let Some(cached) = entry.fresh() {
                    return Slot::Cached(cached);
                }
                match &entry.run {
                    CheckFn::Sync(check) => Slot::Done(run_sync(check)),
                    CheckFn::Async(check) => {
                        let check = check.clone();
                        let limit = entry.options.timeout;
                        // The closure is called inside the task so that a panic
                        // while *building* the future is contained too.
                        Slot::Running(AbortOnDrop(tokio::spawn(async move {
                            match tokio::time::timeout(limit, check()).await {
                                Ok(status) => status,
                                Err(_) => HealthStatus::Down,
                            }
                        })))
                    }
                }
            })
            .collect();

        let mut out = Vec::with_capacity(checks.len());
        for (entry, slot) in checks.iter().zip(slots) {
            let status = match slot {
                Slot::Cached(status) => {
                    out.push((entry.name.clone(), status));
                    continue;
                }
                Slot::Done(status) => status,
                // A panicked (or aborted) check is not healthy.
                Slot::Running(mut task) => (&mut task.0).await.unwrap_or(HealthStatus::Down),
            };
            entry.store(status);
            out.push((entry.name.clone(), status));
        }
        for manager in managers {
            out.extend(manager.health_checks());
        }
        HealthReport::new(out)
    }

    /// Runs every check — asynchronous ones concurrently, each bounded by its
    /// timeout — and returns the full report.
    pub async fn check(&self) -> HealthReport {
        self.report_async(None).await
    }

    /// Like [`check`](Self::check), restricted to the checks of one probe
    /// group. Use `check_probe(ProbeKind::Liveness).await.is_live()` for a
    /// liveness endpoint and `check_probe(ProbeKind::Readiness).await.is_ready()`
    /// for readiness.
    pub async fn check_probe(&self, probe: ProbeKind) -> HealthReport {
        self.report_async(Some(probe)).await
    }

    /// The synchronous report restricted to one probe group; see
    /// [`report`](Self::report) for how asynchronous checks appear.
    pub fn report_probe(&self, probe: ProbeKind) -> HealthReport {
        self.report_sync(Some(probe))
    }

    /// Runs every synchronous check, returning `(name, status)` pairs. Checks
    /// are snapshotted under the lock (they're `Arc`, so cloning is cheap) and
    /// then run **without holding it**, so one slow check can't block
    /// registration or other reports. A panicking check counts as
    /// [`Down`](HealthStatus::Down).
    ///
    /// Asynchronous checks cannot be awaited here: each contributes its most
    /// recent result from [`check`](Self::check)/[`check_probe`](Self::check_probe),
    /// or [`Down`](HealthStatus::Down) if it has never run. Prefer the
    /// asynchronous API when any are registered.
    pub fn report(&self) -> Vec<(String, HealthStatus)> {
        self.report_sync(None).into_checks()
    }

    /// The aggregate status: `Down` if any check is down, else `Degraded` if any
    /// is degraded, else `Up` (also `Up` when empty).
    pub fn overall(&self) -> HealthStatus {
        self.report_sync(None).overall()
    }

    /// Whether every check is fully [`Up`](HealthStatus::Up) — the readiness signal.
    pub fn is_ready(&self) -> bool {
        self.report_sync(None).is_ready()
    }

    /// The report as a JSON object `{ "overall": "...", "checks": { name: "..." } }`.
    ///
    /// Every check runs **exactly once** (the aggregate is derived from the same
    /// snapshot, so side-effecting checks aren't double-invoked), and check names
    /// are JSON-escaped.
    pub fn to_json(&self) -> String {
        self.report_sync(None).to_json()
    }
}

/// Runs a synchronous check, treating a panic as `Down`.
fn run_sync(check: &SyncCheck) -> HealthStatus {
    catch_unwind(AssertUnwindSafe(|| check())).unwrap_or(HealthStatus::Down)
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

    #[test]
    fn a_panicking_sync_check_is_down_not_a_crash() {
        let reg = HealthRegistry::new();
        reg.register("boom", || panic!("check exploded"));
        reg.register("fine", || HealthStatus::Up);
        assert_eq!(
            reg.report(),
            vec![
                ("boom".to_string(), HealthStatus::Down),
                ("fine".to_string(), HealthStatus::Up)
            ]
        );
    }

    #[test]
    fn worst_orders_statuses() {
        use HealthStatus::{Degraded, Down, Up};
        assert_eq!(Up.worst(Up), Up);
        assert_eq!(Up.worst(Degraded), Degraded);
        assert_eq!(Degraded.worst(Down), Down);
        assert_eq!(Down.worst(Up), Down);
    }
}
