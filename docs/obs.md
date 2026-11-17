# `ferroly::obs` — distributed span & event tracing

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `obs` · **Module:** `ferroly::obs`

Records units of work as **spans** (a named, timed operation) and **events** (a
point-in-time note on a span). Spans nest: a span started while another is
current becomes its child, inheriting the same trace id and pointing back at its
parent — so a whole request forms one causally-linked tree, across threads,
`.await` points and (through the W3C `traceparent` header) across services.
Finished spans are handed to pluggable **exporters** (JSON or human-readable
text to a writer, or OTLP to a collector).

This complements [`log`](log.md) (structured logging) and
[`metrics`](metrics.md) (counters/gauges/histograms + Prometheus) — it is the
*tracing* leg of observability.

Enable with the `obs` feature (`obs = ["codec"]`, std-only); the OTLP exporter
and the HTTP client trace interceptor also need `http`:

```toml
ferroly = { version = "0.7", features = ["obs"] }                     # spans + JSON / plain-text export
ferroly = { version = "0.7", features = ["obs", "http"] }             # + OTLP/HTTP (JSON) export
ferroly = { version = "0.7", features = ["obs", "http", "protobuf"] } # + OTLP/HTTP protobuf
ferroly = { version = "0.7", features = ["obs", "grpc"] }             # + OTLP/gRPC
```

## At a glance

| You have | Use |
|---|---|
| synchronous code | `Span::new("x").enter()` → `SpanGuard` (current on this thread until dropped) |
| a future | `obs::instrument(fut, Span::new("x")).await` (current during each poll) |
| a parent from somewhere else | `Span::new("x").child_of(&ctx).start()` → `ActiveSpan` |
| a task to spawn | `spawn(obs::with_current_context(fut))` |
| an inbound request | `obs::extract(\|name\| headers.get(name))` → `Option<Context>` |
| an outbound request | `obs::inject(&ctx, &mut \|name, value\| headers.set(name, value))` |

## Spans

```rust
use ferroly::obs::{Level, Span, SpanKind, Status};

# fn handle() {
let root = Span::new("request")
    .level(Level::Info)
    .kind(SpanKind::Server)
    .field("path", "/health")   // typed fields (str, int, bool, float, …)
    .enter();                   // returns a SpanGuard

{
    let q = Span::new("db.query").field("rows", 3).enter();  // child of `request`
    q.record("cache", false);
    q.set_status(Status::Ok);
}   // `q` finishes here; `root` finishes when it drops
# }
```

### The builder: `Span`

| Method | Effect |
|---|---|
| `Span::new(name)` | Start building; level `Info`, kind `Internal`. |
| `.level(Level)` | Severity; spans below the global threshold are not recorded. |
| `.kind(SpanKind)` | `Internal`, `Server`, `Client`, `Producer`, `Consumer`. |
| `.field(key, value)` | Attach a typed field (any `Into<Value>`). |
| `.link(Link)` | Reference another span that is not the parent. |
| `.child_of(&Context)` | **Explicit parent** — overrides whatever is current. |
| `.child_of_opt(Option<&Context>)` | Explicit parent when there is one (pairs with `extract`). |
| `.root()` | Start a new trace, ignoring the current context. |
| `.enter() -> SpanGuard` | Start it and make it current on this thread. |
| `.start() -> ActiveSpan` | Start it **without** making it current. |
| `.in_scope(\|\| …)` | Run a closure inside the entered span. |

Parent resolution when a span starts: the explicit parent, else the context
current on the calling thread, else none (a new trace with a fresh trace id).

### The handles: `ActiveSpan` and `SpanGuard`

`ActiveSpan` is a started span that is not tied to a thread. It is
`Send + Sync`, and **ends when dropped** (or with `.end()`).

| Method | Effect |
|---|---|
| `context() -> Option<Context>` | The span's context, for children / other tasks / `inject`. `None` for an inert (filtered) span. |
| `record(key, value)` | Add a field. |
| `event(level, message, fields)` | Add an event with its own fields. |
| `set_status(Status)` / `set_error(msg)` | Outcome: `Unset`, `Ok`, `Error(message)`. `Ok` is final; `Unset` never overwrites. |
| `set_name(name)` | Rename (an HTTP server span learns its route after routing). |
| `add_link(Link)` | Add a link. |
| `is_recording() -> bool` | Skip computing expensive fields when `false`. |
| `enter() -> ContextGuard` | Make it current on this thread **without** ending it. |
| `end()` | End and export now. |

