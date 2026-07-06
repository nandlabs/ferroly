//! A tiny, dependency-free metrics registry with Prometheus text exposition.
//!
//! Three instrument types — [`Counter`], [`Gauge`], and [`Histogram`] — are
//! registered in a [`Registry`] under a metric name and a set of labels. Each
//! distinct label set is its own time series. Instruments are handed back as
//! `Arc<…>` so the hot path only does relaxed atomic operations (no lock).
//!
//! [`Registry::encode`] renders the standard Prometheus/OpenMetrics text format,
//! ready to serve from a `/metrics` endpoint. A process-global [`global`]
//! registry is provided for the common single-registry case; the `turbo` router
//! records RED metrics into it via
//! [`Router::metrics`](../turbo/struct.Router.html) when the `metrics` feature
//! is on.
//!
//! ```
//! use ferroly::metrics::Registry;
//!
//! let reg = Registry::new();
//! let hits = reg.counter("hits_total", "Total hits", &[("route", "/")]);
//! hits.inc();
//! assert!(reg.encode().contains("hits_total{route=\"/\"} 1"));
//! ```

#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// A monotonically increasing counter.
#[derive(Default)]
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

/// A value that can go up and down (e.g. in-flight requests).
#[derive(Default)]
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
    /// The current value.
    pub fn get(&self) -> i64 {
        self.v.load(Ordering::Relaxed)
    }
}

