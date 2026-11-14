//! A tiny, dependency-free metrics registry with Prometheus text exposition.
//!
//! Five instrument types — [`Counter`], [`FloatCounter`], [`Gauge`],
//! [`FloatGauge`] and [`Histogram`] — are registered in a [`Registry`] under a
//! metric name and a set of labels. Each distinct label set is its own time
//! series. Instruments are handed back as `Arc<…>` so the hot path only does
//! relaxed atomic operations (no lock); cache the handle, or pre-bind label
//! values through a [`MetricVec`] ([`CounterVec`], [`HistogramVec`], …).
//!
//! The metric **kind is a property of the family** (the name). Registering a
//! name a second time with a different kind, an invalid metric or label name, a
//! reserved label, or a duplicated label is a [`MetricsError`]: the `try_*`
//! constructors return it, while the infallible constructors hand back a
//! *detached* instrument (it works, but is never exported) and count the
//! failure in [`Registry::registration_errors`]. Either way the exposition
//! output stays valid.
//!
//! A per-family **cardinality guard** caps the number of label sets
//! ([`DEFAULT_MAX_SERIES`] unless reconfigured). Label sets beyond the cap all
//! share one overflow series labelled `overflow="true"`, so totals stay correct
//! while memory stays bounded.
//!
//! [`Registry::encode`] renders the Prometheus text format (version 0.0.4) and
//! [`Registry::encode_openmetrics`] the OpenMetrics 1.0 text format, ready to
//! serve from a `/metrics` endpoint. A process-global [`global`] registry is
//! provided for the common single-registry case; the `turbo` router records RED
//! metrics into it via [`Router::metrics`](../turbo/struct.Router.html) when the
//! `metrics` feature is on.
//!
//! ```
//! use ferroly::metrics::Registry;
//!
//! let reg = Registry::new();
//! let hits = reg.counter("hits_total", "Total hits", &[("route", "/")]);
//! hits.inc();
//! assert!(reg.encode().contains("hits_total{route=\"/\"} 1"));
//!
//! // Pre-bound label values, an RAII timer and an in-flight guard.
//! let latency = reg.histogram_vec("latency_seconds", "Latency", &["method"], &[0.1, 1.0]);
//! let in_flight = reg.gauge("in_flight", "In-flight requests", &[]);
//! {
//!     let _busy = in_flight.track();
//!     let _timer = latency.with_labels(&["GET"]).start_timer();
//! } // both guards record on drop
//! assert_eq!(in_flight.get(), 0);
//! assert_eq!(latency.with_labels(&["GET"]).count(), 1);
//! ```

#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

mod process;

/// The `Content-Type` for [`Registry::encode`] output.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The `Content-Type` for [`Registry::encode_openmetrics`] output.
pub const OPENMETRICS_CONTENT_TYPE: &str =
    "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// The default per-family cap on distinct label sets (see
/// [`Registry::set_default_max_series`]).
pub const DEFAULT_MAX_SERIES: usize = 2000;

/// The label carried (with the value `"true"`) by a family's overflow series —
/// the single series that absorbs every label set beyond the cardinality cap.
pub const OVERFLOW_LABEL: &str = "overflow";

// ---- lock helpers (a poisoned lock keeps serving its data) -----------------

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn rlock<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn wlock<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// Atomically applies `f` to an `f64` stored as bits in an `AtomicU64`.
fn update_f64(cell: &AtomicU64, f: impl Fn(f64) -> f64) {
    let mut cur = cell.load(Ordering::Relaxed);
    loop {
        let next = f(f64::from_bits(cur)).to_bits();
        match cell.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => cur = observed,
        }
    }
}

// ---- instruments -----------------------------------------------------------

/// A monotonically increasing integer counter.
#[derive(Debug, Default)]
pub struct Counter {
    v: AtomicU64,
}

impl Counter {
    /// Increments by one.
    pub fn inc(&self) {
        self.v.fetch_add(1, Ordering::Relaxed);
    }
    /// Increments by `n`.
    pub fn inc_by(&self, n: u64) {
        self.v.fetch_add(n, Ordering::Relaxed);
    }
    /// The current value.
    pub fn get(&self) -> u64 {
        self.v.load(Ordering::Relaxed)
    }
}

/// A monotonically increasing floating-point counter (for example seconds of
/// CPU time or bytes scaled to a fractional unit).
#[derive(Debug, Default)]
pub struct FloatCounter {
    bits: AtomicU64,
}

impl FloatCounter {
    /// Increments by one.
    pub fn inc(&self) {
        self.inc_by(1.0);
    }
    /// Increments by `v`. A counter never goes down, so a negative, `NaN` or
    /// infinite `v` is ignored.
    pub fn inc_by(&self, v: f64) {
        if v.is_finite() && v > 0.0 {
            update_f64(&self.bits, |cur| cur + v);
        }
    }
    /// The current value.
    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }
    /// Raises the counter to `total` when that is larger (used by collectors
    /// that mirror an externally maintained monotonic total).
    fn raise_to(&self, total: f64) {
        if total.is_finite() {
            update_f64(&self.bits, |cur| if total > cur { total } else { cur });
        }
    }
}

/// An integer value that can go up and down (e.g. in-flight requests).
#[derive(Debug, Default)]
pub struct Gauge {
    v: AtomicI64,
}

impl Gauge {
    /// Sets the value.
    pub fn set(&self, n: i64) {
        self.v.store(n, Ordering::Relaxed);
    }
    /// Increments by one.
    pub fn inc(&self) {
        self.v.fetch_add(1, Ordering::Relaxed);
    }
    /// Decrements by one.
    pub fn dec(&self) {
        self.v.fetch_sub(1, Ordering::Relaxed);
    }
    /// Adds `n` (which may be negative).
    pub fn add(&self, n: i64) {
        self.v.fetch_add(n, Ordering::Relaxed);
    }
    /// Subtracts `n`.
    pub fn sub(&self, n: i64) {
        self.v.fetch_sub(n, Ordering::Relaxed);
    }
    /// The current value.
    pub fn get(&self) -> i64 {
        self.v.load(Ordering::Relaxed)
    }
    /// Increments the gauge and returns a guard that decrements it again when
    /// dropped — so an in-flight count cannot leak when a future is cancelled
    /// or a handler panics.
    pub fn track(self: &Arc<Self>) -> InFlightGuard {
        self.inc();
        InFlightGuard {
            gauge: Arc::clone(self),
        }
    }
}

/// An RAII guard from [`Gauge::track`]: the gauge was incremented when the
/// guard was created and is decremented when it drops.
#[must_use = "the gauge is decremented as soon as the guard is dropped"]
#[derive(Debug)]
pub struct InFlightGuard {
    gauge: Arc<Gauge>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.gauge.dec();
    }
}