`SpanGuard` (from `Span::enter`) is an `ActiveSpan` that is also current on the
entering thread; it dereferences to `ActiveSpan`, so all of the above is
available on it. It is `!Send`. Dropping it removes the span from the thread's
context and ends it.

Per span, at most `MAX_SPAN_FIELDS` (256) fields, `MAX_SPAN_EVENTS` (1024) events
and `MAX_SPAN_LINKS` (128) links are kept; later ones are discarded.

### Operations on the current span

```rust
use ferroly::obs::{self, Level, Status};

# fn f() {
obs::record("user.id", 42);                   // -> bool: was there a current span?
obs::set_status(Status::error("declined"));   // or obs::set_error("declined")
obs::event(Level::Warn, "retrying", vec![("attempt".into(), 2i64.into())]);
# }
```

`event` attaches to the current span. With **no** current span it emits a
standalone zero-duration span named `"event"` carrying the event, so nothing is
lost.

## Identifiers

`TraceId` (16 bytes) and `SpanId` (8 bytes) display as 32 / 16 lowercase hex
characters. They are generated by a per-thread **xoshiro256\*\*** generator
seeded from the wall clock, a monotonic clock, the process id, the thread id, a
stack address, a heap address and `std::collections::hash_map::RandomState`
(which the standard library keys from operating-system entropy). Ids are never
all-zero and never sequential, so traces from different processes, threads and
restarts do not collide.

`TraceId::random()`, `SpanId::random()`, `from_hex` (strict: exact length,
lowercase, non-zero), `from_bytes`, `to_bytes` are public.

They are identifiers, not secrets: do not use them as session tokens.

## Context: who is the parent?

A `Context` is the identity of a span — trace id, span id, sampled flag,
`tracestate` — as a small value that is **`Send + Sync + Clone`**.

```rust
ctx.trace_id() / ctx.span_id()        // TraceId / SpanId
ctx.is_sampled() / ctx.is_remote()    // flags
ctx.trace_state() -> Option<&str>
ctx.attach() -> ContextGuard          // current on this thread until the guard drops
ctx.scope(|| …)                       // run a closure with it current
ctx.wrap(future) -> WithContext<F>    // current whenever the future is polled
obs::current() -> Option<Context>     // what is current right now
```

The current context lives in a per-thread stack of *attachments*. Three rules
make it safe:

1. **Every attachment is removed by its own identity, never by position.**
   Guards can be dropped in any order; an out-of-order drop removes exactly its
   own entry.
2. **A span owns its data.** Fields, events, status and the start time live in
   the span, not in the stack, so a span always finishes as itself — it can
   never pick up another span's fields or duration.
3. **A wrapped future owns its part of the stack.** `instrument`,
   `with_context` and `with_current_context` install the future's context (and
   any guards the future is holding) on the polling thread for the duration of
   each poll, and take them back when the poll returns.

### Async code

```rust
use ferroly::obs::{self, Instrument, Span};

async fn load_user(id: u64) -> Option<String> {
    obs::record("user.id", id);     // lands on the `load_user` span
    None
}

# async fn demo() {
// Free-function form …
let user = obs::instrument(load_user(7), Span::new("load_user")).await;
// … or the method form.
let user = load_user(7).instrument(Span::new("load_user")).await;
# let _ = user;
# }
```

`instrument(future, span)`:

- starts the span immediately, so its parent is the context current at the call;
- makes it current **on each poll**, on whichever thread polls — nothing is held
  across an `.await`, so it is correct on work-stealing runtimes;
- ends the span when the future completes **or is dropped** (cancellation);
- accepts a `Span` builder or an already started `ActiveSpan`.

It is runtime-agnostic: no Tokio task-local is involved, so it works with any
executor and needs only the `obs` feature.

**Concurrent branches and spawned tasks** do not inherit a context by themselves.
Carry it explicitly:

```rust
use ferroly::obs::{self, Span};

# async fn a() {} async fn b() {}
# fn spawn<F: std::future::Future + Send + 'static>(_f: F) {}
# async fn demo() {
let _request = Span::new("request").enter();

// Each branch gets its own span, both children of `request`.
let fa = obs::instrument(a(), Span::new("a"));
let fb = obs::instrument(b(), Span::new("b"));
// futures::join!(fa, fb) / tokio::join!(fa, fb)
# drop((fa, fb));

// A spawned task inherits the current context …
spawn(obs::with_current_context(async { /* children of `request` */ }));
// … or gets a span of its own.
spawn(obs::instrument(async {}, Span::new("background job")));
# }
```

**Rule of thumb:** do not hold a `SpanGuard` across an `.await` in a *bare*
future. While that future is suspended its span would still look current to
whatever else is polled on the same thread (a sibling `join!` branch would
adopt it as parent). Spans never corrupt each other even then — rules 1 and 2
hold — but the parent link would be wrong. Inside an `instrument`ed (or
`with_current_context`-wrapped) future the same guard is fine: it travels with
the future.

### Explicit parents

When there is no ambient context to rely on — a worker thread, a callback, a
queue consumer — pass the parent as a value:

```rust
use ferroly::obs::Span;

let parent = Span::new("batch").start();
let ctx = parent.context().expect("recording");

std::thread::spawn(move || {
    let item = Span::new("item").child_of(&ctx).start();   // no thread-local needed
    item.record("n", 1);
});
```

## W3C Trace Context

```rust
use ferroly::obs::{self, Context};

// Parse / format the header directly …
let ctx = Context::from_traceparent(
    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
).unwrap();
assert_eq!(ctx.to_traceparent(), "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");

// … or go through any header-like carrier. No HTTP types are involved.
# let headers: std::collections::HashMap<&str, &str> = Default::default();
let parent: Option<Context> = obs::extract(|name| headers.get(name).copied());

let mut out: Vec<(String, String)> = Vec::new();
obs::inject(&ctx, &mut |name, value| out.push((name.into(), value.into())));
```

| Function | Signature |
|---|---|
| `Context::from_traceparent` | `(header: &str) -> Result<Context, TraceParentError>` |
| `Context::to_traceparent` | `(&self) -> String` |
| `obs::extract` / `Context::extract` | `(get: impl Fn(&str) -> Option<&str>) -> Option<Context>` |
| `obs::inject` / `Context::inject` | `(ctx: &Context, set: &mut impl FnMut(&str, &str))` |
| `obs::inject_current` | `(set: &mut impl FnMut(&str, &str)) -> bool` |

`get` and `set` are called with the lowercase header names `traceparent` and
`tracestate` (`TRACEPARENT_HEADER`, `TRACESTATE_HEADER`).

**Validation** of `traceparent` is strict: `version-traceid-spanid-flags` with
2 / 32 / 16 / 2 **lowercase** hex digits, non-zero trace and span ids, and
version `ff` rejected. Version `00` must have exactly four fields. As the
specification asks, a *higher* version is accepted when its first four fields
are valid (extra fields are ignored). An invalid header is ignored — `extract`
returns `None` and the request starts a new trace.

**`tracestate`** is passed through unchanged. A value that is empty, longer than
1024 bytes, or contains control characters is dropped (the trace itself is
kept).

**Flags:** only the *sampled* bit is read and propagated. `to_traceparent`
always writes version `00`.

A typical server + client hop:

```rust
use ferroly::obs::{self, http};

# fn header(_n: &str) -> Option<&'static str> { None }
# fn set_header(_n: &str, _v: &str) {}
// Inbound: adopt the caller's trace.
let parent = obs::extract(header);
let server = http::server_span("GET", "/users/:id")
    .child_of_opt(parent.as_ref())
    .start();

// Outbound: put the *client* span's context on the wire.
let client = http::client_span("POST", "https://billing.internal/charge")
    .child_of_opt(server.context().as_ref())
    .start();
if let Some(ctx) = client.context() {
    obs::inject(&ctx, &mut |name, value| set_header(name, value));
}
http::record_client_status(&client, 200);
client.end();
http::record_server_status(&server, 200);
```

## Sampling

```rust
use ferroly::obs::{set_sampler, Sampler};

// Keep 10 % of the traces that start here; follow the caller for the rest.
set_sampler(Sampler::parent_based(Sampler::TraceIdRatio(0.1)));
```

