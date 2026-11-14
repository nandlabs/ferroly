//! Regression tests for `ferroly::metrics`: every test parses the exposition
//! output with a strict reader, so an invalid document fails loudly.

#![cfg(feature = "metrics")]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use ferroly::metrics::{
    is_valid_label_name, is_valid_metric_name, Histogram, MetricKind, MetricsError, Registry,
    DEFAULT_BUCKETS, DEFAULT_MAX_SERIES, OVERFLOW_LABEL,
};

// ---- a strict exposition reader ---------------------------------------------

type Labels = Vec<(String, String)>;

#[derive(Debug, Clone, PartialEq)]
struct Sample {
    name: String,
    labels: Vec<(String, String)>,
    value: f64,
}

#[derive(Debug, Default)]
struct Doc {
    types: BTreeMap<String, String>,
    helps: BTreeMap<String, String>,
    units: BTreeMap<String, String>,
    samples: Vec<Sample>,
    eof: bool,
}

impl Doc {
    fn value(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let mut want: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        want.sort();
        self.samples
            .iter()
            .find(|s| {
                let mut have = s.labels.clone();
                have.sort();
                s.name == name && have == want
            })
            .map(|s| s.value)
    }

    fn named(&self, name: &str) -> Vec<&Sample> {
        self.samples.iter().filter(|s| s.name == name).collect()
    }
}

fn parse_value(text: &str) -> f64 {
    match text {
        "+Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        "NaN" => f64::NAN,
        other => other
            .parse()
            .unwrap_or_else(|_| panic!("bad sample value {other:?}")),
    }
}

/// Reverses the format's escaping, rejecting any escape it does not define.
fn unescape(text: &str, quotes: bool) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('"') if quotes => out.push('"'),
            other => panic!("invalid escape \\{other:?} in {text:?}"),
        }
    }
    out
}

/// Parses `{k="v",…}` starting after the `{`; returns the labels and the rest.
fn parse_labels(mut rest: &str, line: &str) -> (Vec<(String, String)>, String) {
    let mut labels = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix('}') {
            return (labels, after.to_string());
        }
        let eq = rest
            .find('=')
            .unwrap_or_else(|| panic!("no `=` in {line:?}"));
        let key = &rest[..eq];
        // `le` is the one name the label grammar reserves but histograms emit.
        assert!(
            is_valid_label_name(key),
            "invalid label name {key:?} in {line:?}"
        );
        rest = rest[eq + 1..]
            .strip_prefix('"')
            .unwrap_or_else(|| panic!("label value not quoted in {line:?}"));
        // Find the closing quote, skipping escaped characters.
        let mut end = None;
        let mut escaped = false;
        for (i, c) in rest.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(i);
                break;
            }
        }
        let end = end.unwrap_or_else(|| panic!("unterminated label value in {line:?}"));
        labels.push((key.to_string(), unescape(&rest[..end], true)));
        rest = &rest[end + 1..];
        if let Some(after) = rest.strip_prefix(',') {
            rest = after;
        } else {
            assert!(rest.starts_with('}'), "expected `,` or `}}` in {line:?}");
        }
    }
}