/// A floating-point value that can go up and down (ratios, temperatures,
/// seconds).
#[derive(Debug, Default)]
pub struct FloatGauge {
    bits: AtomicU64,
}

impl FloatGauge {
    /// Sets the value.
    pub fn set(&self, v: f64) {
        self.bits.store(v.to_bits(), Ordering::Relaxed);
    }
    /// Increments by one.
    pub fn inc(&self) {
        self.add(1.0);
    }
    /// Decrements by one.
    pub fn dec(&self) {
        self.add(-1.0);
    }
    /// Adds `v` (which may be negative). A `NaN` delta is ignored.
    pub fn add(&self, v: f64) {
        if !v.is_nan() {
            update_f64(&self.bits, |cur| cur + v);
        }
    }
    /// Subtracts `v`. A `NaN` delta is ignored.
    pub fn sub(&self, v: f64) {
        self.add(-v);
    }
    /// The current value.
    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }
}

/// Default histogram bucket upper bounds (seconds), matching Prometheus'
/// client-library defaults — good for request-latency observations.
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Normalises bucket bounds: non-finite values dropped (the `+Inf` bucket is
/// implicit), sorted ascending, duplicates removed.
fn normalise_bounds(bounds: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = bounds.iter().copied().filter(|b| b.is_finite()).collect();
    out.sort_by(f64::total_cmp);
    // `-0.0 == 0.0`, so `dedup` also folds the two zeroes into one bucket.
    out.dedup();
    out
}

/// A cumulative histogram over fixed bucket bounds, plus a running sum.
#[derive(Debug)]
pub struct Histogram {
    /// Ascending, de-duplicated, finite upper bounds. An observation falls in
    /// the first bound it is `<=`; anything larger lands in the implicit `+Inf`
    /// bucket.
    bounds: Vec<f64>,
    /// Per-bucket (non-cumulative) counts; `counts[bounds.len()]` is `+Inf`.
    counts: Vec<AtomicU64>,
    sum_bits: AtomicU64,
}

impl Histogram {
    /// A standalone histogram that belongs to no registry. `bounds` may be in
    /// any order; non-finite bounds are dropped and duplicates removed.
    pub fn new(bounds: &[f64]) -> Self {
        let bounds = normalise_bounds(bounds);
        let n = bounds.len() + 1; // +1 for the +Inf overflow bucket
        Self {
            bounds,
            counts: (0..n).map(|_| AtomicU64::new(0)).collect(),
            sum_bits: AtomicU64::new(0),
        }
    }

    /// Records one observation. `NaN` and infinite values are ignored, so one
    /// bad sample cannot poison `_sum`.
    pub fn observe(&self, v: f64) {
        if !v.is_finite() {
            return;
        }
        let idx = self.bounds.partition_point(|&b| b < v);
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
        update_f64(&self.sum_bits, |cur| cur + v);
    }

    /// The number of observations.
    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// The sum of all observed values.
    pub fn sum(&self) -> f64 {
        f64::from_bits(self.sum_bits.load(Ordering::Relaxed))
    }

    /// The finite bucket upper bounds, ascending (the `+Inf` bucket is implicit).
    pub fn bounds(&self) -> &[f64] {
        &self.bounds
    }

    /// A snapshot of the **cumulative** bucket counts as `(upper_bound, count)`
    /// pairs. The last pair is the `+Inf` bucket, whose count equals
    /// [`count`](Self::count) for the same snapshot.
    pub fn buckets(&self) -> Vec<(f64, u64)> {
        let mut cumulative = 0u64;
        let mut out = Vec::with_capacity(self.counts.len());
        for (i, c) in self.counts.iter().enumerate() {
            cumulative += c.load(Ordering::Relaxed);
            let bound = self.bounds.get(i).copied().unwrap_or(f64::INFINITY);
            out.push((bound, cumulative));
        }
        out
    }

    /// Starts a timer that records the elapsed time, in seconds, into this
    /// histogram when it is dropped (or when
    /// [`observe_duration`](HistogramTimer::observe_duration) is called). Because
    /// the observation happens in `Drop`, a cancelled future still records.
    pub fn start_timer(self: &Arc<Self>) -> HistogramTimer {
        HistogramTimer {
            hist: Arc::clone(self),
            start: Instant::now(),
            armed: true,
        }
    }

    fn reset(&self) {
        for c in &self.counts {
            c.store(0, Ordering::Relaxed);
        }
        self.sum_bits.store(0, Ordering::Relaxed);
    }
}

/// An RAII timer from [`Histogram::start_timer`]: observes the elapsed seconds
/// into its histogram when dropped.
#[must_use = "the duration is observed as soon as the timer is dropped"]
#[derive(Debug)]
pub struct HistogramTimer {
    hist: Arc<Histogram>,
    start: Instant,
    armed: bool,
}

impl HistogramTimer {
    /// Stops the timer now, records the elapsed seconds and returns them.
    pub fn observe_duration(mut self) -> f64 {
        self.armed = false;
        let secs = self.start.elapsed().as_secs_f64();
        self.hist.observe(secs);
        secs
    }

    /// Stops the timer without recording anything.
    pub fn discard(mut self) {
        self.armed = false;
    }
}

impl Drop for HistogramTimer {
    fn drop(&mut self) {
        if self.armed {
            self.hist.observe(self.start.elapsed().as_secs_f64());
        }
    }
}

// ---- kinds and errors ------------------------------------------------------

/// The kind of instrument a metric family holds. Fixed by the first
/// registration of the family's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// An integer [`Counter`].
    Counter,
    /// A [`FloatCounter`].
    FloatCounter,
    /// An integer [`Gauge`].
    Gauge,
    /// A [`FloatGauge`].
    FloatGauge,
    /// A [`Histogram`].
    Histogram,
}

impl MetricKind {
    /// The exposition `# TYPE` keyword: `"counter"`, `"gauge"` or `"histogram"`.
    pub fn type_str(self) -> &'static str {
        match self {
            MetricKind::Counter | MetricKind::FloatCounter => "counter",
            MetricKind::Gauge | MetricKind::FloatGauge => "gauge",
            MetricKind::Histogram => "histogram",
        }
    }
}

