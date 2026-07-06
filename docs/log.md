# ferroly::log

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `log` · **Module:** `ferroly::log`

## Overview

`log` is a dependency-free **structured logger** built for enterprise
services: typed structured fields, RFC 3339 timestamps on every record,
plain-text *or* JSON-lines output, level filtering at two layers, a
process-global logger, request-context / trace-ID propagation, and an opt-in
non-blocking background appender that sheds load instead of stalling the hot
path.

Everything is built on [`ferroly::codec::Value`](codec.md) — field values keep
their type (a number stays a number in JSON output, not a quoted string) — with
no external logging framework, no `serde`, and no allocation-heavy formatting
layer.

Deliberately **out of scope** (left to the sink or the surrounding app, to keep
the module small and vendor-neutral):

- **Log rotation** — point the sink at a file that your rotation tooling manages,
  or a writer that rotates itself.
- **Sampling** — apply it in a `Write` wrapper or upstream.
- **OpenTelemetry / OTLP export** — emit JSON lines and let a collector ingest
  them, or wrap a sink that forwards to OTel.

## Enabling

This is an **optional, non-default** feature. It is **std-only** — the async
appender uses a `std::thread` + `std::sync::mpsc` bounded channel, so there is
**no `tokio` dependency**.

```toml
[dependencies]
ferroly = { version = "*", features = ["log"] }
```

The feature implies [`codec`](codec.md) (field values are [`Value`](codec.md)).

## Quick start

```rust
use ferroly::log::{Level, Logger};

let log = Logger::json().with("service", "api");
log.info("request handled", &[
    ("method", "GET".into()),
    ("status", 200.into()),
]);

assert!(Level::Error > Level::Info); // levels are ordered
```

## API reference

### `Level`

Severity, ordered `Trace < Debug < Info < Warn < Error`:

```rust
pub enum Level { Trace = 0, Debug = 1, Info = 2, Warn = 3, Error = 4 }
```

Derives `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord`, so you can compare
levels directly (`Level::Warn >= Level::Info`). `Level::as_str(self)` returns the
lowercase name (`"info"`, …).

### Global level filter

```rust
pub fn set_max_level(level: Level)  // records below `level` are dropped everywhere
pub fn enabled(level: Level) -> bool // whether `level` currently passes the global filter
```