/// Parses an exposition document and checks its structure: one `# TYPE` per
/// family, every sample under its own family's header, valid names, no
/// repeated label names and no repeated series.
fn parse(text: &str, openmetrics: bool) -> Doc {
    assert!(
        text.is_empty() || text.ends_with('\n'),
        "no trailing newline"
    );
    let mut doc = Doc::default();
    let mut current: Option<String> = None;
    let mut seen: BTreeSet<(String, Vec<(String, String)>)> = BTreeSet::new();

    for line in text.lines() {
        assert!(!doc.eof, "content after # EOF: {line:?}");
        assert!(!line.is_empty(), "blank line in exposition output");
        if line == "# EOF" {
            assert!(openmetrics, "# EOF in Prometheus output");
            doc.eof = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            let mut parts = rest.splitn(3, ' ');
            let keyword = parts.next().unwrap();
            let name = parts
                .next()
                .unwrap_or_else(|| panic!("no name in {line:?}"));
            let text = parts.next().unwrap_or("");
            assert!(is_valid_metric_name(name), "invalid family name {name:?}");
            match keyword {
                "HELP" => {
                    let help = unescape(text, openmetrics);
                    assert!(
                        doc.helps.insert(name.to_string(), help).is_none(),
                        "second # HELP for {name}"
                    );
                }
                "TYPE" => {
                    assert!(
                        matches!(text, "counter" | "gauge" | "histogram"),
                        "unknown type in {line:?}"
                    );
                    assert!(
                        doc.types
                            .insert(name.to_string(), text.to_string())
                            .is_none(),
                        "second # TYPE for {name}"
                    );
                }
                "UNIT" => {
                    assert!(openmetrics, "# UNIT in Prometheus output");
                    assert!(
                        name.ends_with(&format!("_{text}")),
                        "unit {text:?} is not a suffix of {name:?}"
                    );
                    doc.units.insert(name.to_string(), text.to_string());
                }
                other => panic!("unknown comment keyword {other:?}"),
            }
            current = Some(name.to_string());
            continue;
        }

        // A sample line: name[{labels}] value
        let name_end = line
            .find(['{', ' '])
            .unwrap_or_else(|| panic!("no value in {line:?}"));
        let name = &line[..name_end];
        assert!(is_valid_metric_name(name), "invalid sample name {name:?}");
        let (labels, rest) = if line[name_end..].starts_with('{') {
            parse_labels(&line[name_end + 1..], line)
        } else {
            (Vec::new(), line[name_end..].to_string())
        };
        let value = parse_value(
            rest.strip_prefix(' ')
                .unwrap_or_else(|| panic!("no space before value in {line:?}")),
        );

        let names: BTreeSet<&String> = labels.iter().map(|(k, _)| k).collect();
        assert_eq!(names.len(), labels.len(), "repeated label in {line:?}");

        let family = current
            .clone()
            .unwrap_or_else(|| panic!("sample before any # TYPE: {line:?}"));
        let kind = doc
            .types
            .get(&family)
            .unwrap_or_else(|| panic!("no # TYPE for family {family}"));
        let allowed: Vec<String> = match (kind.as_str(), openmetrics) {
            ("histogram", _) => vec![
                format!("{family}_bucket"),
                format!("{family}_sum"),
                format!("{family}_count"),
            ],
            ("counter", true) => vec![format!("{family}_total")],
            _ => vec![family.clone()],
        };
        assert!(
            allowed.iter().any(|a| a == name),
            "sample {name} is not part of {kind} family {family}"
        );
        if kind == "histogram" {
            let has_le = labels.iter().any(|(k, _)| k == "le");
            assert_eq!(
                has_le,
                name == format!("{family}_bucket"),
                "`le` belongs on histogram buckets only: {line:?}"
            );
        }

        let mut key = labels.clone();
        key.sort();
        assert!(
            seen.insert((name.to_string(), key)),
            "duplicate series {line:?}"
        );
        doc.samples.push(Sample {
            name: name.to_string(),
            labels,
            value,
        });
    }
    if openmetrics {
        assert!(doc.eof, "OpenMetrics output must end with # EOF");
    }
    check_histograms(&doc);
    doc
}

/// Every histogram series: ascending `le`, non-decreasing cumulative counts,
/// a final `+Inf` bucket equal to `_count`, and a `_sum`.
fn check_histograms(doc: &Doc) {
    for (family, kind) in &doc.types {
        if kind != "histogram" {
            continue;
        }
        let mut groups: BTreeMap<Labels, Vec<(f64, f64)>> = BTreeMap::new();
        for s in doc.named(&format!("{family}_bucket")) {
            let le = s
                .labels
                .iter()
                .find(|(k, _)| k == "le")
                .map(|(_, v)| parse_value(v))
                .expect("bucket has le");
            let rest: Vec<_> = s
                .labels
                .iter()
                .filter(|(k, _)| k != "le")
                .cloned()
                .collect();
            groups.entry(rest).or_default().push((le, s.value));
        }
        for (labels, buckets) in groups {
            let refs: Vec<(&str, &str)> = labels
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            for pair in buckets.windows(2) {
                assert!(pair[0].0 < pair[1].0, "{family}: le not strictly ascending");
                assert!(pair[0].1 <= pair[1].1, "{family}: buckets not cumulative");
            }
            let last = buckets.last().expect("at least the +Inf bucket");
            assert_eq!(last.0, f64::INFINITY, "{family}: last bucket must be +Inf");
            let count = doc
                .value(&format!("{family}_count"), &refs)
                .unwrap_or_else(|| panic!("{family}: missing _count"));
            assert_eq!(last.1, count, "{family}: +Inf bucket != _count");
            let sum = doc
                .value(&format!("{family}_sum"), &refs)
                .unwrap_or_else(|| panic!("{family}: missing _sum"));
            assert!(!sum.is_nan(), "{family}: _sum is NaN");
        }
    }
}

fn prom(reg: &Registry) -> Doc {
    parse(&reg.encode(), false)
}

// ---- I12: the kind belongs to the family ------------------------------------