/// Why a metric could not be registered or looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsError {
    /// The metric name does not match `[a-zA-Z_:][a-zA-Z0-9_:]*`.
    InvalidMetricName(String),
    /// A label name does not match `[a-zA-Z_][a-zA-Z0-9_]*`, or starts with the
    /// reserved `__` prefix.
    InvalidLabelName {
        /// The metric being registered.
        metric: String,
        /// The offending label name.
        label: String,
    },
    /// A label the exposition format reserves for the instrument itself (`le`
    /// and `quantile` on a histogram).
    ReservedLabel {
        /// The metric being registered.
        metric: String,
        /// The reserved label name.
        label: String,
    },
    /// The same label name appears twice in one label set.
    DuplicateLabel {
        /// The metric being registered.
        metric: String,
        /// The repeated label name.
        label: String,
    },
    /// The name is already registered with a different kind.
    KindMismatch {
        /// The metric name.
        name: String,
        /// The kind the family was first registered with.
        registered: MetricKind,
        /// The kind requested now.
        requested: MetricKind,
    },
    /// The name would produce the same sample names as an existing family —
    /// a histogram `x` owns `x_bucket`, `x_sum` and `x_count`.
    NameCollision {
        /// The name being registered.
        name: String,
        /// The already-registered family it collides with.
        existing: String,
    },
    /// A [`MetricVec`] was given the wrong number of label values.
    LabelArity {
        /// The metric name.
        metric: String,
        /// The number of label names the vec was declared with.
        expected: usize,
        /// The number of values supplied.
        got: usize,
    },
}

impl fmt::Display for MetricsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricsError::InvalidMetricName(name) => {
                write!(f, "invalid metric name {name:?}")
            }
            MetricsError::InvalidLabelName { metric, label } => {
                write!(f, "invalid label name {label:?} on metric {metric:?}")
            }
            MetricsError::ReservedLabel { metric, label } => {
                write!(f, "label {label:?} is reserved on metric {metric:?}")
            }
            MetricsError::DuplicateLabel { metric, label } => {
                write!(f, "label {label:?} given twice on metric {metric:?}")
            }
            MetricsError::KindMismatch {
                name,
                registered,
                requested,
            } => write!(
                f,
                "metric {name:?} is registered as {registered:?}, requested as {requested:?}"
            ),
            MetricsError::NameCollision { name, existing } => write!(
                f,
                "metric {name:?} collides with the samples of registered metric {existing:?}"
            ),
            MetricsError::LabelArity {
                metric,
                expected,
                got,
            } => write!(
                f,
                "metric {metric:?} takes {expected} label value(s), got {got}"
            ),
        }
    }
}

impl std::error::Error for MetricsError {}

/// True when `name` matches the Prometheus metric-name grammar
/// `[a-zA-Z_:][a-zA-Z0-9_:]*`.
pub fn is_valid_metric_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b':' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':')
}

/// True when `name` matches the Prometheus label-name grammar
/// `[a-zA-Z_][a-zA-Z0-9_]*` and does not use the reserved `__` prefix.
pub fn is_valid_label_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    !name.starts_with("__") && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

type LabelSet = Vec<(String, String)>;

fn check_label_name(metric: &str, kind: MetricKind, label: &str) -> Result<(), MetricsError> {
    if !is_valid_label_name(label) {
        return Err(MetricsError::InvalidLabelName {
            metric: metric.to_string(),
            label: label.to_string(),
        });
    }
    if kind == MetricKind::Histogram && (label == "le" || label == "quantile") {
        return Err(MetricsError::ReservedLabel {
            metric: metric.to_string(),
            label: label.to_string(),
        });
    }
    Ok(())
}

/// Validates label names for `metric` and returns the set sorted by name.
fn checked_labels(
    metric: &str,
    kind: MetricKind,
    labels: &[(&str, &str)],
) -> Result<LabelSet, MetricsError> {
    let mut out: LabelSet = Vec::with_capacity(labels.len());
    for (k, v) in labels {
        check_label_name(metric, kind, k)?;
        out.push((k.to_string(), v.to_string()));
    }
    out.sort();
    if let Some(pair) = out.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(MetricsError::DuplicateLabel {
            metric: metric.to_string(),
            label: pair[0].0.clone(),
        });
    }
    Ok(out)
}

// ---- the sealed instrument trait -------------------------------------------

mod sealed {
    use super::{Arc, Counter, FloatCounter, FloatGauge, Gauge, Histogram, MetricKind};

    /// One series' instrument, type-erased.
    pub enum Slot {
        Counter(Arc<Counter>),
        FloatCounter(Arc<FloatCounter>),
        Gauge(Arc<Gauge>),
        FloatGauge(Arc<FloatGauge>),
        Histogram(Arc<Histogram>),
    }

    pub trait Sealed: Sized + Send + Sync + 'static {
        const KIND: MetricKind;
        fn create(bounds: &[f64]) -> Self;
        fn wrap(this: Arc<Self>) -> Slot;
        fn unwrap(slot: &Slot) -> Option<Arc<Self>>;
    }

    macro_rules! seal {
        ($ty:ident, |$b:ident| $new:expr) => {
            impl Sealed for $ty {
                const KIND: MetricKind = MetricKind::$ty;
                fn create($b: &[f64]) -> Self {
                    $new
                }
                fn wrap(this: Arc<Self>) -> Slot {
                    Slot::$ty(this)
                }
                fn unwrap(slot: &Slot) -> Option<Arc<Self>> {
                    match slot {
                        Slot::$ty(inner) => Some(Arc::clone(inner)),
                        _ => None,
                    }
                }
            }
            impl super::Metric for $ty {}
        };
    }

    seal!(Counter, |_b| Counter::default());
    seal!(FloatCounter, |_b| FloatCounter::default());
    seal!(Gauge, |_b| Gauge::default());
    seal!(FloatGauge, |_b| FloatGauge::default());
    seal!(Histogram, |b| Histogram::new(b));
}

use sealed::Slot;

/// An instrument type a [`Registry`] can hold: [`Counter`], [`FloatCounter`],
/// [`Gauge`], [`FloatGauge`] or [`Histogram`]. Sealed — it cannot be
/// implemented outside this module.
pub trait Metric: sealed::Sealed {}

impl Slot {
    fn reset(&self) {
        match self {
            Slot::Counter(c) => c.v.store(0, Ordering::Relaxed),
            Slot::FloatCounter(c) => c.bits.store(0, Ordering::Relaxed),
            Slot::Gauge(g) => g.v.store(0, Ordering::Relaxed),
            Slot::FloatGauge(g) => g.bits.store(0, Ordering::Relaxed),
            Slot::Histogram(h) => h.reset(),
        }
    }
}

// ---- families ---------------------------------------------------------------

/// Counters shared between a registry and its families.
#[derive(Default)]
struct Stats {
    errors: AtomicU64,
    last_error: Mutex<Option<MetricsError>>,
    overflows: AtomicU64,
}