| `Sampler` | Decision |
|---|---|
| `AlwaysOn` | record every span |
| `AlwaysOff` | record nothing |
| `TraceIdRatio(f64)` | record that fraction of traces; a pure function of the trace id (the OpenTelemetry rule), so every span of a trace agrees |
| `ParentBased(Box<Sampler>)` | follow the parent's sampled flag (local or remote); use the inner sampler for root spans |

The default is `ParentBased(AlwaysOn)`: everything is recorded unless an upstream
service marked the trace as not sampled. `sampler()` returns the one in force.

The decision is made once, when a span starts, and **propagates**: a child sees
it through its parent's `Context::is_sampled`, and other services see it as the
flags byte of `traceparent` (`-01` sampled, `-00` not). An unsampled span still
has a valid context — so the "no" keeps travelling — but records nothing
(`is_recording()` is `false`) and is never exported.

This is head sampling. Tail sampling (deciding after the trace finished) belongs
in a collector.

## Level filtering

```rust
use ferroly::obs::{set_level, level, Level};
set_level(Level::Debug);
assert_eq!(level(), Level::Debug);
```

`set_level` / `level` gate the whole module. A span below the threshold is
*inert*: it has no context, records nothing, and is transparent — its children
attach to the nearest recorded ancestor. Events below the threshold are dropped.
Order: `Trace < Debug < Info < Warn < Error`.

## Exporters

Until an exporter is installed, finishing a span is a near-free no-op.

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
| `trait Exporter { export(&FinishedSpan); flush(); shutdown() }` | a sink for finished spans (`Send + Sync`); `flush` and `shutdown` have defaults |
| `set_exporter(Box<dyn Exporter>)` | install one exporter, replacing all others |
| `add_exporter(Box<dyn Exporter>)` | install another alongside (fan-out) |
| `clear_exporters()` | remove all, without flushing |
| `flush()` | flush every installed exporter |
| `shutdown()` | shut every exporter down (each flushes) and remove them — call once at exit |
| `JsonExporter::new(writer)` | built-in: one JSON object per span to any `Write` |
| `PlainExporter::new(writer)` | built-in: human-readable lines in `log`'s plain style ([below](#plainexporter-output)) |

Exporters are called with **no internal lock held**: an exporter may call
`set_exporter`, `flush`, or start spans of its own. Spans that finish on a
thread while it is inside an exporter call are not exported, and
`obs::suppress(future)` turns span recording off for a whole future — the OTLP
exporter wraps its own I/O in it, so an instrumented HTTP client does not
produce telemetry about sending telemetry.

### `FinishedSpan`

| Field | Type | Notes |
|---|---|---|
| `name` | `String` | |
| `trace_id` / `span_id` | `String` | 32 / 16 lowercase hex |
| `parent_id` | `Option<String>` | `None` for a root |
| `parent_remote` | `bool` | the parent came from a `traceparent` header |
| `trace_state` | `Option<String>` | W3C `tracestate` |
| `level` | `Level` | |
| `kind` | `SpanKind` | |
| `status` | `Status` | `Unset` / `Ok` / `Error(message)` |
| `start_unix_nanos` / `duration_nanos` | `u128` | |
| `fields` | `Vec<(String, Value)>` | typed |
| `events` | `Vec<SpanEvent>` | `level`, `message`, `fields`, `unix_nanos` |
| `links` | `Vec<Link>` | `trace_id`, `span_id`, `trace_state`, `fields` |

`FinishedSpan` implements `Default`, so tests and custom exporters can use
struct-update syntax: `FinishedSpan { name: "x".into(), ..Default::default() }`.

### `JsonExporter` output

```json
{"name":"GET /users/:id","trace_id":"4bf9…4736","span_id":"a1b2…","parent_id":"00f0…02b7",
 "level":"info","start_unix_nanos":1700000000000000000,"duration_nanos":1500000,
 "fields":{"http.route":"/users/:id"},"events":[],
 "kind":"server","status":"error","status_message":"HTTP 503",
 "links":[…],"trace_state":"vendor=1","parent_remote":true}
```

`kind` and `status` are always present; `status_message`, `links`, `trace_state`
and `parent_remote` only when they carry something.

### `PlainExporter` output

The span counterpart of [`log`](log.md)'s plain format, for an application
that offers a human output mode: pick one format and both logs and spans follow
it.