#[test]
fn kind_mismatch_is_an_error_and_never_reaches_the_output() {
    let reg = Registry::new();
    reg.counter("jobs", "Jobs", &[("q", "a")]).inc();

    // Fallible API: a typed error.
    let err = reg.try_gauge("jobs", "Jobs", &[("q", "b")]).unwrap_err();
    assert_eq!(
        err,
        MetricsError::KindMismatch {
            name: "jobs".into(),
            registered: MetricKind::Counter,
            requested: MetricKind::Gauge,
        }
    );
    assert!(reg
        .try_histogram("jobs", "Jobs", &[], DEFAULT_BUCKETS)
        .is_err());
    assert!(reg.try_float_counter("jobs", "Jobs", &[]).is_err());
    assert_eq!(reg.registration_errors(), 0, "try_* does not count");

    // Infallible API: a detached instrument plus a counted error.
    let detached = reg.gauge("jobs", "Jobs", &[("q", "b")]);
    detached.set(99);
    assert_eq!(detached.get(), 99, "a detached instrument still works");
    let h = reg.histogram("jobs", "Jobs", &[("q", "c")], &[1.0]);
    h.observe(0.5);
    assert_eq!(reg.registration_errors(), 2);
    assert!(matches!(
        reg.last_registration_error(),
        Some(MetricsError::KindMismatch { .. })
    ));

    let text = reg.encode();
    assert_eq!(text.matches("# TYPE jobs ").count(), 1, "{text}");
    let doc = parse(&text, false);
    assert_eq!(doc.types["jobs"], "counter");
    assert_eq!(doc.samples.len(), 1, "only the counter series: {text}");
    assert_eq!(doc.value("jobs", &[("q", "a")]), Some(1.0));
}

#[test]
fn metric_and_label_names_are_validated() {
    let reg = Registry::new();
    for bad in [
        "",
        "9lives",
        "has-dash",
        "has space",
        "dot.ted",
        "new\nline",
    ] {
        assert_eq!(
            reg.try_counter(bad, "h", &[]).unwrap_err(),
            MetricsError::InvalidMetricName(bad.to_string()),
        );
        reg.counter(bad, "h", &[]).inc();
        reg.counter_vec(bad, "h", &["a"]).with_labels(&["x"]).inc();
    }
    for bad in ["", "9a", "has-dash", "a:b", "__reserved", "sp ace"] {
        assert!(
            matches!(
                reg.try_gauge("ok_metric", "h", &[(bad, "v")]),
                Err(MetricsError::InvalidLabelName { .. })
            ),
            "{bad:?}"
        );
        reg.gauge("ok_metric", "h", &[(bad, "v")]).set(1);
        assert!(reg.try_gauge_vec("ok_metric", "h", &[bad]).is_err());
    }
    assert!(reg.registration_errors() >= 18);
    assert!(
        reg.families().is_empty(),
        "a rejected registration leaves no family"
    );
    assert_eq!(reg.encode(), "");

    assert!(is_valid_metric_name("ns:sub_total"));
    assert!(!is_valid_label_name("ns:sub"));
}

#[test]
fn le_and_quantile_are_reserved_on_histograms() {
    let reg = Registry::new();
    for reserved in ["le", "quantile"] {
        assert_eq!(
            reg.try_histogram("lat", "h", &[(reserved, "1")], &[1.0])
                .unwrap_err(),
            MetricsError::ReservedLabel {
                metric: "lat".into(),
                label: reserved.into(),
            }
        );
        assert!(reg
            .try_histogram_vec("lat", "h", &[reserved], &[1.0])
            .is_err());
        // Infallible: detached, so the bucket lines can never carry two `le`s.
        reg.histogram("lat", "h", &[(reserved, "1")], &[1.0])
            .observe(0.5);
    }
    reg.histogram("lat", "h", &[("route", "/")], &[1.0])
        .observe(0.5);
    // Not reserved on other kinds.
    reg.try_counter("plain", "h", &[("le", "x")]).unwrap().inc();

    let text = reg.encode();
    let doc = parse(&text, false);
    for line in text.lines().filter(|l| l.starts_with("lat_bucket")) {
        assert_eq!(line.matches("le=").count(), 1, "{line}");
    }
    assert_eq!(
        doc.value("lat_bucket", &[("route", "/"), ("le", "1")]),
        Some(1.0)
    );
    assert_eq!(reg.registration_errors(), 2);
}

#[test]
fn a_label_given_twice_is_rejected() {
    let reg = Registry::new();
    assert_eq!(
        reg.try_counter("c", "h", &[("a", "1"), ("a", "2")])
            .unwrap_err(),
        MetricsError::DuplicateLabel {
            metric: "c".into(),
            label: "a".into(),
        }
    );
    assert!(reg.try_counter_vec("c", "h", &["a", "b", "a"]).is_err());
    reg.counter("c", "h", &[("a", "1"), ("a", "2")]).inc();
    assert_eq!(reg.encode(), "");
}