impl Stats {
    fn record(&self, err: &MetricsError) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        *lock(&self.last_error) = Some(err.clone());
    }
}

struct Family {
    name: String,
    help: String,
    kind: MetricKind,
    /// Histogram bucket bounds shared by every series of the family.
    bounds: Vec<f64>,
    unit: Mutex<Option<String>>,
    max_series: AtomicUsize,
    series: RwLock<BTreeMap<LabelSet, Slot>>,
    /// The series that absorbs label sets beyond `max_series`.
    overflow: OnceLock<Slot>,
    stats: Arc<Stats>,
}

impl Family {
    fn new(
        name: &str,
        help: &str,
        kind: MetricKind,
        bounds: &[f64],
        max_series: usize,
        stats: Arc<Stats>,
    ) -> Self {
        Family {
            name: name.to_string(),
            help: help.to_string(),
            kind,
            bounds: normalise_bounds(bounds),
            unit: Mutex::new(None),
            max_series: AtomicUsize::new(max_series.max(1)),
            series: RwLock::new(BTreeMap::new()),
            overflow: OnceLock::new(),
            stats,
        }
    }

    /// Returns the series for `key`, creating it on first use. Past the
    /// cardinality cap a new label set resolves to the shared overflow series.
    fn series<T: Metric>(&self, key: LabelSet) -> Arc<T> {
        if let Some(found) = rlock(&self.series).get(&key).and_then(T::unwrap) {
            return found;
        }
        let mut map = wlock(&self.series);
        if let Some(found) = map.get(&key).and_then(T::unwrap) {
            return found;
        }
        if map.len() >= self.max_series.load(Ordering::Relaxed) {
            drop(map);
            self.stats.overflows.fetch_add(1, Ordering::Relaxed);
            let slot = self
                .overflow
                .get_or_init(|| T::wrap(Arc::new(T::create(&self.bounds))));
            return T::unwrap(slot).unwrap_or_else(|| Arc::new(T::create(&self.bounds)));
        }
        let created = Arc::new(T::create(&self.bounds));
        map.insert(key, T::wrap(Arc::clone(&created)));
        created
    }
}

/// Metadata about one registered metric family (see [`Registry::family`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyInfo {
    /// The metric name.
    pub name: String,
    /// The `# HELP` text given at first registration.
    pub help: String,
    /// The unit set with [`Registry::set_unit`], if any.
    pub unit: Option<String>,
    /// The instrument kind.
    pub kind: MetricKind,
    /// The number of distinct label sets currently held (the overflow series
    /// is not counted).
    pub series: usize,
    /// The family's cardinality cap.
    pub max_series: usize,
}

// ---- pre-bound label handles -----------------------------------------------

/// A metric family with a fixed list of label **names**, from which series are
/// selected by label **values** alone. Resolving a series takes only the
/// family's own lock (never the registry lock), and the returned `Arc` handle
/// can be cached to update lock-free.
///
/// ```
/// use ferroly::metrics::Registry;
///
/// let reg = Registry::new();
/// let reqs = reg.counter_vec("http_requests_total", "Requests", &["method", "status"]);
/// reqs.with_labels(&["GET", "200"]).inc();
/// let ok = reqs.with_labels(&["GET", "200"]); // cache this on a hot path
/// ok.inc();
/// assert_eq!(ok.get(), 2);
/// ```
pub struct MetricVec<T: Metric> {
    family: Arc<Family>,
    names: Arc<[String]>,
    /// `names` indices in label-name order (series keys are sorted by name).
    order: Arc<[usize]>,
    _kind: PhantomData<fn() -> T>,
}

/// A [`Counter`] family with pre-declared label names.
pub type CounterVec = MetricVec<Counter>;
/// A [`FloatCounter`] family with pre-declared label names.
pub type FloatCounterVec = MetricVec<FloatCounter>;
/// A [`Gauge`] family with pre-declared label names.
pub type GaugeVec = MetricVec<Gauge>;
/// A [`FloatGauge`] family with pre-declared label names.
pub type FloatGaugeVec = MetricVec<FloatGauge>;
/// A [`Histogram`] family with pre-declared label names.
pub type HistogramVec = MetricVec<Histogram>;

impl<T: Metric> Clone for MetricVec<T> {
    fn clone(&self) -> Self {
        MetricVec {
            family: Arc::clone(&self.family),
            names: Arc::clone(&self.names),
            order: Arc::clone(&self.order),
            _kind: PhantomData,
        }
    }
}

impl<T: Metric> fmt::Debug for MetricVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricVec")
            .field("name", &self.family.name)
            .field("kind", &self.family.kind)
            .field("label_names", &self.names)
            .finish()
    }
}

impl<T: Metric> MetricVec<T> {
    fn build(family: Arc<Family>, names: &[&str]) -> Self {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let mut order: Vec<usize> = (0..names.len()).collect();
        order.sort_by(|&a, &b| names[a].cmp(&names[b]));
        MetricVec {
            family,
            names: names.into(),
            order: order.into(),
            _kind: PhantomData,
        }
    }

    fn key(&self, values: &[&str]) -> Result<LabelSet, MetricsError> {
        if values.len() != self.names.len() {
            return Err(MetricsError::LabelArity {
                metric: self.family.name.clone(),
                expected: self.names.len(),
                got: values.len(),
            });
        }
        Ok(self
            .order
            .iter()
            .map(|&i| (self.names[i].clone(), values[i].to_string()))
            .collect())
    }

    /// The series for `values` (one per declared label name, in declaration
    /// order), created on first use.
    pub fn try_with_labels(&self, values: &[&str]) -> Result<Arc<T>, MetricsError> {
        Ok(self.family.series(self.key(values)?))
    }

    /// Like [`try_with_labels`](Self::try_with_labels), but a wrong number of
    /// values yields a detached instrument (never exported) and is counted in
    /// [`Registry::registration_errors`] instead of returning an error.
    pub fn with_labels(&self, values: &[&str]) -> Arc<T> {
        match self.try_with_labels(values) {
            Ok(series) => series,
            Err(err) => {
                self.family.stats.record(&err);
                Arc::new(T::create(&self.family.bounds))
            }
        }
    }

    /// Removes the series for `values`; `true` when it existed. Handles already
    /// handed out keep working but are no longer exported.
    pub fn remove(&self, values: &[&str]) -> bool {
        match self.key(values) {
            Ok(key) => wlock(&self.family.series).remove(&key).is_some(),
            Err(_) => false,
        }
    }

    /// The metric name.
    pub fn name(&self) -> &str {
        &self.family.name
    }

    /// The declared label names, in declaration order.
    pub fn label_names(&self) -> &[String] {
        &self.names
    }
}