`set_max_level` sets a **process-wide** minimum; every logger honors it. The
default global minimum is `Level::Info`. Note this is separate from a logger's
own per-logger floor (see [`Logger::level`](#logger-builder)) — a record must
pass **both** to be emitted.

### `Format`

```rust
pub enum Format { Plain, Json }
```

- **`Plain`** — `ts LEVEL  message key=value …`. String field values render raw;
  everything else renders as compact JSON. Message newlines are replaced with
  spaces so a record can't break line-oriented output.
- **`Json`** — one JSON object per line, with `ts`, `level`, `msg`, then all
  fields. Typed values are preserved (numbers stay numbers, bools stay bools).

Derives `Debug, Clone, Copy, PartialEq, Eq`.

### `Logger`

A cheaply-cloneable structured logger. Clones share the underlying sink but each
carries its own accumulated context fields and level floor.

#### Constructors

| Constructor | Result |
|---|---|
| `Logger::new()` | Plain-text logger writing **synchronously to stderr**. |
| `Logger::json()` | JSON-lines logger writing synchronously to stderr. |
| `Logger::default()` | Same as `new()`. |

#### Builder methods (consume and return `self`)

| Method | Effect |
|---|---|
| `format(Format)` | Sets the output format. |
| `level(Level)` | Sets a **per-logger floor**: records below it are dropped even if the global filter would pass them. |
| `to_writer<W: Write + Send + 'static>(writer)` | Redirect output **synchronously** to any writer (file, buffer, socket …). |
| `async_to<W: Write + Send + 'static>(writer, capacity)` | Redirect to a **background writer thread** over a bounded channel of `capacity` records — the non-blocking appender. |
| `with(key: impl Into<String>, value: impl Into<Value>)` | Add a **context field** carried on every record from this logger. Chainable. |

`flush(&self)` (not a builder) blocks until the async appender has drained all queued records —
call it before process exit so buffered logs aren't lost. No-op for a synchronous logger.

#### Emitting records

```rust
pub fn log(&self, level: Level, msg: &str, fields: &[(&str, Value)])
pub fn trace(&self, msg: &str, fields: &[(&str, Value)])
pub fn debug(&self, msg: &str, fields: &[(&str, Value)])
pub fn info (&self, msg: &str, fields: &[(&str, Value)])
pub fn warn (&self, msg: &str, fields: &[(&str, Value)])
pub fn error(&self, msg: &str, fields: &[(&str, Value)])
```

`fields` is a slice of **typed** `(&str, Value)` pairs — construct values with
`.into()` (e.g. `("status", 200.into())`, `("ok", true.into())`,
`("path", "/x".into())`). The per-level methods forward to `log`.

#### Introspection

- `dropped(&self) -> u64` — the number of records dropped because the async
  buffer was full. Always `0` for a synchronous logger.

### Process-global logger

```rust
pub fn set_global(logger: Logger) // install/replace the process-global logger

pub fn trace(msg: &str, fields: &[(&str, Value)])
pub fn debug(msg: &str, fields: &[(&str, Value)])
pub fn info (msg: &str, fields: &[(&str, Value)])
pub fn warn (msg: &str, fields: &[(&str, Value)])
pub fn error(msg: &str, fields: &[(&str, Value)])
```

The free functions (`ferroly::log::info`, …) log via the global logger. If none
has been installed, they fall back to a default plain-text stderr logger.
`set_global` may be called again to reconfigure at runtime.

### Context providers (trace-ID propagation)

```rust
pub fn add_context_provider<F>(f: F)
where F: Fn() -> Vec<(String, Value)> + Send + Sync + 'static
```

Registers a function consulted on **every** log call that injects extra fields
into the record — see [Request-context propagation](#request-context--trace-id-propagation).

## In depth

### Two-layer level filtering

A record is emitted only if it clears **both** the global filter
(`set_max_level`) **and** the logger's own floor (`.level(...)`). The global
filter is a fast atomic check shared by all loggers; the per-logger floor lets a
noisy component be quieted independently.

```rust
use ferroly::log::{set_max_level, Level, Logger};

set_max_level(Level::Debug);              // global: Debug and up

let quiet = Logger::new().level(Level::Warn); // this logger: Warn and up
quiet.info("suppressed", &[]);            // dropped by the per-logger floor
quiet.error("kept", &[]);                 // emitted
```

### Plain output

```rust
use ferroly::log::Logger;

Logger::new().warn("cache miss", &[("key", "user:42".into()), ("n", 7.into())]);
// 2026-07-05T12:34:56.789Z WARN  cache miss key=user:42 n=7
```

The level is left-padded to five columns; string fields print raw; numeric/bool
fields print as JSON. Newlines in the message become spaces.

### JSON output

```rust
use ferroly::log::Logger;

Logger::json()
    .with("svc", "api")
    .info("hello", &[("status", 200.into()), ("ok", true.into())]);
// {"ts":"2026-07-05T…Z","level":"info","msg":"hello","svc":"api","status":200,"ok":true}
```

Field ordering is: `ts`, `level`, `msg`, then context fields (from
providers, then from `.with(...)`), then the per-call `fields`. Types are
preserved — `"status":200` is a JSON number, not `"200"`.

### Carried context fields with `with`

`.with(k, v)` attaches a field to a logger clone so every subsequent record
includes it — ideal for per-component or per-request loggers:

```rust
use ferroly::log::Logger;

let base = Logger::json().with("service", "billing");
let req_log = base.clone().with("request_id", "abc-123");

req_log.info("charge", &[("amount_cents", 4999.into())]);
// includes service=billing and request_id=abc-123 on every line
```

Because a `Logger` is cheap to clone and clones share the sink, deriving a
child logger with extra context is inexpensive.

### RFC 3339 timestamps

Every record carries a UTC RFC 3339 timestamp with millisecond precision
(`2026-07-05T12:34:56.789Z`), computed dependency-free from the system clock —
no `chrono`/`time` crate. It appears as the leading token in plain output and the
`ts` field in JSON.

### Non-blocking async appender

`async_to(writer, capacity)` moves all I/O to a background thread. The logging
call encodes the record and pushes it onto a bounded channel of `capacity`
slots; the background thread drains the channel and writes+flushes. The hot path
**never blocks on I/O**. When the buffer is full, the record is **dropped and
counted** rather than blocking the caller — check `dropped()` to observe shed
load (e.g. surface it as a metric).

```rust
use std::fs::File;
use ferroly::log::Logger;

# fn run() -> std::io::Result<()> {
let file = File::create("app.log")?;
let log = Logger::json().async_to(file, 8192); // up to 8192 buffered records

for i in 0..100_000 {
    log.info("tick", &[("i", i.into())]);
}

// Records shed under back-pressure are counted, never silently lost.
if log.dropped() > 0 {
    eprintln!("log appender dropped {} records under load", log.dropped());
}

// Before exit, block until the background thread has written everything queued.
log.flush();
# Ok(())
# }
```

`flush()` queues a marker behind all pending writes and blocks until the
background thread has processed it, so every record enqueued before the call has
reached the writer. **Call it before process exit** (or before you need the log
durable) — otherwise records still sitting in the channel when `main` returns are
lost, because the background thread is torn down with the process. `flush()` is a
no-op on a synchronous logger, so it is always safe to call.

Trade-off: size `capacity` to your burst tolerance, and prefer a synchronous sink
when every last record must reach disk even on an abrupt crash (`flush` only helps
on a clean shutdown path you control).

### Request-context / trace-ID propagation

`add_context_provider` registers a hook that runs on **every** log call and
returns fields to merge into the record. Providers should return an **empty vec**
when there is no active context (i.e. outside a request), so background/global
logs stay clean.

This is exactly how request correlation is wired: [turbo](turbo.md)'s
`trace_context()` registers a provider that reads a **task-local `trace_id`** for
the in-flight request. As a result, *every* log line emitted while handling that
request — from any logger, including the global free functions — automatically
carries the same `trace_id`, with no threading of a context object through your
call stack.

```rust
use ferroly::log::{add_context_provider, info, Logger, set_global};
use ferroly::codec::Value;

// A provider that pulls a request id from wherever your framework stashes it.
add_context_provider(|| {
    match current_request_id() {
        Some(id) => vec![("trace_id".to_string(), Value::Str(id))],
        None => Vec::new(), // no context -> no field
    }
});

set_global(Logger::json());
info("handling", &[]); // JSON line includes "trace_id":"…" when in a request

# fn current_request_id() -> Option<String> { None }
```

Multiple providers may be registered; their fields are concatenated in
registration order, ahead of the logger's own `.with(...)` fields.

### Wiring the global logger

Install the global logger once at startup, then use the free functions anywhere:

```rust
use ferroly::log::{self, set_global, set_max_level, Format, Level, Logger};

set_max_level(Level::Info);
set_global(Logger::json().format(Format::Json).with("service", "api"));

log::info("started", &[("port", 8080.into())]);
log::error("db down", &[("retry_in_ms", 500.into())]);
```

## Error handling

Logging never returns a `Result` — it is fire-and-forget. Write failures on the
synchronous sink are ignored; on the async sink a full buffer increments the drop
counter (surfaced via `dropped()`). There is no panic on I/O error, so a broken
sink cannot take down the hot path.

## Limitations

- **No rotation, sampling, or OTel export** — intentionally delegated to the sink
  or surrounding app.
- **Async appender may lose buffered records at process exit** unless you call
  `flush()` on a clean shutdown path, and it drops under sustained back-pressure
  (by design, counted via `dropped()`).
- `set_max_level` and `add_context_provider` are **process-global** — convenient,
  but shared state; set them up once at startup.
- Field values are limited to codec [`Value`](codec.md) shapes; arbitrary types
  must be converted to a `Value` first.
- Plain output escapes only newlines/carriage returns in the message (to protect
  line framing); it does not escape field values beyond JSON-encoding non-string
  values.

## See also

- [turbo](turbo.md) — its `trace_context()` registers a context provider so every
  request-scoped log line carries a `trace_id`.
- [codec](codec.md) — the [`Value`](codec.md) type used for structured fields.

---
**Related:** [turbo](turbo.md), [codec](codec.md).