#[test]
fn histogram_sample_names_cannot_be_registered_twice() {
    let reg = Registry::new();
    reg.histogram("rpc", "h", &[], &[1.0]).observe(0.1);
    for clash in ["rpc_count", "rpc_sum", "rpc_bucket"] {
        assert_eq!(
            reg.try_counter(clash, "h", &[]).unwrap_err(),
            MetricsError::NameCollision {
                name: clash.into(),
                existing: "rpc".into(),
            }
        );
    }
    // The other direction: the plain metric exists first.
    reg.counter("db_count", "h", &[]).inc();
    assert_eq!(
        reg.try_histogram("db", "h", &[], &[1.0]).unwrap_err(),
        MetricsError::NameCollision {
            name: "db".into(),
            existing: "db_count".into(),
        }
    );
    reg.counter("rpc_count", "h", &[]).inc();
    reg.histogram("db", "h", &[], &[1.0]).observe(1.0);
    let doc = prom(&reg);
    assert_eq!(doc.named("rpc_count").len(), 1);
    assert_eq!(doc.named("db_count").len(), 1);
}

#[test]
fn bucket_bounds_are_sorted_deduplicated_and_finite() {
    let reg = Registry::new();
    let h = reg.histogram(
        "sizes",
        "h",
        &[],
        &[
            10.0,
            1.0,
            f64::NAN,
            10.0,
            f64::INFINITY,
            5.0,
            f64::NEG_INFINITY,
            1.0,
        ],
    );
    assert_eq!(h.bounds(), &[1.0, 5.0, 10.0]);
    h.observe(1.0); // boundary values are inclusive
    h.observe(7.0);
    h.observe(1000.0);

    let doc = prom(&reg);
    let buckets = doc.named("sizes_bucket");
    let les: Vec<&str> = buckets.iter().map(|s| s.labels[0].1.as_str()).collect();
    assert_eq!(les, ["1", "5", "10", "+Inf"]);
    let counts: Vec<f64> = buckets.iter().map(|s| s.value).collect();
    assert_eq!(counts, [1.0, 1.0, 2.0, 3.0]);
    assert_eq!(doc.value("sizes_count", &[]), Some(3.0));
    assert_eq!(doc.value("sizes_sum", &[]), Some(1008.0));
    assert_eq!(h.buckets().last(), Some(&(f64::INFINITY, 3)));
}

#[test]
fn non_finite_observations_are_ignored() {
    let reg = Registry::new();
    let h = reg.histogram("lat", "h", &[], &[0.5, 1.0]);
    h.observe(0.25);
    h.observe(f64::NAN);
    h.observe(f64::INFINITY);
    h.observe(f64::NEG_INFINITY);
    h.observe(0.75);
    assert_eq!(h.count(), 2);
    assert_eq!(h.sum(), 1.0);

    let doc = prom(&reg);
    assert_eq!(doc.value("lat_sum", &[]), Some(1.0));
    assert_eq!(doc.value("lat_count", &[]), Some(2.0));
    assert_eq!(doc.value("lat_bucket", &[("le", "+Inf")]), Some(2.0));
    assert!(!reg.encode().contains("NaN"));

    // The float instruments guard their sums the same way.
    let c = reg.float_counter("work_seconds_total", "h", &[]);
    c.inc_by(1.5);
    c.inc_by(f64::NAN);
    c.inc_by(-3.0);
    c.inc_by(f64::INFINITY);
    assert_eq!(c.get(), 1.5);
    let g = reg.float_gauge("ratio", "h", &[]);
    g.set(0.5);
    g.add(f64::NAN);
    assert_eq!(g.get(), 0.5);
}

#[test]
fn label_values_and_help_text_are_escaped() {
    let reg = Registry::new();
    let nasty = "back\\slash \"quoted\"\nsecond line} 1\nfake_metric 7";
    let help = "path C:\\tmp\nand a \"quote\"";
    reg.counter("esc_total", help, &[("v", nasty), ("plain", "ok")])
        .inc();

    let text = reg.encode();
    assert_eq!(text.lines().count(), 3, "one HELP, one TYPE, one sample");
    assert!(
        text.contains("# HELP esc_total path C:\\\\tmp\\nand a \"quote\"\n"),
        "{text}"
    );
    assert!(
        text.contains("v=\"back\\\\slash \\\"quoted\\\"\\nsecond line} 1\\nfake_metric 7\""),
        "{text}"
    );
    let doc = parse(&text, false);
    assert_eq!(doc.helps["esc_total"], help);
    assert_eq!(
        doc.value("esc_total", &[("v", nasty), ("plain", "ok")]),
        Some(1.0)
    );
    assert!(doc.named("fake_metric").is_empty());

    // OpenMetrics also escapes `"` in help text.
    let om = reg.encode_openmetrics();
    assert!(
        om.contains("# HELP esc path C:\\\\tmp\\nand a \\\"quote\\\"\n"),
        "{om}"
    );
    assert_eq!(parse(&om, true).helps["esc"], help);
}