// ---- registry ----------------------------------------------------------------

type Collector = Arc<dyn Fn(&Registry) + Send + Sync>;

/// A collection of metrics, keyed by name and label set, that renders to the
/// Prometheus (or OpenMetrics) text exposition format.
pub struct Registry {
    families: RwLock<BTreeMap<String, Arc<Family>>>,
    default_max_series: AtomicUsize,
    stats: Arc<Stats>,
    collectors: Mutex<Vec<Collector>>,
    process_collector: AtomicBool,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            families: RwLock::new(BTreeMap::new()),
            default_max_series: AtomicUsize::new(DEFAULT_MAX_SERIES),
            stats: Arc::new(Stats::default()),
            collectors: Mutex::new(Vec::new()),
            process_collector: AtomicBool::new(false),
        }
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("families", &rlock(&self.families).len())
            .field("registration_errors", &self.registration_errors())
            .field("series_overflows", &self.series_overflows())
            .finish()
    }
}

const HISTOGRAM_SUFFIXES: [&str; 3] = ["_bucket", "_sum", "_count"];

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty registry whose families hold at most `max` label sets
    /// each (see [`set_default_max_series`](Self::set_default_max_series)).
    pub fn with_max_series(max: usize) -> Self {
        let reg = Self::default();
        reg.set_default_max_series(max);
        reg
    }

    // -- family resolution --

    fn family_for<T: Metric>(
        &self,
        name: &str,
        help: &str,
        bounds: &[f64],
    ) -> Result<Arc<Family>, MetricsError> {
        let check_kind = |fam: &Arc<Family>| {
            if fam.kind == T::KIND {
                Ok(Arc::clone(fam))
            } else {
                Err(MetricsError::KindMismatch {
                    name: name.to_string(),
                    registered: fam.kind,
                    requested: T::KIND,
                })
            }
        };
        if let Some(fam) = rlock(&self.families).get(name) {
            return check_kind(fam);
        }
        if !is_valid_metric_name(name) {
            return Err(MetricsError::InvalidMetricName(name.to_string()));
        }
        let mut fams = wlock(&self.families);
        if let Some(fam) = fams.get(name) {
            return check_kind(fam);
        }
        // A histogram `x` emits `x_bucket`, `x_sum` and `x_count`; refuse any
        // registration that would duplicate those sample names.
        for suffix in HISTOGRAM_SUFFIXES {
            if let Some(stem) = name.strip_suffix(suffix) {
                if fams
                    .get(stem)
                    .is_some_and(|f| f.kind == MetricKind::Histogram)
                {
                    return Err(MetricsError::NameCollision {
                        name: name.to_string(),
                        existing: stem.to_string(),
                    });
                }
            }
            if T::KIND == MetricKind::Histogram {
                let derived = format!("{name}{suffix}");
                if fams.contains_key(&derived) {
                    return Err(MetricsError::NameCollision {
                        name: name.to_string(),
                        existing: derived,
                    });
                }
            }
        }
        let fam = Arc::new(Family::new(
            name,
            help,
            T::KIND,
            bounds,
            self.default_max_series.load(Ordering::Relaxed),
            Arc::clone(&self.stats),
        ));
        fams.insert(name.to_string(), Arc::clone(&fam));
        Ok(fam)
    }

    fn try_series<T: Metric>(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        bounds: &[f64],
    ) -> Result<Arc<T>, MetricsError> {
        // Validate the labels before touching the registry, so a rejected
        // registration never leaves an empty family behind.
        let key = checked_labels(name, T::KIND, labels)?;
        Ok(self.family_for::<T>(name, help, bounds)?.series(key))
    }

    fn series_or_detached<T: Metric>(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        bounds: &[f64],
    ) -> Arc<T> {
        match self.try_series(name, help, labels, bounds) {
            Ok(series) => series,
            Err(err) => {
                self.stats.record(&err);
                Arc::new(T::create(bounds))
            }
        }
    }

    fn try_vec<T: Metric>(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        bounds: &[f64],
    ) -> Result<MetricVec<T>, MetricsError> {
        let placeholder: Vec<(&str, &str)> = label_names.iter().map(|n| (*n, "")).collect();
        checked_labels(name, T::KIND, &placeholder)?;
        let family = self.family_for::<T>(name, help, bounds)?;
        Ok(MetricVec::build(family, label_names))
    }

    fn vec_or_detached<T: Metric>(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        bounds: &[f64],
    ) -> MetricVec<T> {
        match self.try_vec(name, help, label_names, bounds) {
            Ok(vec) => vec,
            Err(err) => {
                self.stats.record(&err);
                // A family that is never inserted into the registry: it hands
                // out working instruments, capped like any other, that are
                // never exported.
                let family = Arc::new(Family::new(
                    name,
                    help,
                    T::KIND,
                    bounds,
                    self.default_max_series.load(Ordering::Relaxed),
                    Arc::clone(&self.stats),
                ));
                MetricVec::build(family, label_names)
            }
        }
    }

    // -- infallible constructors --

    /// Returns the [`Counter`] for `name` + `labels`, creating it (with `help`)
    /// on first use. Repeated calls with the same name/labels return the same
    /// instrument.
    ///
    /// If the registration is invalid (see [`MetricsError`]) the returned
    /// counter is **detached** — usable, but never exported — and the failure
    /// is counted in [`registration_errors`](Self::registration_errors). Use
    /// [`try_counter`](Self::try_counter) to get the error instead.
    pub fn counter(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Arc<Counter> {
        self.series_or_detached(name, help, labels, &[])
    }

    /// Returns the [`FloatCounter`] for `name` + `labels`, creating it on first
    /// use. Invalid registrations yield a detached instrument, as for
    /// [`counter`](Self::counter).
    pub fn float_counter(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
    ) -> Arc<FloatCounter> {
        self.series_or_detached(name, help, labels, &[])
    }

    /// Returns the [`Gauge`] for `name` + `labels`, creating it on first use.
    /// Invalid registrations yield a detached instrument, as for
    /// [`counter`](Self::counter).
    pub fn gauge(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Arc<Gauge> {
        self.series_or_detached(name, help, labels, &[])
    }

    /// Returns the [`FloatGauge`] for `name` + `labels`, creating it on first
    /// use. Invalid registrations yield a detached instrument, as for
    /// [`counter`](Self::counter).
    pub fn float_gauge(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Arc<FloatGauge> {
        self.series_or_detached(name, help, labels, &[])
    }

    /// Returns the [`Histogram`] for `name` + `labels`, creating it on first
    /// use (see [`DEFAULT_BUCKETS`]). The bucket `bounds` belong to the family:
    /// the first registration of `name` fixes them for every series, and later
    /// calls' `bounds` are ignored. Invalid registrations yield a detached
    /// instrument, as for [`counter`](Self::counter).
    pub fn histogram(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        bounds: &[f64],
    ) -> Arc<Histogram> {
        self.series_or_detached(name, help, labels, bounds)
    }

    // -- fallible constructors --

    /// Like [`counter`](Self::counter), but reports an invalid registration.
    pub fn try_counter(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
    ) -> Result<Arc<Counter>, MetricsError> {
        self.try_series(name, help, labels, &[])
    }

    /// Like [`float_counter`](Self::float_counter), but reports an invalid
    /// registration.
    pub fn try_float_counter(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
    ) -> Result<Arc<FloatCounter>, MetricsError> {
        self.try_series(name, help, labels, &[])
    }

    /// Like [`gauge`](Self::gauge), but reports an invalid registration.
    pub fn try_gauge(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
    ) -> Result<Arc<Gauge>, MetricsError> {
        self.try_series(name, help, labels, &[])
    }

    /// Like [`float_gauge`](Self::float_gauge), but reports an invalid
    /// registration.
    pub fn try_float_gauge(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
    ) -> Result<Arc<FloatGauge>, MetricsError> {
        self.try_series(name, help, labels, &[])
    }

    /// Like [`histogram`](Self::histogram), but reports an invalid registration.
    pub fn try_histogram(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        bounds: &[f64],
    ) -> Result<Arc<Histogram>, MetricsError> {
        self.try_series(name, help, labels, bounds)
    }

    // -- pre-bound label vectors --

    /// Declares (or fetches) a counter family with the given label names and
    /// returns a [`CounterVec`] for selecting series by label values. An
    /// invalid registration yields a detached vec and is counted in
    /// [`registration_errors`](Self::registration_errors).
    pub fn counter_vec(&self, name: &str, help: &str, label_names: &[&str]) -> CounterVec {
        self.vec_or_detached(name, help, label_names, &[])
    }

    /// Like [`counter_vec`](Self::counter_vec), but reports an invalid
    /// registration.
    pub fn try_counter_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
    ) -> Result<CounterVec, MetricsError> {
        self.try_vec(name, help, label_names, &[])
    }

    /// Declares (or fetches) a float-counter family; see
    /// [`counter_vec`](Self::counter_vec).
    pub fn float_counter_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
    ) -> FloatCounterVec {
        self.vec_or_detached(name, help, label_names, &[])
    }

    /// Like [`float_counter_vec`](Self::float_counter_vec), but reports an
    /// invalid registration.
    pub fn try_float_counter_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
    ) -> Result<FloatCounterVec, MetricsError> {
        self.try_vec(name, help, label_names, &[])
    }

    /// Declares (or fetches) a gauge family; see
    /// [`counter_vec`](Self::counter_vec).
    pub fn gauge_vec(&self, name: &str, help: &str, label_names: &[&str]) -> GaugeVec {
        self.vec_or_detached(name, help, label_names, &[])
    }

    /// Like [`gauge_vec`](Self::gauge_vec), but reports an invalid registration.
    pub fn try_gauge_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
    ) -> Result<GaugeVec, MetricsError> {
        self.try_vec(name, help, label_names, &[])
    }

    /// Declares (or fetches) a float-gauge family; see
    /// [`counter_vec`](Self::counter_vec).
    pub fn float_gauge_vec(&self, name: &str, help: &str, label_names: &[&str]) -> FloatGaugeVec {
        self.vec_or_detached(name, help, label_names, &[])
    }

    /// Like [`float_gauge_vec`](Self::float_gauge_vec), but reports an invalid
    /// registration.
    pub fn try_float_gauge_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
    ) -> Result<FloatGaugeVec, MetricsError> {
        self.try_vec(name, help, label_names, &[])
    }

    /// Declares (or fetches) a histogram family with the given label names and
    /// bucket `bounds`; see [`counter_vec`](Self::counter_vec).
    pub fn histogram_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        bounds: &[f64],
    ) -> HistogramVec {
        self.vec_or_detached(name, help, label_names, bounds)
    }

    /// Like [`histogram_vec`](Self::histogram_vec), but reports an invalid
    /// registration.
    pub fn try_histogram_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        bounds: &[f64],
    ) -> Result<HistogramVec, MetricsError> {
        self.try_vec(name, help, label_names, bounds)
    }

    // -- diagnostics --

    /// How many registrations the infallible constructors rejected (each one
    /// produced a detached instrument). A non-zero value means some metric in
    /// the code base is misdeclared.
    pub fn registration_errors(&self) -> u64 {
        self.stats.errors.load(Ordering::Relaxed)
    }

    /// The most recent error counted by
    /// [`registration_errors`](Self::registration_errors).
    pub fn last_registration_error(&self) -> Option<MetricsError> {
        lock(&self.stats.last_error).clone()
    }

    /// How many series lookups were redirected to an overflow series because
    /// their family had reached its cardinality cap.
    pub fn series_overflows(&self) -> u64 {
        self.stats.overflows.load(Ordering::Relaxed)
    }

    // -- cardinality --

    /// Sets the cap on distinct label sets for families created **after** this
    /// call (minimum 1; default [`DEFAULT_MAX_SERIES`]).
    pub fn set_default_max_series(&self, max: usize) {
        self.default_max_series.store(max.max(1), Ordering::Relaxed);
    }

    /// Sets the cap on distinct label sets for the existing family `name`
    /// (minimum 1). Returns `false` when no such family is registered. Lowering
    /// the cap keeps the series already present.
    pub fn set_max_series(&self, name: &str, max: usize) -> bool {
        match rlock(&self.families).get(name) {
            Some(fam) => {
                fam.max_series.store(max.max(1), Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    // -- metadata --

    /// Sets the unit of the family `name` (for example `"seconds"`). It is
    /// reported by [`family`](Self::family) and emitted as `# UNIT` in the
    /// OpenMetrics format when the metric name ends in `_<unit>`. Returns
    /// `false` when no such family is registered.
    pub fn set_unit(&self, name: &str, unit: &str) -> bool {
        match rlock(&self.families).get(name) {
            Some(fam) => {
                *lock(&fam.unit) = if unit.is_empty() {
                    None
                } else {
                    Some(unit.to_string())
                };
                true
            }
            None => false,
        }
    }

    /// Metadata for the family `name`, if registered.
    pub fn family(&self, name: &str) -> Option<FamilyInfo> {
        rlock(&self.families).get(name).map(family_info)
    }

    /// Metadata for every registered family, ordered by name.
    pub fn families(&self) -> Vec<FamilyInfo> {
        rlock(&self.families).values().map(family_info).collect()
    }

    // -- removal --

    /// Removes the family `name` with all its series; `true` when it existed.
    /// Instruments already handed out keep working but are no longer exported,
    /// and the name becomes free to register again (with any kind).
    pub fn unregister(&self, name: &str) -> bool {
        wlock(&self.families).remove(name).is_some()
    }

    /// Removes one series (`name` + `labels`); `true` when it existed.
    pub fn remove(&self, name: &str, labels: &[(&str, &str)]) -> bool {
        let fam = match rlock(&self.families).get(name) {
            Some(fam) => Arc::clone(fam),
            None => return false,
        };
        let mut key: LabelSet = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        key.sort();
        let removed = wlock(&fam.series).remove(&key).is_some();
        removed
    }

    /// Removes every family and every collector, and zeroes the diagnostic
    /// counters — the registry is as good as new. Handy between tests that
    /// share the [`global`] registry.
    pub fn clear(&self) {
        wlock(&self.families).clear();
        lock(&self.collectors).clear();
        self.process_collector.store(false, Ordering::Relaxed);
        self.stats.errors.store(0, Ordering::Relaxed);
        self.stats.overflows.store(0, Ordering::Relaxed);
        *lock(&self.stats.last_error) = None;
    }

    /// Zeroes every instrument but keeps all registrations, so handles cached
    /// elsewhere (for example by a router middleware) stay attached.
    pub fn reset(&self) {
        for fam in self.snapshot() {
            for slot in rlock(&fam.series).values() {
                slot.reset();
            }
            if let Some(slot) = fam.overflow.get() {
                slot.reset();
            }
        }
    }

    // -- collectors --

    /// Registers a callback that runs at the start of every
    /// [`encode`](Self::encode) / [`encode_openmetrics`](Self::encode_openmetrics)
    /// call, to refresh instruments whose values are sampled rather than
    /// pushed (queue depths, pool sizes, process statistics). The callback runs
    /// with no registry lock held and may register and update instruments; a
    /// nested `encode` from inside a collector skips the collectors.
    pub fn register_collector<F>(&self, collector: F)
    where
        F: Fn(&Registry) + Send + Sync + 'static,
    {
        lock(&self.collectors).push(Arc::new(collector));
    }

    /// Registers the built-in process collector (idempotent). On every encode
    /// it refreshes:
    ///
    /// - `process_start_time_seconds` and `process_uptime_seconds` — on every
    ///   platform. Off Linux the start time is the moment this module was first
    ///   used, which is close to process start when metrics are set up early.
    /// - On Linux only (read from `/proc/self`): `process_cpu_seconds_total`,
    ///   `process_resident_memory_bytes`, `process_virtual_memory_bytes`,
    ///   `process_threads`, `process_open_fds` and `process_max_fds`. Any value
    ///   that cannot be read is omitted.
    pub fn register_process_collector(&self) {
        if !self.process_collector.swap(true, Ordering::Relaxed) {
            process::touch_start();
            self.register_collector(process::collect);
        }
    }

    fn run_collectors(&self) {
        thread_local! {
            static COLLECTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        if COLLECTING.with(|c| c.replace(true)) {
            return;
        }
        // Reset the flag even if a collector panics.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                COLLECTING.with(|c| c.set(false));
            }
        }
        let _reset = Reset;
        // Snapshot first: collectors run without the list lock held.
        let collectors: Vec<Collector> = lock(&self.collectors).clone();
        for collector in collectors {
            collector(self);
        }
    }

    fn snapshot(&self) -> Vec<Arc<Family>> {
        rlock(&self.families).values().cloned().collect()
    }

    // -- encoding --

    /// Renders every metric in the Prometheus text exposition format (version
    /// 0.0.4; serve it with [`PROMETHEUS_CONTENT_TYPE`]).
    pub fn encode(&self) -> String {
        self.run_collectors();
        let mut out = String::new();
        for fam in self.snapshot() {
            encode_family(&mut out, &fam, Flavor::Prometheus);
        }
        out
    }

    /// Renders every metric in the OpenMetrics 1.0 text format (serve it with
    /// [`OPENMETRICS_CONTENT_TYPE`]). Differences from [`encode`](Self::encode):
    /// counter samples always carry the `_total` suffix while their family name
    /// drops it, `# UNIT` lines are emitted for families with a unit, `le`
    /// values are written as floats (`"1.0"`), and the document ends with
    /// `# EOF`.
    pub fn encode_openmetrics(&self) -> String {
        self.run_collectors();
        let mut out = String::new();
        for fam in self.snapshot() {
            encode_family(&mut out, &fam, Flavor::OpenMetrics);
        }
        out.push_str("# EOF\n");
        out
    }
}

fn family_info(fam: &Arc<Family>) -> FamilyInfo {
    FamilyInfo {
        name: fam.name.clone(),
        help: fam.help.clone(),
        unit: lock(&fam.unit).clone(),
        kind: fam.kind,
        series: rlock(&fam.series).len(),
        max_series: fam.max_series.load(Ordering::Relaxed),
    }
}

// ---- text encoders -----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Prometheus,
    OpenMetrics,
}

fn encode_family(out: &mut String, fam: &Family, flavor: Flavor) {
    let is_counter = matches!(fam.kind, MetricKind::Counter | MetricKind::FloatCounter);
    // OpenMetrics names a counter family without `_total` and its sample with it.
    let (family_name, sample_name): (&str, String) = match (flavor, is_counter) {
        (Flavor::OpenMetrics, true) => {
            let base = fam.name.strip_suffix("_total").unwrap_or(&fam.name);
            (base, format!("{base}_total"))
        }
        _ => (fam.name.as_str(), fam.name.clone()),
    };

    out.push_str("# HELP ");
    out.push_str(family_name);
    out.push(' ');
    escape_into(out, &fam.help, flavor == Flavor::OpenMetrics);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(family_name);
    out.push(' ');
    out.push_str(fam.kind.type_str());
    out.push('\n');
    if flavor == Flavor::OpenMetrics {
        if let Some(unit) = lock(&fam.unit).as_deref() {
            // The format requires the unit to be the family name's suffix.
            if family_name
                .strip_suffix(unit)
                .is_some_and(|rest| rest.ends_with('_'))
            {
                out.push_str("# UNIT ");
                out.push_str(family_name);
                out.push(' ');
                out.push_str(unit);
                out.push('\n');
            }
        }
    }

    let series = rlock(&fam.series);
    for (labels, slot) in series.iter() {
        encode_series(out, &sample_name, labels, slot, flavor);
    }
    drop(series);
    if let Some(slot) = fam.overflow.get() {
        let labels = vec![(OVERFLOW_LABEL.to_string(), "true".to_string())];
        encode_series(out, &sample_name, &labels, slot, flavor);
    }
}

fn encode_series(out: &mut String, name: &str, labels: &LabelSet, slot: &Slot, flavor: Flavor) {
    match slot {
        Slot::Counter(c) => sample(out, name, "", labels, None, &c.get().to_string()),
        Slot::FloatCounter(c) => sample(out, name, "", labels, None, &format_f64(c.get())),
        Slot::Gauge(g) => sample(out, name, "", labels, None, &g.get().to_string()),
        Slot::FloatGauge(g) => sample(out, name, "", labels, None, &format_f64(g.get())),
        Slot::Histogram(h) => {
            // One snapshot feeds both the buckets and `_count`, so the `+Inf`
            // bucket and `_count` always agree.
            let buckets = h.buckets();
            let sum = h.sum();
            let mut total = 0;
            for (bound, cumulative) in &buckets {
                let le = match flavor {
                    Flavor::Prometheus => format_f64(*bound),
                    Flavor::OpenMetrics => format_le_openmetrics(*bound),
                };
                sample(
                    out,
                    name,
                    "_bucket",
                    labels,
                    Some(&le),
                    &cumulative.to_string(),
                );
                total = *cumulative;
            }
            sample(out, name, "_sum", labels, None, &format_f64(sum));
            sample(out, name, "_count", labels, None, &total.to_string());
        }
    }
}

/// Writes one `name{labels} value` line; `le` appends the histogram bucket label.
fn sample(
    out: &mut String,
    name: &str,
    suffix: &str,
    labels: &LabelSet,
    le: Option<&str>,
    value: &str,
) {
    out.push_str(name);
    out.push_str(suffix);
    if !labels.is_empty() || le.is_some() {
        out.push('{');
        let mut first = true;
        for (k, v) in labels {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(k);
            out.push_str("=\"");
            escape_into(out, v, true);
            out.push('"');
        }
        if let Some(le) = le {
            if !first {
                out.push(',');
            }
            out.push_str("le=\"");
            out.push_str(le);
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

/// Escapes `\` and newline — plus `"` when `quotes` is set (label values, and
/// OpenMetrics help text).
fn escape_into(out: &mut String, text: &str, quotes: bool) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '"' if quotes => out.push_str("\\\""),
            other => out.push(other),
        }
    }
}

/// Formats a float the way Prometheus expects: integers without a decimal
/// point, `+Inf`/`-Inf`/`NaN` spelled out, very large or small magnitudes in
/// exponent form, otherwise a plain decimal.
fn format_f64(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        }
    } else if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else if v.abs() >= 1e15 || v.abs() < 1e-6 {
        format!("{v:e}")
    } else {
        format!("{v}")
    }
}