```rust
use std::time::Duration;
use ferroly::{log, obs};

# let human = true;
if human {
    log::set_global(log::Logger::new());
    obs::set_exporter(Box::new(
        obs::PlainExporter::new(std::io::stderr()).min_duration(Duration::from_micros(100)),
    ));
} else {
    log::set_global(log::Logger::json());
    obs::set_exporter(Box::new(obs::JsonExporter::new(std::io::stderr())));
}
```

One line per finished span, written when it ends, then one indented line per
event:

```text
2026-11-15T09:12:03.120Z INFO  server listening addr=0.0.0.0:8080
2026-11-15T09:12:03.121Z INFO  span request 48.2ms trace=4bf92f35 route=/v1/chat/completions tokens=24
  event "first token" +12.3ms n=1
2026-11-15T09:12:03.170Z INFO  request finished status=200
```

| Part | Format |
|---|---|
| span line | `<start> <LEVEL> span <name> <duration> trace=<8 hex> key=value …` |
| event line | `  event <name> +<offset from span start> key=value …` |
| status | a failed span (`Status::Error`) gets `status=error error=<message>` after the ids (`error=` left out for an empty message); `Ok` and `Unset` print nothing |
| start | RFC 3339, UTC, milliseconds (see the options below) |
| `LEVEL` | uppercase, padded to five columns, as in `log` |
| duration / offset | `ns` below 1 µs, then `µs`, `ms`, `s` with three significant digits (`750ns`, `12.3µs`, `48.2ms`, `2.50s`); whole seconds from 1000 s |
| names, keys, values | exactly as `log`'s plain format (the same code): strings bare unless empty or holding whitespace, a control character, `=`, `"` or `\` (then quoted and escaped); numbers, booleans and `null` as is; arrays and objects as compact JSON |

A repeated field key prints once with its last value, and a field named like
one of the line's own keys (`trace`, and `span` / `parent` / `status` / `error`
when printed) moves to the end as `fields.<key>=…`. A standalone event (the
zero-duration `event` span that `obs::event` emits with no current span) is
written as one line, `<time> <LEVEL> event <message> trace=… key=value …`.
`obs::event` marks that span as it hands it to the exporters, so any other
span — even a real one named `event` — is written as a span.

| Option | Effect |
|---|---|
| `.with_ids(true)` | full ids: `trace=<32 hex> span=<16 hex>` instead of the 8-digit trace prefix |
| `.with_parent(true)` | adds `parent=<id>` to spans that have a parent (8 hex digits, or 16 with `with_ids`) |
| `.min_duration(Duration)` | skips spans shorter than this (and their events); standalone events are always written |
| `.time_precision(Precision)` | `Seconds`, `Millis` (default), `Micros`, `Nanos` — the same type as `log::Precision` |
| `.utc_offset(seconds_east)` | timestamps at a fixed offset (`+05:30`), like `log::Logger::utc_offset` |
| `.redact(log::Redactor)` | masks sensitive span and event fields like a `log::Logger` does (with the `log` feature; no redaction otherwise) |

Like `JsonExporter`, the writer sits behind a mutex and each span — its line and
its event lines — is written with one `write_all` under it, so spans finishing
on many threads never interleave within a line. Nothing is buffered by the
exporter: wrap the writer in a `BufWriter` to batch, and `obs::flush()` /
`obs::shutdown()` flush it. Span kind, status and links are not printed; use
`JsonExporter` or OTLP when you need them.

## OTLP export (`obs` + `http`)