#[test]
fn histogram_inf_sum_and_count_are_consistent_per_series() {
    let reg = Registry::new();
    let v = reg.histogram_vec("req_seconds", "h", &["method"], &[0.1, 1.0]);
    for secs in [0.05, 0.5, 0.5, 5.0] {
        v.with_labels(&["GET"]).observe(secs);
    }
    v.with_labels(&["POST"]).observe(0.2);

    let doc = prom(&reg);
    let get = [("method", "GET")];
    assert_eq!(
        doc.value("req_seconds_bucket", &[("method", "GET"), ("le", "0.1")]),
        Some(1.0)
    );
    assert_eq!(
        doc.value("req_seconds_bucket", &[("method", "GET"), ("le", "1")]),
        Some(3.0)
    );
    assert_eq!(
        doc.value("req_seconds_bucket", &[("method", "GET"), ("le", "+Inf")]),
        Some(4.0)
    );
    assert_eq!(doc.value("req_seconds_count", &get), Some(4.0));
    assert_eq!(doc.value("req_seconds_sum", &get), Some(6.05));
    assert_eq!(
        doc.value("req_seconds_count", &[("method", "POST")]),
        Some(1.0)
    );
    // An empty histogram still emits a complete, valid series.
    reg.histogram("idle", "h", &[], &[]);
    let doc = prom(&reg);
    assert_eq!(doc.value("idle_bucket", &[("le", "+Inf")]), Some(0.0));
    assert_eq!(doc.value("idle_sum", &[]), Some(0.0));
    assert_eq!(doc.value("idle_count", &[]), Some(0.0));
}

#[test]
fn histogram_snapshots_stay_consistent_under_concurrent_updates() {
    let reg = Arc::new(Registry::new());
    let h = reg.histogram("busy", "h", &[], &[1.0, 2.0, 3.0]);
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|i| {
            let (h, stop) = (h.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    h.observe(((n + i) % 5) as f64);
                    n += 1;
                }
            })
        })
        .collect();
    for _ in 0..200 {
        // `parse` asserts that +Inf equals _count in every snapshot.
        prom(&reg);
    }
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }
    let doc = prom(&reg);
    assert_eq!(doc.value("busy_count", &[]), Some(h.count() as f64));
}

// ---- cardinality guard --------------------------------------------------------

#[test]
fn cardinality_guard_folds_excess_label_sets_into_one_overflow_series() {
    let reg = Registry::with_max_series(3);
    for user in 0..10 {
        reg.counter("logins_total", "h", &[("user", &user.to_string())])
            .inc();
    }
    // Existing series stay reachable after the cap is hit.
    reg.counter("logins_total", "h", &[("user", "0")]).inc();
    assert_eq!(reg.series_overflows(), 7);
    assert_eq!(reg.registration_errors(), 0, "overflow is not an error");

    let doc = prom(&reg);
    let series = doc.named("logins_total");
    assert_eq!(series.len(), 4, "3 real series + 1 overflow series");
    assert_eq!(
        doc.value("logins_total", &[(OVERFLOW_LABEL, "true")]),
        Some(7.0)
    );
    let total: f64 = series.iter().map(|s| s.value).sum();
    assert_eq!(total, 11.0, "no increment is lost");
    assert_eq!(reg.family("logins_total").unwrap().series, 3);
    assert_eq!(reg.family("logins_total").unwrap().max_series, 3);

    // Histograms and vec handles are guarded too; the cap is per family.
    let lat = reg.histogram_vec("lat", "h", &["path"], &[1.0]);
    for i in 0..5 {
        lat.with_labels(&[&format!("/p/{i}")]).observe(0.5);
    }
    let doc = prom(&reg);
    assert_eq!(doc.named("lat_count").len(), 4);
    assert_eq!(
        doc.value("lat_count", &[(OVERFLOW_LABEL, "true")]),
        Some(2.0)
    );

    // Raising one family's cap lets new label sets in again.
    assert!(reg.set_max_series("logins_total", 100));
    assert!(!reg.set_max_series("missing", 100));
    reg.counter("logins_total", "h", &[("user", "50")]).inc();
    assert_eq!(
        prom(&reg).value("logins_total", &[("user", "50")]),
        Some(1.0)
    );

    assert_eq!(Registry::new().family("x"), None);
    let fresh = Registry::new();
    fresh.counter("x", "h", &[]);
    assert_eq!(fresh.family("x").unwrap().max_series, DEFAULT_MAX_SERIES);
}

// ---- capability additions ------------------------------------------------------