/// Default histogram bucket upper bounds (seconds), matching Prometheus'
/// client-library defaults — good for request-latency observations.
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// A cumulative histogram over fixed bucket bounds, plus a running sum and count.
pub struct Histogram {
    /// Ascending upper bounds. An observation falls in the first bound it is
    /// `<=`; anything larger lands in the implicit `+Inf` bucket.
    bounds: Vec<f64>,
    /// Per-bucket (non-cumulative) counts; `counts[bounds.len()]` is `+Inf`.
    counts: Vec<AtomicU64>,
    sum_bits: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new(mut bounds: Vec<f64>) -> Self {
        bounds.retain(|b| !b.is_nan());
        bounds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = bounds.len() + 1; // +1 for the +Inf overflow bucket
        Self {
            bounds,
            counts: (0..n).map(|_| AtomicU64::new(0)).collect(),
            sum_bits: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Records one observation.
    pub fn observe(&self, v: f64) {
        let idx = self
            .bounds
            .iter()
            .position(|&b| v <= b)
            .unwrap_or(self.bounds.len());
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // Add `v` to the f64 sum via a compare-and-swap loop on its bit pattern.
        let mut cur = self.sum_bits.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + v).to_bits();
            match self.sum_bits.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// The number of observations.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The sum of all observed values.
    pub fn sum(&self) -> f64 {
        f64::from_bits(self.sum_bits.load(Ordering::Relaxed))
    }
}

enum Instrument {
    Counter(Arc<Counter>),
    Gauge(Arc<Gauge>),
    Histogram(Arc<Histogram>),
}

impl Instrument {
    fn type_str(&self) -> &'static str {
        match self {
            Instrument::Counter(_) => "counter",
            Instrument::Gauge(_) => "gauge",
            Instrument::Histogram(_) => "histogram",
        }
    }
}

struct Family {
    help: String,
    series: BTreeMap<Vec<(String, String)>, Instrument>,
}

/// A collection of metrics, keyed by name and label set, that renders to the
/// Prometheus text exposition format.
#[derive(Default)]
pub struct Registry {
    families: Mutex<BTreeMap<String, Family>>,
}

fn sorted_labels(labels: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = labels
        .iter()
        .map(|(k, val)| (k.to_string(), val.to_string()))
        .collect();
    v.sort();
    v
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the [`Counter`] for `name` + `labels`, creating it (with `help`)
    /// on first use. Repeated calls with the same name/labels return the same
    /// instrument.
    pub fn counter(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Arc<Counter> {
        let mut fams = self.families.lock().unwrap();
        let fam = fams.entry(name.to_string()).or_insert_with(|| Family {
            help: help.to_string(),
            series: BTreeMap::new(),
        });
        match fam
            .series
            .entry(sorted_labels(labels))
            .or_insert_with(|| Instrument::Counter(Arc::new(Counter::default())))
        {
            Instrument::Counter(c) => c.clone(),
            // Name reused with a different kind: hand back a detached handle
            // rather than panicking in a request path.
            _ => Arc::new(Counter::default()),
        }
    }

    /// Returns the [`Gauge`] for `name` + `labels`, creating it on first use.
    pub fn gauge(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Arc<Gauge> {
        let mut fams = self.families.lock().unwrap();
        let fam = fams.entry(name.to_string()).or_insert_with(|| Family {
            help: help.to_string(),
            series: BTreeMap::new(),
        });
        match fam
            .series
            .entry(sorted_labels(labels))
            .or_insert_with(|| Instrument::Gauge(Arc::new(Gauge::default())))
        {
            Instrument::Gauge(g) => g.clone(),
            _ => Arc::new(Gauge::default()),
        }
    }

    /// Returns the [`Histogram`] for `name` + `labels`, creating it with the
    /// given bucket `bounds` on first use (see [`DEFAULT_BUCKETS`]).
    pub fn histogram(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        bounds: &[f64],
    ) -> Arc<Histogram> {
        let mut fams = self.families.lock().unwrap();
        let fam = fams.entry(name.to_string()).or_insert_with(|| Family {
            help: help.to_string(),
            series: BTreeMap::new(),
        });
        match fam
            .series
            .entry(sorted_labels(labels))
            .or_insert_with(|| Instrument::Histogram(Arc::new(Histogram::new(bounds.to_vec()))))
        {
            Instrument::Histogram(h) => h.clone(),
            _ => Arc::new(Histogram::new(bounds.to_vec())),
        }
    }

    /// Renders every metric in the Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let fams = self.families.lock().unwrap();
        let mut out = String::new();
        for (name, fam) in fams.iter() {
            let kind = fam
                .series
                .values()
                .next()
                .map(Instrument::type_str)
                .unwrap_or("untyped");
            out.push_str(&format!("# HELP {name} {}\n", escape_help(&fam.help)));
            out.push_str(&format!("# TYPE {name} {kind}\n"));
            for (labels, inst) in fam.series.iter() {
                match inst {
                    Instrument::Counter(c) => {
                        out.push_str(&sample_line(name, labels, &[], c.get() as f64));
                    }
                    Instrument::Gauge(g) => {
                        out.push_str(&sample_line(name, labels, &[], g.get() as f64));
                    }
                    Instrument::Histogram(h) => {
                        let mut cumulative = 0u64;
                        for (i, &bound) in h.bounds.iter().enumerate() {
                            cumulative += h.counts[i].load(Ordering::Relaxed);
                            out.push_str(&sample_line(
                                &format!("{name}_bucket"),
                                labels,
                                &[("le", &format_f64(bound))],
                                cumulative as f64,
                            ));
                        }
                        cumulative += h.counts[h.bounds.len()].load(Ordering::Relaxed);
                        out.push_str(&sample_line(
                            &format!("{name}_bucket"),
                            labels,
                            &[("le", "+Inf")],
                            cumulative as f64,
                        ));
                        out.push_str(&sample_line(&format!("{name}_sum"), labels, &[], h.sum()));
                        out.push_str(&sample_line(
                            &format!("{name}_count"),
                            labels,
                            &[],
                            h.count() as f64,
                        ));
                    }
                }
            }
        }
        out
    }
}

/// Renders one `name{labels} value` sample line, merging `base` labels with any
/// `extra` labels (e.g. a histogram's `le`).
fn sample_line(
    name: &str,
    base: &[(String, String)],
    extra: &[(&str, &str)],
    value: f64,
) -> String {
    let mut parts: Vec<String> = base
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
        .collect();
    parts.extend(
        extra
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v))),
    );
    if parts.is_empty() {
        format!("{name} {}\n", format_f64(value))
    } else {
        format!("{name}{{{}}} {}\n", parts.join(","), format_f64(value))
    }
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Formats a float the way Prometheus expects: integers without a decimal point,
/// `+Inf` where relevant, otherwise a plain decimal.
fn format_f64(v: f64) -> String {
    if v.is_infinite() {
        if v > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        }
    } else if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
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
}