/// OpenMetrics wants `le` as a float literal: `1.0`, not `1`.
fn format_le_openmetrics(v: f64) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        format_f64(v)
    }
}

static GLOBAL: OnceLock<Registry> = OnceLock::new();

/// The process-global [`Registry`], created on first use. Shared by the `turbo`
/// router's RED middleware and any `/metrics` endpoint.
pub fn global() -> &'static Registry {
    GLOBAL.get_or_init(Registry::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_and_gauge_encode() {
        let reg = Registry::new();
        let c = reg.counter("reqs_total", "Total requests", &[("method", "GET")]);
        c.inc();
        c.inc_by(4);
        let g = reg.gauge("inflight", "In flight", &[]);
        g.inc();
        g.inc();
        g.dec();
        let out = reg.encode();
        assert!(out.contains("# TYPE reqs_total counter"));
        assert!(out.contains("reqs_total{method=\"GET\"} 5"));
        assert!(out.contains("# TYPE inflight gauge"));
        assert!(out.contains("inflight 1"));
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let reg = Registry::new();
        let h = reg.histogram("latency", "Latency", &[], &[0.1, 0.5, 1.0]);
        h.observe(0.05); // <= 0.1
        h.observe(0.2); // <= 0.5
        h.observe(2.0); // +Inf
        let out = reg.encode();
        assert!(out.contains("latency_bucket{le=\"0.1\"} 1"), "out={out}");
        assert!(out.contains("latency_bucket{le=\"0.5\"} 2"), "out={out}");
        assert!(out.contains("latency_bucket{le=\"1\"} 2"), "out={out}");
        assert!(out.contains("latency_bucket{le=\"+Inf\"} 3"), "out={out}");
        assert!(out.contains("latency_count 3"), "out={out}");
        assert_eq!(h.count(), 3);
        assert!((h.sum() - 2.25).abs() < 1e-9);
    }

    #[test]
    fn same_name_labels_return_same_series() {
        let reg = Registry::new();
        reg.counter("hits", "h", &[("a", "1")]).inc();
        reg.counter("hits", "h", &[("a", "1")]).inc();
        assert!(reg.encode().contains("hits{a=\"1\"} 2"));
    }

    #[test]
    fn name_grammar() {
        for good in ["a", "_a", "a:b", ":a", "http_requests_total", "A9"] {
            assert!(is_valid_metric_name(good), "{good}");
        }
        for bad in ["", "9a", "a-b", "a b", "a.b", "é"] {
            assert!(!is_valid_metric_name(bad), "{bad}");
        }
        for good in ["a", "_a", "le", "A9_b"] {
            assert!(is_valid_label_name(good), "{good}");
        }
        for bad in ["", "9a", "a:b", "a-b", "__name__", "a b"] {
            assert!(!is_valid_label_name(bad), "{bad}");
        }
    }

    #[test]
    fn bounds_are_normalised() {
        let bounds = normalise_bounds(&[
            1.0,
            f64::NAN,
            0.5,
            f64::INFINITY,
            1.0,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
        ]);
        assert_eq!(bounds.len(), 3);
        assert_eq!(bounds[1], 0.5);
        assert_eq!(bounds[2], 1.0);
    }

    #[test]
    fn float_formatting() {
        assert_eq!(format_f64(1.0), "1");
        assert_eq!(format_f64(0.25), "0.25");
        assert_eq!(format_f64(f64::INFINITY), "+Inf");
        assert_eq!(format_f64(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_f64(f64::NAN), "NaN");
        assert_eq!(format_f64(1e300), "1e300");
        assert_eq!(format_f64(2.5e-9), "2.5e-9");
        assert_eq!(format_le_openmetrics(1.0), "1.0");
        assert_eq!(format_le_openmetrics(0.5), "0.5");
        assert_eq!(format_le_openmetrics(f64::INFINITY), "+Inf");
    }
}