#[test]
fn float_instruments_encode_fractional_values() {
    let reg = Registry::new();
    let c = reg.float_counter("cpu_seconds_total", "CPU", &[("mode", "user")]);
    c.inc();
    c.inc_by(0.25);
    let g = reg.float_gauge("load_ratio", "Load", &[]);
    g.set(0.75);
    g.add(0.5);
    g.sub(1.0);
    g.inc();
    g.dec();
    assert_eq!(c.get(), 1.25);
    assert_eq!(g.get(), 0.25);

    let doc = prom(&reg);
    assert_eq!(doc.types["cpu_seconds_total"], "counter");
    assert_eq!(doc.types["load_ratio"], "gauge");
    assert_eq!(
        doc.value("cpu_seconds_total", &[("mode", "user")]),
        Some(1.25)
    );
    assert_eq!(doc.value("load_ratio", &[]), Some(0.25));
    // Integer and float variants of a name do not mix.
    assert!(reg
        .try_counter("cpu_seconds_total", "CPU", &[("mode", "sys")])
        .is_err());
    assert!(reg.try_gauge("load_ratio", "Load", &[]).is_err());

    // A gauge may legitimately be NaN or infinite; it is spelled out.
    g.set(f64::NAN);
    assert!(reg.encode().contains("load_ratio NaN\n"));
    g.set(f64::NEG_INFINITY);
    assert_eq!(prom(&reg).value("load_ratio", &[]), Some(f64::NEG_INFINITY));

    // Integer counters keep full 64-bit precision in the output.
    let big = reg.counter("big_total", "h", &[]);
    big.inc_by(u64::MAX);
    assert!(reg.encode().contains("big_total 18446744073709551615\n"));
}

#[test]
fn vec_handles_bind_label_values_in_declaration_order() {
    let reg = Registry::new();
    let reqs = reg
        .try_counter_vec("http_requests_total", "Requests", &["status", "method"])
        .unwrap();
    assert_eq!(reqs.name(), "http_requests_total");
    assert_eq!(reqs.label_names(), ["status", "method"]);

    let ok = reqs.with_labels(&["200", "GET"]);
    ok.inc();
    reqs.clone().with_labels(&["200", "GET"]).inc_by(2);
    assert_eq!(ok.get(), 3, "the same series every time");
    // The name+labels API reaches the same series.
    reg.counter(
        "http_requests_total",
        "Requests",
        &[("method", "GET"), ("status", "200")],
    )
    .inc();
    assert_eq!(ok.get(), 4);

    let doc = prom(&reg);
    assert_eq!(
        doc.value(
            "http_requests_total",
            &[("method", "GET"), ("status", "200")]
        ),
        Some(4.0)
    );

    // Wrong arity: an error from try_, a detached instrument otherwise.
    assert_eq!(
        reqs.try_with_labels(&["200"]).unwrap_err(),
        MetricsError::LabelArity {
            metric: "http_requests_total".into(),
            expected: 2,
            got: 1,
        }
    );
    reqs.with_labels(&["200"]).inc();
    assert_eq!(reg.registration_errors(), 1);
    assert_eq!(prom(&reg).named("http_requests_total").len(), 1);

    // remove() drops one series from the output.
    assert!(reqs.remove(&["200", "GET"]));
    assert!(!reqs.remove(&["200", "GET"]));
    assert!(prom(&reg).named("http_requests_total").is_empty());

    // Every kind has a vec.
    reg.gauge_vec("g", "h", &["a"]).with_labels(&["1"]).set(5);
    reg.float_gauge_vec("fg", "h", &["a"])
        .with_labels(&["1"])
        .set(0.5);
    reg.float_counter_vec("fc_total", "h", &["a"])
        .with_labels(&["1"])
        .inc_by(0.5);
    reg.histogram_vec("hv", "h", &[], &[1.0])
        .with_labels(&[])
        .observe(2.0);
    let doc = prom(&reg);
    assert_eq!(doc.value("g", &[("a", "1")]), Some(5.0));
    assert_eq!(doc.value("fg", &[("a", "1")]), Some(0.5));
    assert_eq!(doc.value("fc_total", &[("a", "1")]), Some(0.5));
    assert_eq!(doc.value("hv_count", &[]), Some(1.0));

    // A vec of the wrong kind is detached and counted.
    let before = reg.registration_errors();
    reg.gauge_vec("http_requests_total", "Requests", &["status"])
        .with_labels(&["500"])
        .set(1);
    assert_eq!(reg.registration_errors(), before + 1);
    assert_eq!(prom(&reg).types["http_requests_total"], "counter");
}

