# ferroly::metrics

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `metrics` — module `ferroly::metrics`. No external dependencies.

## Overview

`metrics` is a tiny, dependency-free metrics registry that renders the standard
**Prometheus / OpenMetrics text exposition format** — ready to scrape from a
`/metrics` endpoint. It provides the three core instrument types and a
process-global registry, and the [`turbo`](turbo.md) router can record RED
metrics (Rate, Errors, Duration) into it with a single middleware.

- [`Counter`](#counter) — a monotonically increasing value.
- [`Gauge`](#gauge) — a value that goes up and down.
- [`Histogram`](#histogram) — bucketed observations plus a running sum and count.
- [`Registry`](#registry) — holds instruments keyed by name + labels, and
  [`encode`](#encode)s them to Prometheus text.

Instruments are handed back as `Arc<…>`, so the hot path only does relaxed atomic
operations — no lock per increment.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.2", features = ["metrics"] }
# with the router integration:
ferroly = { version = "0.2", features = ["turbo", "metrics"] }
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

### Counter

| Method | Description |
|---|---|
| `inc()` | Increment by one. |
| `inc_by(n: u64)` | Increment by `n`. |
| `get() -> u64` | Current value. |

### Gauge

| Method | Description |
|---|---|
| `set(n: i64)` / `get() -> i64` | Set / read. |
| `inc()` / `dec()` / `add(n: i64)` | Adjust (up or down). |

### Histogram

| Method | Description |
|---|---|
| `observe(v: f64)` | Record one observation into the matching bucket. |
| `count() -> u64` | Number of observations. |
| `sum() -> f64` | Sum of observed values. |

Buckets are fixed at construction. [`DEFAULT_BUCKETS`] matches Prometheus'
client-library latency defaults (`0.005 … 10.0` seconds). On encode, bucket
counts are emitted cumulatively (`le="…"`), with the implicit `+Inf` bucket, plus
`_sum` and `_count` series.

Pass your own bounds when the default latency buckets don't fit the quantity you
are measuring (e.g. payload sizes in bytes). Bounds may be given in any order —
they are sorted, and `NaN` values are dropped — and the `+Inf` overflow bucket is
always added implicitly:

```rust
use ferroly::metrics::Registry;

let reg = Registry::new();
let sizes = reg.histogram(
    "payload_bytes", "Request payload size", &[],
    &[256.0, 1024.0, 4096.0, 65536.0],   // custom bounds
);
sizes.observe(900.0);   // falls in the le="1024" bucket
sizes.observe(50000.0); // falls in the le="65536" bucket

assert_eq!(sizes.count(), 2);
assert!(reg.encode().contains("payload_bytes_bucket{le=\"1024\"} 1"));
```

## Registry

| Method | Description |
|---|---|
| `new()` | An empty registry. |
| `counter(name, help, labels) -> Arc<Counter>` | Get-or-create a counter series. |
| `gauge(name, help, labels) -> Arc<Gauge>` | Get-or-create a gauge series. |
| `histogram(name, help, labels, bounds) -> Arc<Histogram>` | Get-or-create a histogram series. |
| `encode() -> String` | Render all metrics in Prometheus text format. |

Each distinct **label set** under a name is its own time series. Repeated calls
with the same name + labels return the same instrument, so you can fetch a handle
on the hot path (or cache it once).

### The process-global registry

`ferroly::metrics::global()` returns a lazily-created shared [`Registry`]. It is
what the `turbo` integration records into and what a `/metrics` route serves. Use
it when you want application code and the router to publish into one registry
without threading a `Registry` handle around:

```rust
use ferroly::metrics::global;

// Anywhere in the process, fetch-or-create a series and record into it.
let jobs = global().counter("jobs_processed_total", "Jobs processed", &[("queue", "email")]);
jobs.inc();

// A `/metrics` handler (or the turbo `metrics_route`) renders the same registry.
let body = global().encode();
assert!(body.contains("jobs_processed_total{queue=\"email\"} 1"));
```

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
  - `http_requests_total` — counter, labelled by `method` + `status`.
  - `http_request_duration_seconds` — histogram, labelled by `method`.
  - `http_requests_in_flight` — gauge.
- **`metrics_route(path)`** registers a `GET` route serving
  `global().encode()` with `content-type: text/plain; version=0.0.4`.

Scrape it like any Prometheus target:

```
scrape_configs:
  - job_name: my-service
    static_configs:
      - targets: ["my-service:8080"]
```

## Exposition format

```
# HELP http_requests_total Total HTTP requests
# TYPE http_requests_total counter
http_requests_total{method="GET",status="200"} 5
# HELP http_request_duration_seconds HTTP request latency in seconds
# TYPE http_request_duration_seconds histogram
http_request_duration_seconds_bucket{method="GET",le="0.005"} 3
http_request_duration_seconds_bucket{method="GET",le="+Inf"} 5
http_request_duration_seconds_sum{method="GET"} 0.081
http_request_duration_seconds_count{method="GET"} 5
```

Label values and `# HELP` text are escaped (`\`, `"`, newline) per the exposition
format.

## Design notes & limitations

- **Zero dependencies** — the encoder and instruments are hand-rolled; no
  `prometheus`/`metrics` crate.
- **Cardinality is the caller's responsibility** — each distinct label set is a
  retained series. Keep label values low-cardinality (method, status, route
  template — not user IDs or raw paths).
- **No push / OTLP export** — this is a *pull* (scrape) surface. Exporting to an
  OTLP collector or a push gateway is left to the surrounding app.
- **Kind consistency** — reuse a metric name with the same instrument type; a
  name requested first as a counter and later as a gauge returns a detached
  handle rather than panicking.

## See also

- [turbo](turbo.md) — the router that records RED metrics.
- [log](log.md) — structured logging with trace-ID correlation (the other half
  of observability).
- [lifecycle](lifecycle.md) — health probes for k8s liveness/readiness.

---
**Related:** [turbo](turbo.md), [log](log.md), [lifecycle](lifecycle.md).
