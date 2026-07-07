# `ferroly::obs` — distributed span & event tracing

Records units of work as **spans** (a named, timed operation) and **events** (a
point-in-time note on a span). Spans nest: a span opened while another is active
becomes its child, inheriting the same `trace_id` and pointing back at its
parent — so a whole request forms one causally-linked tree. Finished spans are
handed to a pluggable **exporter** (JSON to a writer, or OTLP to a collector).

This complements the existing [`log`](log.md) (structured logging) and
[`metrics`](metrics.md) (counters/gauges/histograms + Prometheus) modules — it
adds the *tracing* leg of observability.

Enable with the `obs` feature (`obs = ["codec"]`); the OTLP exporter also needs
`http`:

```toml
ferroly = { version = "0.1", features = ["obs"] }              # spans + JSON export
ferroly = { version = "0.1", features = ["obs", "http"] }      # + OTLP/HTTP export
```

## Spans

```rust
use ferroly::obs::{Level, Span};

# fn handle() {
let root = Span::new("request")
    .level(Level::Info)
    .field("path", "/health")   // typed fields (str, int, bool, float, …)
    .enter();                   // returns a SpanGuard

{
    let _q = Span::new("db.query").field("rows", 3).enter();  // child of `request`
    // … work …
}   // `_q` finishes here (LIFO); `root` finishes when it drops
# }
```

- `Span::new(name).level(..).field(k, v).enter()` opens a span; the returned
  `SpanGuard` closes it on drop and records its duration.
- **Guards must drop in scope (LIFO) order** — the current span is a thread-local
  stack. `SpanGuard` is `!Send` so it can't accidentally cross threads.
- `guard.record(k, v)` adds a field after entering; `guard.event(level, msg,
  fields)` attaches an event.

## Events

```rust
use ferroly::obs::{event, Level, Span};

# fn f() {
let _s = Span::new("job").enter();
event(Level::Warn, "retrying", vec![("attempt".into(), 2i64.into())]);
# }
```

`event(level, message, fields)` attaches to the current span. With **no** active
span it emits a standalone zero-duration span named `"event"` carrying the event
(fresh `trace_id`, no parent), so nothing is lost.

## Level filtering

```rust
use ferroly::obs::{set_level, level, Level};
set_level(Level::Debug);
assert_eq!(level(), Level::Debug);
```

`set_level` / `level` gate the whole module. Spans and events below the
threshold are dropped **cheaply** — a filtered `enter()` returns an inert guard
without touching the thread-local stack, and finishing is a no-op when no
exporter is installed. Order: `Trace < Debug < Info < Warn < Error`.

## Exporters

Install one process-wide exporter; until then, finishing a span is a near-free
no-op.

```rust
use ferroly::obs::{set_exporter, JsonExporter};
// One compact JSON object per finished span, written to stderr.
set_exporter(Box::new(JsonExporter::new(std::io::stderr())));
```

Implement the trait to send spans anywhere:

```rust
use ferroly::obs::{Exporter, FinishedSpan};

struct Count(std::sync::atomic::AtomicUsize);
impl Exporter for Count {
    fn export(&self, _span: &FinishedSpan) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
```

| Item | Purpose |
|---|---|
| `trait Exporter { export(&FinishedSpan); flush() }` | a sink for finished spans (`Send + Sync`) |
| `set_exporter(Box<dyn Exporter>)` / `flush()` | install / flush the process-wide exporter |
| `JsonExporter::new(writer)` | built-in: one JSON object per span to any `Write` |
| `FinishedSpan` / `SpanEvent` | the exported records (name, ids, level, timings, fields, events) |

## OTLP export (`obs` + `http`)

`OtlpHttpExporter` batches spans and `POST`s them to an OTLP `/v1/traces`
endpoint as OTLP JSON, using the in-house [`http`](http.md) client. Because a
span finishes in a synchronous `Drop`, it hands spans to a background task over
a channel — `export` never blocks — which batches and sends them. Best-effort:
transport errors are dropped so telemetry never breaks the app.

```rust,no_run
use ferroly::obs::{self, OtlpHttpExporter};

# async fn setup() {
// Call from inside a Tokio runtime — it spawns a background sender.
obs::set_exporter(Box::new(OtlpHttpExporter::new(
    "http://localhost:4318/v1/traces",
    "my-service",
)));
# }
```

The document is standard OTLP JSON (`resourceSpans → scopeSpans → spans`) with
`service.name` on the resource; 64-bit ints and nanosecond timestamps are
carried as strings, and `trace_id`/`span_id` as hex, per the OTLP/JSON encoding.

## Limitations

- **No `Span` kinds / status codes** beyond the internal kind and a severity
  `Level`; extend via fields if you need more.
- **Ids are process-unique, not globally random** — a per-process seed plus
  atomic counters (no randomness dependency). Fine for correlation within a
  service; not a cryptographic identifier.
- **OTLP export is best-effort and batched** — spans may be dropped on collector
  failure; call `flush()` (or rely on the periodic flush) before shutdown.
- Sampling is not built in — filter with `set_level`, or drop in a custom
  `Exporter`.

## See also

- [log](log.md) — structured logging (levels, JSON, fields).
- [metrics](metrics.md) — counters/gauges/histograms + Prometheus exposition.
- [http](http.md) — the client the OTLP exporter sends through.