#[test]
fn raii_guards_record_on_drop() {
    let reg = Registry::new();
    let in_flight = reg.gauge("in_flight", "h", &[]);
    let latency = reg.histogram("latency_seconds", "h", &[], DEFAULT_BUCKETS);

    {
        let _a = in_flight.track();
        let _b = in_flight.track();
        assert_eq!(in_flight.get(), 2);
        let _timer = latency.start_timer();
        assert_eq!(latency.count(), 0, "nothing recorded while running");
    }
    assert_eq!(in_flight.get(), 0, "guards decrement on drop");
    assert_eq!(latency.count(), 1, "the timer observes on drop");
    assert!(latency.sum() >= 0.0);

    let secs = latency.start_timer().observe_duration();
    assert!(secs >= 0.0);
    assert_eq!(latency.count(), 2, "observe_duration records exactly once");
    latency.start_timer().discard();
    assert_eq!(latency.count(), 2, "a discarded timer records nothing");

    // A panic unwinding through the guards still releases them.
    let (g, h) = (in_flight.clone(), latency.clone());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _busy = g.track();
        let _timer = h.start_timer();
        panic!("handler failed");
    }));
    assert!(result.is_err());
    assert_eq!(in_flight.get(), 0);
    assert_eq!(latency.count(), 3);

    let doc = prom(&reg);
    assert_eq!(doc.value("in_flight", &[]), Some(0.0));
    assert_eq!(doc.value("latency_seconds_count", &[]), Some(3.0));
}

#[test]
fn unregister_remove_clear_and_reset() {
    let reg = Registry::new();
    let a = reg.counter("a_total", "h", &[("k", "1")]);
    let b = reg.counter("a_total", "h", &[("k", "2")]);
    let g = reg.gauge("g", "h", &[]);
    let h = reg.histogram("h_seconds", "h", &[], &[1.0]);
    a.inc();
    b.inc_by(2);
    g.set(7);
    h.observe(0.5);

    // reset(): values zeroed, registrations and handles kept.
    reg.reset();
    assert_eq!((a.get(), b.get(), g.get(), h.count()), (0, 0, 0, 0));
    assert_eq!(h.sum(), 0.0);
    a.inc();
    assert_eq!(prom(&reg).value("a_total", &[("k", "1")]), Some(1.0));
    assert_eq!(reg.families().len(), 3);

    // remove(): one series.
    assert!(reg.remove("a_total", &[("k", "2")]));
    assert!(!reg.remove("a_total", &[("k", "2")]));
    assert!(!reg.remove("nope", &[]));
    assert_eq!(prom(&reg).named("a_total").len(), 1);

    // unregister(): the whole family; the name is free for another kind.
    assert!(reg.unregister("a_total"));
    assert!(!reg.unregister("a_total"));
    a.inc(); // an old handle is harmless
    assert!(prom(&reg).named("a_total").is_empty());
    reg.try_gauge("a_total", "now a gauge", &[]).unwrap().set(3);
    assert_eq!(prom(&reg).types["a_total"], "gauge");

    // clear(): everything, including the diagnostic counters.
    reg.gauge("a_total", "h", &[("bad-label", "x")]);
    assert_eq!(reg.registration_errors(), 1);
    reg.clear();
    assert_eq!(reg.encode(), "");
    assert_eq!(reg.registration_errors(), 0);
    assert_eq!(reg.last_registration_error(), None);
    assert!(reg.families().is_empty());
}

#[test]
fn metadata_accessors_report_help_unit_and_kind() {
    let reg = Registry::new();
    reg.histogram(
        "job_duration_seconds",
        "Job duration",
        &[("q", "a")],
        &[1.0],
    );
    assert!(reg.set_unit("job_duration_seconds", "seconds"));
    assert!(!reg.set_unit("missing", "seconds"));

    let info = reg.family("job_duration_seconds").unwrap();
    assert_eq!(info.name, "job_duration_seconds");
    assert_eq!(info.help, "Job duration");
    assert_eq!(info.unit.as_deref(), Some("seconds"));
    assert_eq!(info.kind, MetricKind::Histogram);
    assert_eq!(info.kind.type_str(), "histogram");
    assert_eq!(info.series, 1);
    assert_eq!(reg.families(), vec![info]);

    // The first registration's help text and buckets win.
    let again = reg.histogram("job_duration_seconds", "other", &[("q", "b")], &[9.0]);
    assert_eq!(again.bounds(), &[1.0]);
    assert_eq!(prom(&reg).helps["job_duration_seconds"], "Job duration");
    assert!(reg.set_unit("job_duration_seconds", ""));
    assert_eq!(reg.family("job_duration_seconds").unwrap().unit, None);
}

#[test]
fn collectors_refresh_values_at_encode_time() {
    let reg = Arc::new(Registry::new());
    let depth = Arc::new(AtomicUsize::new(3));
    let runs = Arc::new(AtomicUsize::new(0));
    {
        let (depth, runs) = (depth.clone(), runs.clone());
        reg.register_collector(move |r| {
            runs.fetch_add(1, Ordering::Relaxed);
            r.gauge("queue_depth", "Queue depth", &[])
                .set(depth.load(Ordering::Relaxed) as i64);
            // A nested encode must not run the collectors again.
            let nested = r.encode();
            assert!(nested.contains("queue_depth"));
        });
    }
    assert_eq!(prom(&reg).value("queue_depth", &[]), Some(3.0));
    depth.store(8, Ordering::Relaxed);
    assert_eq!(prom(&reg).value("queue_depth", &[]), Some(8.0));
    assert_eq!(runs.load(Ordering::Relaxed), 2, "once per top-level encode");
}

