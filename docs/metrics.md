# ferroly::metrics

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `metrics` — module `ferroly::metrics`. No external dependencies
(std-only).

## Overview

`metrics` is a small, dependency-free metrics registry that renders the
**Prometheus text exposition format** (and, alternatively, **OpenMetrics 1.0**)
— ready to scrape from a `/metrics` endpoint. It provides five instrument types
and a process-global registry, and the [`turbo`](turbo.md) router can record RED
metrics (Rate, Errors, Duration) into it with a single middleware.

- [`Counter`](#counter--floatcounter) / `FloatCounter` — monotonically increasing values.
- [`Gauge`](#gauge--floatgauge) / `FloatGauge` — values that go up and down.
- [`Histogram`](#histogram) — bucketed observations plus a running sum and count.
- [`Registry`](#registry) — holds instruments keyed by name + labels, validates
  registrations, caps cardinality, and [encodes](#exposition-formats) to text.
- [`MetricVec`](#pre-bound-label-handles-metricvec) — a family with pre-declared
  label names, for cheap per-request series selection.
- [RAII guards](#raii-guards) — a histogram timer and an in-flight gauge guard.
- [Collectors](#collectors-and-process-metrics) — callbacks that sample values at
  scrape time, including a built-in process collector.

Instruments are handed back as `Arc<…>`, so the hot path only does relaxed atomic
operations — no lock per update once you hold the handle.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.3", features = ["metrics"] }
# with the router integration:
ferroly = { version = "0.3", features = ["turbo", "metrics"] }
```

## Quick start

```rust
use ferroly::metrics::Registry;

let reg = Registry::new();
let hits = reg.counter("hits_total", "Total hits", &[("route", "/")]);
hits.inc();
hits.inc_by(4);

let inflight = reg.gauge("inflight", "In-flight requests", &[]);
inflight.inc();

let latency = reg.histogram(
    "latency_seconds", "Handler latency", &[],
    ferroly::metrics::DEFAULT_BUCKETS,
);
latency.observe(0.042);

print!("{}", reg.encode()); // Prometheus text
```

## Instruments

### Counter / FloatCounter

| Method | `Counter` | `FloatCounter` |
|---|---|---|
| `inc()` | +1 | +1.0 |
| `inc_by(n)` | `n: u64` | `v: f64` — negative, `NaN` and infinite deltas are ignored (a counter never goes down) |
| `get()` | `u64` | `f64` |

Integer counters are written to the exposition output as exact integers (the full
`u64` range, no float rounding).

### Gauge / FloatGauge

| Method | `Gauge` | `FloatGauge` |
|---|---|---|
| `set(v)` / `get()` | `i64` | `f64` (may be `NaN`/`±Inf`; rendered as `NaN`, `+Inf`, `-Inf`) |
| `inc()` / `dec()` | ±1 | ±1.0 |
| `add(v)` / `sub(v)` | `i64` | `f64` — a `NaN` delta is ignored |
| `track()` | returns an [`InFlightGuard`](#raii-guards) | — |

Float instruments store the `f64` bit pattern in an `AtomicU64` and update it with
a compare-and-swap loop, so they are lock-free like the integer ones.

### Histogram

| Method | Description |
|---|---|
| `Histogram::new(bounds: &[f64])` | A standalone histogram outside any registry. |
| `observe(v: f64)` | Record one observation. **`NaN` and `±Inf` are ignored** so one bad sample cannot poison `_sum`. |
| `count() -> u64` | Number of observations. |
| `sum() -> f64` | Sum of observed values. |
| `bounds() -> &[f64]` | The finite bucket upper bounds, ascending. |
| `buckets() -> Vec<(f64, u64)>` | A snapshot of the cumulative buckets; the last pair is `(+Inf, count)`. |
| `start_timer()` | Returns a [`HistogramTimer`](#raii-guards). |

`DEFAULT_BUCKETS` matches Prometheus' client-library latency defaults
(`0.005 … 10.0` seconds).

Bucket bounds may be given in any order. They are **sorted, de-duplicated, and
non-finite values (`NaN`, `±Inf`) are dropped**; the `+Inf` bucket is always
added implicitly. A value equal to a bound falls in that bound's bucket
(`le` is inclusive). The bounds belong to the **family**: the first registration
of a name fixes them for every series of that name, and `bounds` passed by later
calls are ignored.

On encode, one snapshot of the bucket counters produces the cumulative
`_bucket{le="…"}` lines **and** `_count`, so the `+Inf` bucket always equals
`_count` even while other threads are observing.

```rust
use ferroly::metrics::Registry;

let reg = Registry::new();
let sizes = reg.histogram(
    "payload_bytes", "Request payload size", &[],
    &[4096.0, 256.0, 1024.0, 65536.0, 1024.0],   // any order, duplicates fine
);
sizes.observe(900.0);   // falls in the le="1024" bucket
sizes.observe(50000.0); // falls in the le="65536" bucket

assert_eq!(sizes.bounds(), &[256.0, 1024.0, 4096.0, 65536.0]);
assert_eq!(sizes.count(), 2);
assert!(reg.encode().contains("payload_bytes_bucket{le=\"1024\"} 1"));
```

## Registry

| Method | Description |
|---|---|
| `new()` | An empty registry (cardinality cap `DEFAULT_MAX_SERIES` = 2000 per family). |
| `with_max_series(max)` | An empty registry with a different per-family cap. |
| `counter` / `float_counter` / `gauge` / `float_gauge` `(name, help, labels)` | Get-or-create a series. Infallible — see [Registration rules](#registration-rules). |
| `histogram(name, help, labels, bounds)` | Get-or-create a histogram series. |
| `try_counter` / `try_float_counter` / `try_gauge` / `try_float_gauge` / `try_histogram` | The same, returning `Result<Arc<T>, MetricsError>`. |
| `counter_vec` / `float_counter_vec` / `gauge_vec` / `float_gauge_vec` `(name, help, label_names)`, `histogram_vec(name, help, label_names, bounds)` | Declare a family with fixed label names → a [`MetricVec`](#pre-bound-label-handles-metricvec). |
| `try_*_vec(…)` | The same, returning `Result`. |
| `encode() -> String` | Prometheus text format 0.0.4 (`PROMETHEUS_CONTENT_TYPE`). |
| `encode_openmetrics() -> String` | OpenMetrics 1.0 text (`OPENMETRICS_CONTENT_TYPE`). |
| `registration_errors() -> u64` / `last_registration_error()` | Rejected registrations seen by the infallible constructors. |
| `series_overflows() -> u64` | Lookups redirected to an overflow series by the cardinality guard. |
| `set_default_max_series(max)` / `set_max_series(name, max) -> bool` | Configure the [cardinality guard](#cardinality-guard). |
| `set_unit(name, unit) -> bool` | Attach a unit to a family. |
| `family(name) -> Option<FamilyInfo>` / `families() -> Vec<FamilyInfo>` | Metadata: `name`, `help`, `unit`, `kind`, `series`, `max_series`. |
| `unregister(name) -> bool` | Remove a family and all its series. |
| `remove(name, labels) -> bool` | Remove one series. |
| `reset()` | Zero every instrument, keep all registrations (cached handles stay attached). |
| `clear()` | Remove every family and collector and zero the diagnostic counters. |
| `register_collector(f)` / `register_process_collector()` | See [Collectors](#collectors-and-process-metrics). |

Each distinct **label set** under a name is its own time series. Repeated calls
with the same name + labels return the same instrument, so you can fetch a handle
once and keep it.

### Registration rules

The metric **kind is a property of the family** (the name), fixed by the first
registration. A registration is rejected with a `MetricsError` when:

| Error | Cause |
|---|---|
| `InvalidMetricName` | the name does not match `[a-zA-Z_:][a-zA-Z0-9_:]*` |
| `InvalidLabelName` | a label name does not match `[a-zA-Z_][a-zA-Z0-9_]*`, or starts with the reserved `__` prefix |
| `ReservedLabel` | `le` or `quantile` on a histogram (the format owns them) |
| `DuplicateLabel` | the same label name twice in one label set |
| `KindMismatch` | the name is already registered with another kind (integer and float variants are different kinds: `Counter` ≠ `FloatCounter`) |
| `NameCollision` | the name would duplicate a histogram's sample names — a histogram `x` owns `x_bucket`, `x_sum`, `x_count` |
| `LabelArity` | a `MetricVec` was given the wrong number of label values |

How the two API families report it:

- **`try_*`** constructors return the error.
- The **infallible** constructors (`counter`, `gauge`, `histogram`, `*_vec`,
  `with_labels`, …) never panic and never emit invalid output. They return a
  **detached** instrument — it accepts updates but is not part of the registry,
  so it is never exported — and increment `registration_errors()`. Check that
  counter (and `last_registration_error()`) in a startup self-test or expose it
  as a gauge; a non-zero value means some metric is misdeclared.

`is_valid_metric_name(&str)` and `is_valid_label_name(&str)` are public if you
need to pre-check names that come from configuration.

The first registration also fixes the family's `# HELP` text.

```rust
use ferroly::metrics::{MetricsError, Registry};

let reg = Registry::new();
reg.counter("jobs_total", "Jobs", &[]).inc();

// Same name, different kind: an error from try_*, a detached handle otherwise.
assert!(matches!(
    reg.try_gauge("jobs_total", "Jobs", &[]),
    Err(MetricsError::KindMismatch { .. })
));
reg.gauge("jobs_total", "Jobs", &[]).set(5);       // detached, not exported
assert_eq!(reg.registration_errors(), 1);
assert_eq!(reg.encode().matches("# TYPE jobs_total").count(), 1);
```

### Cardinality guard

Every family holds at most `max_series` distinct label sets (default
`DEFAULT_MAX_SERIES` = **2000**; change it with `Registry::with_max_series`,
`set_default_max_series` for families created later, or `set_max_series(name, n)`
for one existing family).

When a family is full, a **new** label set is not added. Instead the lookup
resolves to the family's single **overflow series**, exported with the label
`overflow="true"` (`OVERFLOW_LABEL`), and `series_overflows()` is incremented.
Existing series keep working. This keeps totals correct (nothing is dropped —
`sum(metric)` still counts every increment) while bounding memory when a label
value turns out to be unbounded (user ids, raw paths).

```rust
use ferroly::metrics::Registry;

let reg = Registry::with_max_series(2);
for user in ["a", "b", "c", "d"] {
    reg.counter("logins_total", "Logins", &[("user", user)]).inc();
}
let text = reg.encode();
assert!(text.contains("logins_total{user=\"a\"} 1"));
assert!(text.contains("logins_total{overflow=\"true\"} 2")); // c and d
assert_eq!(reg.series_overflows(), 2);
```

### Pre-bound label handles (`MetricVec`)

`reg.counter(name, help, labels)` takes the registry lock (shared) to find the
family on every call. For per-request updates, declare the family once and
select series by **label values**:

```rust
use ferroly::metrics::Registry;

let reg = Registry::new();
// Declare once (for example when building a router) …
let requests = reg.counter_vec("http_requests_total", "Requests", &["method", "status"]);
let latency = reg.histogram_vec(
    "http_request_duration_seconds", "Latency", &["method"],
    ferroly::metrics::DEFAULT_BUCKETS,
);

// … then per request: values in declaration order, no registry lock.
requests.with_labels(&["GET", "200"]).inc();
latency.with_labels(&["GET"]).observe(0.012);

// The returned Arc is a plain handle: cache it to skip the lookup entirely.
let get_ok = requests.with_labels(&["GET", "200"]);
get_ok.inc();
```

`CounterVec`, `FloatCounterVec`, `GaugeVec`, `FloatGaugeVec` and `HistogramVec`
are aliases of `MetricVec<T>`. A vec is `Clone` (cheap, shared).

| Method | Description |
|---|---|
| `with_labels(&[&str]) -> Arc<T>` | The series for these values; wrong arity → detached instrument + `registration_errors()`. |
| `try_with_labels(&[&str]) -> Result<Arc<T>, MetricsError>` | The same, reporting `LabelArity`. |
| `remove(&[&str]) -> bool` | Drop one series from the family. |
| `name()` / `label_names()` | Metadata. |

A vec and the `name + labels` API address the same series, and the cardinality
guard applies to both.

### RAII guards

Two guards make request instrumentation safe against early returns, panics and
**cancelled futures** — the measurement is taken in `Drop`:

```rust
use ferroly::metrics::{Registry, DEFAULT_BUCKETS};

let reg = Registry::new();
let in_flight = reg.gauge("http_requests_in_flight", "In flight", &[]);
let latency = reg.histogram("http_request_duration_seconds", "Latency", &[], DEFAULT_BUCKETS);

{
    let _busy = in_flight.track();        // +1 now, -1 on drop
    let _timer = latency.start_timer();   // observes elapsed seconds on drop
    // … handle the request (may `.await`, return early, or be cancelled) …
}
assert_eq!(in_flight.get(), 0);
assert_eq!(latency.count(), 1);
```

- `Gauge::track(&Arc<Gauge>) -> InFlightGuard`
- `Histogram::start_timer(&Arc<Histogram>) -> HistogramTimer`, with
  `observe_duration(self) -> f64` (stop now, return the seconds) and
  `discard(self)` (record nothing).

Both guards own an `Arc` to their instrument, so they are `Send` and can be moved
into a spawned task or held across `.await`.

### Collectors and process metrics

`register_collector(|reg: &Registry| …)` installs a callback that runs at the
start of every `encode()` / `encode_openmetrics()`, with **no registry lock
held**, to refresh instruments whose values are sampled rather than pushed:

```rust
use ferroly::metrics::Registry;

let reg = Registry::new();
reg.register_collector(|r| {
    r.gauge("queue_depth", "Jobs waiting", &[]).set(current_depth());
});
assert!(reg.encode().contains("queue_depth 3"));
# fn current_depth() -> i64 { 3 }
```

A collector that itself calls `encode()` does not re-run the collectors.

`register_process_collector()` (opt-in, idempotent) adds:

| Metric | Type | Platforms |
|---|---|---|
| `process_start_time_seconds` | gauge | all — off Linux this is the moment the collector was first registered, a close approximation when metrics are set up at startup |
| `process_uptime_seconds` | gauge | all |
| `process_cpu_seconds_total` | counter | Linux |
| `process_resident_memory_bytes` | gauge | Linux |
| `process_virtual_memory_bytes` | gauge | Linux |
| `process_threads` | gauge | Linux |
| `process_open_fds` | gauge | Linux |
| `process_max_fds` | gauge | Linux (omitted when the limit is `unlimited`) |

The Linux values are parsed from `/proc/self/{stat,status,fd,limits}` and
`/proc/stat` with plain `std::fs` — no `unsafe`, no dependency. CPU time and
start time assume the standard 100 Hz `USER_HZ` clock tick. A value that cannot
be read is simply omitted; on other platforms only the first two are emitted.

### The process-global registry

`ferroly::metrics::global()` returns a lazily-created shared `Registry`. It is
what the `turbo` integration records into and what a `/metrics` route serves:

```rust
use ferroly::metrics::global;

let jobs = global().counter("jobs_processed_total", "Jobs processed", &[("queue", "email")]);
jobs.inc();

let body = global().encode();
assert!(body.contains("jobs_processed_total{queue=\"email\"}"));
```

Tests that share the global registry can call `global().reset()` (zero the
values, keep handles attached) or `unregister(name)`; prefer an isolated
`Registry::new()` where possible.

## Router integration (`turbo` + `metrics`)

With both features enabled, the [`Router`](turbo.md) gains two methods:

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let router = Router::new()
    .metrics()                 // RED middleware → global registry
    .metrics_route("/metrics") // GET /metrics → Prometheus text
    .get("/", |_ctx| async move { HttpResponse::text(StatusCode::OK, "ok") });
```

- **`metrics()`** installs a middleware that records, per request:
  - `http_requests_total` — counter, labelled by `method`, `route` and `status`.
  - `http_request_duration_seconds` — histogram, labelled by `method` and `route`.
  - `http_requests_in_flight` — gauge.

  Every label comes from a bounded set, so a client cannot create series:
  `method` is a standard method or `OTHER`; `route` is the matched route
  *template* (`/users/:id`, never the raw path) or `unmatched`; `status` is the
  response status, or `cancelled` when the request was dropped before a response.
  The in-flight gauge and the timer are RAII guards, so a cancelled request does
  not leak a count. Use `metrics_in(&registry)` to record into your own registry
  instead of the global one.
- **`metrics_route(path)`** registers a `GET` route serving
  `global().encode()` with `content-type: text/plain; version=0.0.4`.

Scrape it like any Prometheus target:

```
scrape_configs:
  - job_name: my-service
    static_configs:
      - targets: ["my-service:8080"]
```

## Exposition formats

### Prometheus text (`encode`)

```
# HELP http_requests_total Total HTTP requests
# TYPE http_requests_total counter
http_requests_total{method="GET",route="/users/:id",status="200"} 5
# HELP http_request_duration_seconds HTTP request latency in seconds
# TYPE http_request_duration_seconds histogram
http_request_duration_seconds_bucket{method="GET",route="/users/:id",le="0.005"} 3
http_request_duration_seconds_bucket{method="GET",route="/users/:id",le="+Inf"} 5
http_request_duration_seconds_sum{method="GET",route="/users/:id"} 0.081
http_request_duration_seconds_count{method="GET",route="/users/:id"} 5
```

- Families are ordered by name, series by label set; labels within a series are
  ordered by label name, with `le` last on bucket lines.
- Exactly one `# HELP` and one `# TYPE` per family.
- Label values escape `\` → `\\`, `"` → `\"`, newline → `\n`; `# HELP` text
  escapes `\` and newline.
- Floats: integral values print without a decimal point, very large or small
  magnitudes use exponent form (`1e300`), and `NaN` / `+Inf` / `-Inf` are spelled
  out.

### OpenMetrics (`encode_openmetrics`)

The same data in OpenMetrics 1.0 text, for scrapers that negotiate
`application/openmetrics-text`:

```
# HELP http_requests Total HTTP requests
# TYPE http_requests counter
http_requests_total{method="GET",route="/users/:id",status="200"} 5
# HELP rpc_duration_seconds RPC latency
# TYPE rpc_duration_seconds histogram
# UNIT rpc_duration_seconds seconds
rpc_duration_seconds_bucket{le="1.0"} 4
rpc_duration_seconds_bucket{le="+Inf"} 5
rpc_duration_seconds_sum 2.5
rpc_duration_seconds_count 5
# EOF
```

- **Counters:** the family name drops a trailing `_total` and every counter
  sample carries it (`hits` → family `hits`, sample `hits_total`).
- **`# UNIT`** is emitted for families given a unit with `set_unit`, when the
  family name ends in `_<unit>` (the format requires that; otherwise the line is
  omitted).
- `le` values are float literals (`"1.0"`), `# HELP` additionally escapes `"`,
  and the document ends with `# EOF`.

Not implemented: the `summary` type, exemplars, `_created` samples, and
timestamps.

## Design notes & limitations

- **Zero dependencies** — the encoder and instruments are hand-rolled; no
  `prometheus`/`metrics` crate.
- **Keep labels low-cardinality** — the guard bounds the damage, but a family
  that keeps overflowing is a modelling bug: use route templates, not raw paths.
- **No push / OTLP metrics export** — this is a *pull* (scrape) surface.
- **No summaries or exemplars**; histograms have fixed buckets (no native /
  exponential histograms).
- **No runtime (tokio task) collectors** — `metrics` is std-only; sample what you
  need with `register_collector`.
- Removing a family or series (`unregister`, `remove`, `clear`) detaches handles
  already held elsewhere: they keep working but are no longer exported. Use
  `reset()` when cached handles must stay attached.

## See also

- [turbo](turbo.md) — the router that records RED metrics.
- [log](log.md) — structured logging with trace-ID correlation.
- [obs](obs.md) — distributed tracing.
- [lifecycle](lifecycle.md) — health probes for k8s liveness/readiness.

---
**Related:** [turbo](turbo.md), [log](log.md), [obs](obs.md), [lifecycle](lifecycle.md).