`OtlpHttpExporter` batches spans and `POST`s them to an OTLP `/v1/traces`
endpoint as OTLP JSON (the default), using the in-house [`http`](http.md)
client — or as OTLP protobuf over HTTP, or over OTLP/gRPC (see
[Protocols](#protocols)).

```rust,no_run
use std::time::Duration;
use ferroly::obs::{self, OtlpHttpExporter, Resource};

# async fn setup() {
// Call from inside a Tokio runtime — it spawns a background sender.
let exporter = OtlpHttpExporter::builder("http://localhost:4318/v1/traces")
    .resource(
        Resource::new("checkout")
            .service_version("1.4.2")
            .service_instance_id("checkout-7d9f-abcde")
            .deployment_environment("production")
            .attribute("cloud.region", "eu-west-1"),
    )
    .header("authorization", "Bearer <token>")
    .timeout(Duration::from_secs(5))
    .build();
obs::set_exporter(Box::new(exporter.clone()));   // keep a clone for stats / shutdown

// … on the way out:
exporter.shutdown_async().await;
let stats = exporter.stats();
if stats.dropped_spans + stats.failed_spans > 0 {
    eprintln!("tracing lost spans: {stats:?}");
}
# }
```

`OtlpHttpExporter::new(endpoint, service_name)` is the short form with default
settings.

### Configuration (`OtlpBuilder`)

| Method | Default | Meaning |
|---|---|---|
| `resource(Resource)` / `service_name(name)` | `service.name = unknown_service` | who emits the spans |
| `header(name, value)` | none | sent with every request — auth tokens, tenant ids |
| `protocol(OtlpProtocol)` | `HttpJson` | the wire protocol; see [Protocols](#protocols) |
| `tls(ClientTlsConfig)` | web PKI roots, TLS 1.2+ | trusted roots (a private CA), a client certificate (mutual TLS), minimum version |
| `proxy(Proxy)` | none | a forward proxy (`https` collectors through a `CONNECT` tunnel) |
| `http_client(Client)` | none | send with this client instead; `tls` and `proxy` are then ignored |
| `timeout(Duration)` | 10 s | one request, connect to last byte |
| `batch_size(n)` | 512 | spans per request; a full batch is sent at once |
| `queue_size(n)` | 2048 | finished spans waiting to be sent |
| `interval(Duration)` | 2 s | how often a partial batch is sent |
| `max_retries(n)` | 3 | re-sends after a transient failure |
| `retry_backoff(Duration)` | 200 ms | first retry pause; doubles each time, capped at 30 s |
| `flush_timeout(Duration)` | 5 s | the longest `flush` / `shutdown` wait |

**Transport precedence.** A client given to `http_client` wins: it keeps its
own TLS, proxy, pool and interceptors, and the exporter only sets its
response-head timeout to `timeout` (`Client::with_timeout`) and bounds each
export as a whole by the same timeout. Otherwise the exporter builds a client
from `tls` and `proxy`. TLS options that cannot be built (for example no
trusted root at all) do not fail `build()`: every batch then fails, without a
retry, and `stats().last_error` says why. A TLS handshake failure (an
untrusted certificate) is not retried either.

A collector behind a private CA, with mutual TLS:

```rust,no_run
use ferroly::http::ClientTlsConfig;
use ferroly::obs::OtlpHttpExporter;

# async fn setup() -> Result<(), ferroly::http::HttpError> {
# let (ca_pem, cert_pem, key_pem) = (b"", b"", b"");
let tls = ClientTlsConfig::new()
    .without_web_pki_roots()
    .add_root_pem(ca_pem)?
    .identity_pem(cert_pem, key_pem)?;
let exporter = OtlpHttpExporter::builder("https://otel.internal:4318/v1/traces")
    .tls(tls)
    .build();
# let _ = exporter;
# Ok(())
# }
```

### Delivery guarantees

- **Bounded.** `export` never blocks: it `try_send`s into a queue of
  `queue_size`. When the collector is slow or down and the queue is full, the
  span is **dropped and counted** (`dropped_spans`) — memory does not grow.
- **Checked.** A batch counts as exported only on a **2xx** response.
- **Retried.** A transport error (other than a TLS failure), a timeout, or HTTP
  **429 / 502 / 503 / 504** is retried with exponential backoff (a
  `Retry-After` header in seconds is honoured). Over gRPC the retried statuses
  are the ones the OTLP specification lists: `CANCELLED`, `DEADLINE_EXCEEDED`,
  `RESOURCE_EXHAUSTED`, `ABORTED`, `OUT_OF_RANGE`, `UNAVAILABLE`, `DATA_LOSS`.
  Any other status is a permanent failure and is not retried.
- **Reported.** A batch that is given up on is counted (`failed_batches`,
  `failed_spans`) and described in `last_error`; it is then released, so a broken
  collector cannot pin memory.
- **Flushable.** `flush_async().await` and `shutdown_async().await` return only
  after the background task has attempted delivery of everything queued — `true`
  when every batch was accepted, `false` on a failure, a timeout
  (`flush_timeout`), or a stopped exporter. `shutdown` also stops the task;
  spans exported afterwards are dropped and counted.

The synchronous `Exporter::flush` / `Exporter::shutdown` (what `obs::flush()` and
`obs::shutdown()` call) block the calling thread for the same result — from a
plain thread, or from a worker of a multi-thread runtime. On a **current-thread**
runtime a blocking wait could never succeed (the background task shares the
thread), so there the call queues the request and returns immediately: use the
`_async` forms.

`stats()` returns an `OtlpStats` snapshot: `exported_spans`, `dropped_spans`,
`failed_spans`, `sent_batches`, `failed_batches`, `retries`, `last_error`.

### Protocols

| `OtlpProtocol` | Feature | Endpoint | Body |
|---|---|---|---|
| `HttpJson` (default) | `http` | `http(s)://host:4318/v1/traces` | OTLP JSON, `application/json` |
| `HttpProtobuf` | `http` + `protobuf` | `http(s)://host:4318/v1/traces` | `ExportTraceServiceRequest`, `application/x-protobuf` |
| `Grpc` | `grpc` | `http://host:4317` (h2c) or `https://host:4317`, no path | a unary `opentelemetry.proto.collector.trace.v1.TraceService/Export` call |

```rust,no_run
# #[cfg(feature = "protobuf")]
# async fn setup() {
use ferroly::obs::{OtlpHttpExporter, OtlpProtocol};

let exporter = OtlpHttpExporter::builder("http://localhost:4318/v1/traces")
    .protocol(OtlpProtocol::HttpProtobuf)
    .build();
# let _ = exporter;
# }
```

The protobuf body is encoded by hand with [`ferroly::proto::wire`](proto.md)
from the opentelemetry-proto field numbers: ids are raw bytes (16 and 8),
times `fixed64` nanoseconds, `flags` a `fixed32`. Over gRPC, `header(..)`
values are sent as metadata (names lowercased), the per-call deadline is
`timeout`, and `http_client(..)` must have HTTP/2 enabled for the endpoint;
through a `proxy` only an `https` collector can be reached (HTTP/2 needs the
`CONNECT` tunnel).

### Wire format

Standard OTLP/JSON (`resourceSpans → scopeSpans → spans`):

| OTLP field | Encoding |
|---|---|
| `traceId`, `spanId`, `parentSpanId` | lowercase hex strings |
| `traceState` | string, when present |
| `flags` | number: W3C flags in the low byte, `0x100` set, `0x200` when the parent is remote |
| `kind` | number: 1 internal, 2 server, 3 client, 4 producer, 5 consumer |
| `startTimeUnixNano`, `endTimeUnixNano`, `timeUnixNano` | decimal **strings** (64-bit) |
| `attributes` | `[{key, value}]`; `stringValue`, `boolValue`, `intValue` (string), `doubleValue`, `bytesValue` (base64), `arrayValue`, `kvlistValue` |
| `events` | `[{timeUnixNano, name, attributes}]` |
| `links` | `[{traceId, spanId, traceState?, attributes}]` |
| `status` | omitted when unset; `{code: 1}` for ok; `{message, code: 2}` for error |
| `resource.attributes` | the `Resource` |
| `scope` | `{name: "ferroly", version}` |

The protobuf encoding carries the same values. One difference: an unsigned
field above `i64::MAX`, which `int_value` cannot hold, is sent as its decimal
`string_value`.

**Not implemented:** gzip compression, and parsing of a `partialSuccess`
response (JSON, protobuf or gRPC). Only traces are exported (no OTLP logs or
metrics).

## Semantic-convention helpers

Small builders so that instrumentation written in different layers names things
the same way. They take plain strings and numbers — no dependency on other
modules.

| Helper | Span |
|---|---|
| `obs::http::server_span(method, route) -> Span` | kind `Server`, name `"{method} {route}"`, fields `http.request.method`, `http.route`. `route` is the **template** (`/users/:id`), never the raw path. |
| `obs::http::set_route(&span, method, route)` | rename + `http.route`, for when routing happens after the span started |
| `obs::http::record_server_status(&span, status)` | `http.response.status_code`; `5xx` → error status |
| `obs::http::client_span(method, url) -> Span` | kind `Client`, name `"{method}"`, fields `http.request.method`, `url.full` (credentials, query and fragment removed), `server.address`, `server.port` |
| `obs::http::record_client_status(&span, status)` | `http.response.status_code`; `4xx`/`5xx` → error status |
| `obs::messaging::publish_span(system, destination) -> Span` | kind `Producer`, name `"send {destination}"` |
| `obs::messaging::process_span(system, destination) -> Span` | kind `Consumer`, name `"process {destination}"` |
| `obs::genai::chat_span(provider, model) -> Span` | kind `Client`, name `"chat {model}"`, `gen_ai.*` fields |
| `obs::genai::record_usage(&span, input_tokens, output_tokens)` | `gen_ai.usage.*` |

Unknown HTTP methods are recorded as `_OTHER`. The `record_*` and `set_route`
helpers take `&ActiveSpan`, which a `SpanGuard` dereferences to.

Built-in instrumentation that uses these helpers (each behind the `obs` feature):

- [turbo](turbo.md)'s `trace_context()` middleware extracts W3C trace context
  and runs a server span per request.
- The [rest](rest.md) client injects `traceparent` into outgoing requests.
- [genai](genai.md)'s `ObsObserver` records a span per model call with token usage.
- `obs::TraceInterceptor` traces a raw [`http`](http.md) client (below).
- [messaging](messaging.md)'s `Traced` wrapper records producer and consumer
  spans around any provider and carries the context in the message.

### Tracing a raw `http::Client` (`obs` + `http`)

```rust,no_run
use ferroly::http::{Client, Method, Request};
use ferroly::obs::TraceInterceptor;

# async fn ex() -> Result<(), ferroly::http::HttpError> {
let client = Client::builder().interceptor(TraceInterceptor::new()).build()?;
let resp = client
    .send(Request::builder(Method::Get, "http://inventory:8080/items")?.build())
    .await?;
# let _ = resp;
# Ok(())
# }
```

For every request (every redirect hop) `TraceInterceptor` starts a `Client`
span with `obs::http::client_span` as a child of the current context, injects
its `traceparent` / `tracestate`, and ends it when the response head arrives
(`record_client_status`: `4xx`/`5xx` → error) or the request fails (the error
becomes the status). The span covers the time to the response head, not the
body.

- A request that already has a `traceparent` (set by the caller) is left
  alone: no span, no header change.
- With no recorded span (level filter, `obs::suppress`) the current context,
  if any, is still injected.
- At most 1024 spans are open at once (`TraceInterceptor::with_capacity(n)`);
  beyond that the oldest is ended early, so requests that never complete (a
  dropped future) cannot leak. An evicted span carries the attribute
  `ferroly.span.evicted = true` (`obs::EVICTED_ATTRIBUTE`) and no status: the
  request may still succeed, its outcome is just not recorded. `open_spans()`
  reports the open count and `evicted_spans()` the evictions so far. Set the
  capacity above the most requests the client has in flight at once; past it,
  spans of live requests are evicted too. A redirect hop of an evicted request
  is still recognised as the interceptor's own.

## Linking logs to traces

With the `log` feature, `ferroly::log::add_trace_context_provider()` adds the
current span's `trace_id` and `span_id` to every log record — see
[log](log.md#linking-logs-to-traces-obs).

## Limitations

- **OTLP export is traces only and uncompressed** (no gzip); a
  `partialSuccess` answer is not inspected.
- **Head sampling only**; no tail sampling, no per-span sampling attributes.
- **Instrumentation is opt-in**: turbo's `trace_context()`, the
  `TraceInterceptor` for an `http::Client`, messaging's `Traced`, genai's
  `ObsObserver`. The `http` server creates no spans by itself.
- **`tracestate` is opaque** — passed through, never modified.
- **Bare `SpanGuard`s across `.await`** can mis-parent sibling futures polled on
  the same thread (never corrupt them); wrap futures with `instrument` or
  `with_current_context`.
- **Ids are not secrets.** After `fork()` without `exec`, a child process would
  continue the parent's per-thread id sequence.
- Exporter, level and sampler are **process-global**.
- **`PlainExporter` prints a subset** — no kind, links, `tracestate` or
  per-event levels (a span's status only when it is an error); it is meant for
  people at a terminal, not for machines.

## See also

- [log](log.md) — structured logging (levels, JSON, fields, trace correlation).
- [metrics](metrics.md) — counters/gauges/histograms + Prometheus exposition.
- [http](http.md) — the client the OTLP exporter sends through.

---
**Related:** [log](log.md), [metrics](metrics.md), [http](http.md).