#[test]
fn process_collector_reports_start_time_and_uptime() {
    let reg = Registry::new();
    reg.register_process_collector();
    reg.register_process_collector(); // idempotent

    let doc = prom(&reg);
    let start = doc
        .value("process_start_time_seconds", &[])
        .expect("start time");
    let uptime = doc.value("process_uptime_seconds", &[]).expect("uptime");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!(start > 1.0e9 && start <= now + 1.0, "start={start}");
    assert!(uptime >= 0.0 && uptime.is_finite());
    assert_eq!(doc.types["process_start_time_seconds"], "gauge");
    assert_eq!(doc.named("process_uptime_seconds").len(), 1);

    if cfg!(target_os = "linux") {
        assert_eq!(doc.types["process_cpu_seconds_total"], "counter");
        assert!(doc.value("process_resident_memory_bytes", &[]).unwrap() > 0.0);
        assert!(doc.value("process_threads", &[]).unwrap() >= 1.0);
        assert!(doc.value("process_open_fds", &[]).unwrap() >= 1.0);
    } else {
        assert!(doc.named("process_resident_memory_bytes").is_empty());
    }
    // The OpenMetrics rendering of the same registry is valid too.
    parse(&reg.encode_openmetrics(), true);
}

#[test]
fn openmetrics_format_rules() {
    let reg = Registry::new();
    reg.counter("http_requests_total", "Requests", &[("code", "200")])
        .inc_by(3);
    reg.counter("events", "No suffix", &[]).inc();
    reg.gauge("temperature_celsius", "Temp", &[]).set(21);
    reg.histogram("rpc_duration_seconds", "RPC", &[], &[0.5, 1.0])
        .observe(0.75);
    reg.float_counter("cpu_seconds_total", "CPU", &[])
        .inc_by(1.5);
    assert!(reg.set_unit("rpc_duration_seconds", "seconds"));
    assert!(reg.set_unit("temperature_celsius", "celsius"));
    assert!(reg.set_unit("cpu_seconds_total", "seconds"));
    assert!(reg.set_unit("events", "bytes")); // not a suffix: no UNIT line

    let text = reg.encode_openmetrics();
    assert!(text.ends_with("# EOF\n"), "{text}");
    assert_eq!(text.matches("# EOF").count(), 1);
    let doc = parse(&text, true);

    // Counters: the family drops `_total`, the sample always carries it.
    assert_eq!(doc.types["http_requests"], "counter");
    assert!(!doc.types.contains_key("http_requests_total"));
    assert_eq!(
        doc.value("http_requests_total", &[("code", "200")]),
        Some(3.0)
    );
    assert_eq!(doc.types["events"], "counter");
    assert_eq!(doc.value("events_total", &[]), Some(1.0));
    assert_eq!(doc.value("cpu_seconds_total", &[]), Some(1.5));

    // Units.
    assert_eq!(doc.units["rpc_duration_seconds"], "seconds");
    assert_eq!(doc.units["temperature_celsius"], "celsius");
    assert_eq!(doc.units["cpu_seconds"], "seconds");
    assert!(!doc.units.contains_key("events"));
    assert!(text.contains("# UNIT rpc_duration_seconds seconds\n"));

    // Histogram `le` values are floats.
    assert!(
        text.contains("rpc_duration_seconds_bucket{le=\"1.0\"} 1\n"),
        "{text}"
    );
    assert!(text.contains("rpc_duration_seconds_bucket{le=\"0.5\"} 0\n"));
    assert!(text.contains("rpc_duration_seconds_bucket{le=\"+Inf\"} 1\n"));
    assert_eq!(doc.value("temperature_celsius", &[]), Some(21.0));

    // The Prometheus rendering is unchanged by all of this.
    let prom_text = reg.encode();
    assert!(!prom_text.contains("# EOF") && !prom_text.contains("# UNIT"));
    assert!(prom_text.contains("# TYPE http_requests_total counter\n"));
    assert!(prom_text.contains("events 1\n"));
    parse(&prom_text, false);
    assert_eq!(Registry::new().encode_openmetrics(), "# EOF\n");
}

#[test]
fn standalone_histogram_needs_no_registry() {
    let h = Arc::new(Histogram::new(&[2.0, 1.0]));
    h.observe(1.5);
    drop(h.start_timer());
    assert_eq!(h.count(), 2);
    assert_eq!(h.bounds(), &[1.0, 2.0]);
}
