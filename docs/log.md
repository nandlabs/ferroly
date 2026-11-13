# ferroly::log

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `log` · **Module:** `ferroly::log`

## Overview

`log` is a dependency-free **structured logger** built for enterprise
services:

- typed structured fields and RFC 3339 timestamps on every record;
- plain-text *or* JSON-lines output, both **safe against record forging**;
- level filtering at three layers (global, per target, per logger);
- **redaction** of sensitive fields, on by default;
- **fan-out** to several sinks, each with its own format and minimum level;
- a non-blocking background appender with a **drop-or-block** overflow policy;
- a **rotating file** writer (size and daily rotation with retention);
- opt-in **sampling**, **caller capture** and a **panic hook**;
- a process-global logger, context providers, and trace correlation with
  [`obs`](obs.md).

Everything is built on [`ferroly::codec::Value`](codec.md) — field values keep
their type (a number stays a number in JSON output, not a quoted string) — with
no external logging framework and no `serde`.

Deliberately **out of scope**:

- **A bridge from the `log` / `tracing` facade crates.** Capturing records that
  third-party crates emit through those facades needs the facade crates as
  dependencies, which this toolkit does not take. Diagnostics from `rustls` or
  `tokio` are therefore not visible through `ferroly::log`.
- **syslog / journald / network sinks.** A sink is any `std::io::Write`; wrap a
  socket or write your own adapter.
- **Reading the system time zone.** Timestamps are UTC unless you configure a
  fixed offset.

## Enabling

This is an **optional, non-default** feature. It is **std-only** — the
non-blocking appender uses a `std::thread` + `std::sync::mpsc` bounded channel,
so there is **no `tokio` dependency**.

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

Derives `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash`.
`Level::as_str(self)` returns the lowercase name (`"info"`, …); `Display` prints
the same. `Level` implements `FromStr` (case-insensitive; `"warning"` is accepted
for `Warn`), so a level can come straight from configuration:

```rust
use ferroly::log::Level;
let level: Level = "WARN".parse().unwrap();
assert_eq!(level, Level::Warn);
```

### Level filters

```rust
pub fn set_max_level(level: Level)            // global minimum
pub fn max_level() -> Level                    // current global minimum (default Info)
pub fn enabled(level: Level) -> bool           // does `level` pass the global filter?

pub fn set_level_for(target: &str, level: Level) // per-target override
pub fn clear_level_for(target: &str) -> bool
pub fn clear_target_levels()
pub fn level_for(target: &str) -> Level        // level in force for a target
pub fn enabled_for(target: &str, level: Level) -> bool
```

A record is emitted when it clears, in order:

1. the logger's own floor (`Logger::level`, default `Trace`);
2. the **target override** if the logger has a [target](#targets-and-per-module-levels)
   and an override matches it — otherwise the **global level**;
3. the minimum level of at least one [sink](#sinks-and-fan-out).

### `Format`

```rust
pub enum Format { Plain, Json }
```

See [Output formats](#output-formats) for the exact layouts.

### `Logger`

A cheaply-cloneable structured logger. Clones share the underlying sinks but each
carries its own context fields, target and level floor.

#### Constructors

| Constructor | Result |
|---|---|
| `Logger::new()` | Plain-text logger writing **synchronously to stderr**. |
| `Logger::json()` | JSON-lines logger writing synchronously to stderr. |
| `Logger::default()` | Same as `new()`. |

#### Builder methods (consume and return `self`)

| Method | Effect |
|---|---|
| `format(Format)` | Format for every sink that has no format of its own. |
| `level(Level)` | **Per-logger floor**: records below it are dropped even if the other filters would pass them. |
| `to_writer<W: Write + Send + 'static>(writer)` | Replace all sinks with one **synchronous** sink on `writer`. |
| `async_to<W: Write + Send + 'static>(writer, capacity)` | Replace all sinks with one **non-blocking** sink (drop-on-full) on `writer`. |
| `to_sink(Sink)` | Replace all sinks with `sink`. |
| `add_sink(Sink)` | Add `sink` alongside the existing ones (fan-out). |
| `with(key, value)` | Add a **context field** carried on every record. |
| `with_target(target)` | Name the component (module path); printed as `target`, selects the per-target level. |
| `redact(Redactor)` | Replace the redaction policy (`Redactor::none()` turns it off). |
| `sample(max: u32, interval: Duration)` | Rate-limit repeated records. |
| `with_caller(bool)` | Add a `caller` (`file:line`) key to every record. |
| `time_precision(Precision)` | `Seconds`, `Millis` (default), `Micros` or `Nanos`. |
| `utc_offset(seconds_east: i32)` | Print timestamps at a fixed offset (`+05:30`) instead of `Z`. |

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
`("path", "/x".into())`). All of these are `#[track_caller]`.

#### Other methods

| Method | Description |
|---|---|
| `flush(&self)` | Flush every sink — see [Flushing](#flushing-and-the-buffering-contract). |
| `dropped(&self) -> u64` | Records dropped by non-blocking sinks (full buffer, or writer thread gone). |
| `suppressed(&self) -> u64` | Records withheld by `sample`. |
| `enabled(&self, level) -> bool` | Would a record at `level` be emitted right now? |
| `target(&self) -> Option<&str>` | The logger's target. |

### `Sink`, `Overflow`

```rust
Sink::writer(w)                 // synchronous
Sink::stderr() / Sink::stdout() // synchronous
Sink::non_blocking(w, capacity) // background thread, bounded queue

sink.format(Format)             // this sink's format (default: the logger's)
    .level(Level)               // this sink's minimum level (default Trace)
    .overflow(Overflow)         // non-blocking sinks: Drop (default) or Block
    .flush_level(Level)         // synchronous sinks: flush after records >= level (default Warn)

pub enum Overflow { Drop, Block }
```

### `RotatingFile`, `Rotation`

```rust
RotatingFile::open(path, Rotation) -> io::Result<RotatingFile>   // implements Write
file.rotate() -> io::Result<()>                                   // force a rotation
file.path() -> &Path

Rotation::new()            // 10 MiB, keep 5, no daily rotation
    .max_bytes(u64)        // 0 disables size-based rotation
    .keep(usize)           // rotated files to retain; 0 = truncate only
    .daily(bool)           // also rotate when the UTC day changes
```

### `Redactor`

```rust
Redactor::default() / Redactor::new()   // masks DEFAULT_REDACTED with "***"
Redactor::none()                         // masks nothing
Redactor::with_patterns(&["ssn"])        // exactly these patterns
redactor.pattern("ssn")                  // add one more
        .mask("[redacted]")              // change the replacement text
redactor.matches("user_password") -> bool
```

### `Sampler`

The rate limiter behind `Logger::sample`, public so the same policy can be reused:
`Sampler::new(max, interval)`, `check(key) -> Option<u64>`,
`check_at(key, Instant) -> Option<u64>`, `suppressed() -> u64`.

### Process-global logger

```rust
pub fn set_global(logger: Logger) // install/replace the process-global logger
pub fn logger() -> Logger         // a clone of it, for deriving children
pub fn flush()                    // flush it (best-effort, call at shutdown)
pub fn flush_guard() -> FlushGuard // flushes it when dropped

pub fn trace(msg: &str, fields: &[(&str, Value)])
pub fn debug(msg: &str, fields: &[(&str, Value)])
pub fn info (msg: &str, fields: &[(&str, Value)])
pub fn warn (msg: &str, fields: &[(&str, Value)])
pub fn error(msg: &str, fields: &[(&str, Value)])

pub fn install_panic_hook()
pub fn install_panic_hook_with(chain_previous: bool)
```

The free functions log via the global logger. If none has been installed, they
fall back to a default plain-text stderr logger. `set_global` may be called again
to reconfigure at runtime.

### Context providers

```rust
pub fn add_context_provider<F>(f: F)
where F: Fn() -> Vec<(String, Value)> + Send + Sync + 'static
pub fn clear_context_providers()

pub fn trace_context_fields() -> Vec<(String, Value)> // trace_id / span_id of the current obs span
pub fn add_trace_context_provider()                   // register it (idempotent)
```

## Output formats

### Plain

```
2026-07-05T12:34:56.789Z WARN  cache miss target=app::cache key=user:42 n=7 note="two words"
```

Layout: `timestamp`, the level padded to five columns, the message, then
`key=value` pairs separated by single spaces — `target` and `caller` first (when
present), then the fields.

**Escaping rules** (these make one record exactly one line, whatever the input):

- A **key or string value** is written bare when it is non-empty and contains no
  whitespace, no control character and none of the delimiter characters `=`,
  `"`, `\`. Otherwise it is written in double quotes with `\"`, `\\`, `\n`,
  `\r`, `\t` and `\uXXXX` (for every other control character, DEL, the C1
  controls, U+2028 and U+2029) escapes. An empty string is `""`.
- **Non-string values** print as compact JSON (`7`, `true`, `null`,
  `{"a":"x y"}`); JSON's own string escaping applies inside them.
- The **message** is free text: every kind of line break (`\n`, `\r`, VT, FF,
  NEL, U+2028, U+2029) becomes a space, other control characters become
  `\uXXXX`, tabs are kept. The message is *not* quoted, so text such as
  `user=admin` inside a message reads like a field — use the JSON format when
  records are parsed by machines, or keep untrusted text in fields.

### JSON

```json
{"ts":"2026-07-05T12:34:56.789Z","level":"info","msg":"hello","target":"app::api","svc":"api","status":200,"ok":true}
```

Key order: `ts`, `level`, `msg`, then `target` and `caller` (when present), then
the fields. Types are preserved — `"status":200` is a JSON number. Every control
character, DEL, the C1 controls and U+2028/U+2029 are `\u`-escaped, so a record
is always a single line.

### Field order, repeated keys and reserved keys

Fields come from three sources, in this order: **context providers**, the
logger's **`with` fields**, then the **per-call fields**. In both formats:

- **A repeated key is printed once**, with the *last* value (a per-call field
  overrides a `with` field, which overrides a provider field), at the position of
  its first occurrence.
- **A field cannot shadow the record's own keys.** A field named `ts`, `level`
  or `msg` — or `target` / `caller` when the record prints those — is moved into
  a nested **`fields`** object (JSON) or printed with a **`fields.`** prefix
  (plain). The top-level `level` is always the real level:

  ```json
  {"ts":"…","level":"error","msg":"real","status":500,"fields":{"level":"debug","msg":"forged"}}
  ```

  ```
  2026-07-05T12:34:56.789Z ERROR real status=500 fields.level=debug fields.msg=forged
  ```

- A user field that is itself called `fields` stays an ordinary top-level field
  unless the nested object is needed for the record, in which case it moves
  inside it (`fields.fields`), so `fields` is never duplicated either.

JSON output therefore never contains duplicate keys.

### Timestamps

RFC 3339, UTC, millisecond precision by default (`2026-07-05T12:34:56.789Z`),
computed dependency-free from the system clock. `time_precision` selects
seconds, milliseconds, microseconds or nanoseconds; `utc_offset(seconds)` prints
wall-clock fields at a fixed offset with a `+HH:MM` / `-HH:MM` suffix (rounded
toward zero to whole minutes, limited to ±23:59).

## In depth

### Targets and per-module levels

Give a logger a target — conventionally the module path — and tune that part of
the code base independently:

```rust
use ferroly::log::{set_level_for, set_max_level, Level, Logger};

set_max_level(Level::Info);
set_level_for("app::http", Level::Warn);     // quieter than the rest
set_level_for("app::billing", Level::Trace); // louder than the rest

let http = Logger::new().with_target("app::http::client");
http.info("suppressed", &[]);   // app::http is Warn and up
http.warn("kept", &[]);

let billing = Logger::new().with_target("app::billing");
billing.debug("kept", &[]);     // the override replaces the global level
```

An override **replaces** the global level for matching records. Matching is by
prefix at segment boundaries (`::`, `.` or `/`): `app::http` covers
`app::http` and `app::http::client`, not `app::httpx`. The longest matching
prefix wins. Records from a logger without a target use the global level.

To derive targeted loggers from the global one:
`ferroly::log::logger().with_target(module_path!())` (the child shares the
global logger's sinks as they are at that moment).

### Sinks and fan-out

A logger writes each record to **every** sink whose minimum level admits it,
rendering each format at most once:

```rust
use ferroly::log::{Format, Level, Logger, Overflow, RotatingFile, Rotation, Sink};

# fn main() -> std::io::Result<()> {
let file = RotatingFile::open("app.log", Rotation::new().keep(7))?;
let audit = std::fs::File::create("audit.log")?;

let log = Logger::new()                                   // plain text on stderr …
    .add_sink(Sink::non_blocking(file, 8192)              // … JSON to a rotating file …
        .format(Format::Json))
    .add_sink(Sink::non_blocking(audit, 1024)             // … and warnings to an audit
        .level(Level::Warn)                               //     log that never drops
        .overflow(Overflow::Block));
log.info("started", &[]);
log.flush();
# Ok(())
# }
```

`to_writer`, `async_to` and `to_sink` *replace* the sink list; `add_sink`
appends to it.

### Flushing and the buffering contract

- A **synchronous sink** hands each record (with its newline) to the writer in
  one `write_all` call before the logging call returns. The writer is **flushed
  after every record at `Warn` or above** (configurable with
  `Sink::flush_level`) and by `Logger::flush()`. If the writer buffers — a
  `BufWriter<File>` — records below that level sit in its buffer until the next
  flush.
- A **non-blocking sink** queues records for a background thread, which writes
  them and flushes the writer whenever the queue runs empty.
  `Logger::flush()` queues a marker behind all pending records and blocks until
  the thread has written and flushed up to it.
- `Logger::flush()` flushes **all** sinks of the logger, synchronous ones
  included.
- `ferroly::log::flush()` flushes the global logger. Records still buffered when
  `main` returns are otherwise lost — a `static` logger is never dropped — so
  flush on the shutdown path:

```rust
use ferroly::log::{self, Logger};

fn main() {
    log::set_global(Logger::json());
    log::install_panic_hook();            // panics are logged, then flushed
    let _flush = log::flush_guard();      // flushes on every return path
    // … run …
}
```

### Non-blocking appender and overflow policy

`async_to(writer, capacity)` (or `Sink::non_blocking`) moves all I/O to a
background thread behind a bounded queue of `capacity` records.

| `Overflow` | When the queue is full | Use for |
|---|---|---|
| `Drop` (default) | the record is discarded and counted in `dropped()`; the logging call never waits | request paths, where latency matters more than completeness |
| `Block` | the logging call waits for space; nothing is lost | audit logs |

```rust
use ferroly::log::{Logger, Overflow, Sink};

# fn run(file: std::fs::File) {
let log = Logger::json().to_sink(Sink::non_blocking(file, 8192).overflow(Overflow::Block));
log.info("tick", &[]);
if log.dropped() > 0 {
    eprintln!("log appender dropped {} records", log.dropped());
}
log.flush(); // before exit
# }
```

`dropped()` also counts records that could not be queued because the writer
thread has gone away. Expose it as a metric to observe shed load.

### Rotating files

`RotatingFile` is a `Write` that rotates itself:

- **Size:** before a write that would take the file past `max_bytes`, the file
  is rotated. A record is never split across files; a single record larger than
  the limit is written whole into a file of its own.
- **Daily:** with `daily(true)`, the file is also rotated when the UTC calendar
  day changes (checked on each write; a file whose modification time is from an
  earlier day is rotated on the first write after start-up).
- **Retention:** `app.log` → `app.log.1` → `app.log.2` … — higher numbers are
  older — and the file beyond `keep` is deleted. `keep(0)` keeps no history:
  rotation just truncates.

Writes are unbuffered (one system call per record), so nothing is lost if the
process dies; put the file behind a non-blocking sink to keep that I/O off the
calling thread. Rotation is per-process: do not point two processes at the same
file. There is no compression of rotated files.

### Redaction

Every logger redacts by default. A field whose **name** matches a pattern has its
value replaced with `***` — in context-provider fields, `with` fields and
per-call fields, in both formats, and inside nested objects and arrays.

Default patterns (`DEFAULT_REDACTED`): `password`, `passwd`, `pwd`, `secret`,
`token`, `authorization`, `api_key`, `apikey`, `cookie`, `credential`,
`credentials`, `private_key`.

A pattern matches when its words appear as **consecutive whole words** of the
field name, ignoring case. Names are split into words at `_`, `-`, `.`, any other
non-alphanumeric character, and camelCase boundaries:

| Field name | Redacted? |
|---|---|
| `password`, `user_password`, `db.password` | yes (`password`) |
| `access_token`, `X-Auth-Token`, `authToken` | yes (`token`) |
| `api_key`, `x-api-key`, `apiKey`, `APIKEY` | yes (`api_key` / `apikey`) |
| `Authorization`, `Set-Cookie` | yes |
| `max_tokens`, `tokens_used`, `secretary`, `key` | no — a different word |

```rust
use ferroly::log::{Logger, Redactor};

let log = Logger::json().redact(Redactor::default().pattern("ssn").mask("[redacted]"));
log.info("signup", &[("email", "a@b.c".into()), ("ssn", "123-45-6789".into())]);
// {"ts":"…","level":"info","msg":"signup","email":"a@b.c","ssn":"[redacted]"}

let raw = Logger::json().redact(Redactor::none()); // no redaction at all
```

Redaction is by field **name** only: it does not scan message text or the
contents of string values. Do not interpolate secrets into messages.

### Sampling

`sample(max, interval)` caps repeats: at most `max` records per `interval` for
each distinct **(level, target, message)** — field values are not part of the
key. Excess records are withheld and counted (`suppressed()`); the first record
of the next window carries a `suppressed` field with the number withheld. Clones
of a sampled logger share one budget.

```rust
use std::time::Duration;
use ferroly::log::Logger;

let log = Logger::new().sample(5, Duration::from_secs(1));
for i in 0..10_000 {
    log.warn("upstream timeout", &[("attempt", i.into())]); // 5 per second get through
}
```

Sampling applies to every level, `Error` included; leave it off for loggers whose
records must all be kept.

### Caller capture

`with_caller(true)` adds `caller` = `file:line` of the logging call to each
record, using `#[track_caller]` (no backtrace, no macro, negligible cost). It
works through the free functions too. The location is that of the direct caller
of `info` / `log` / …; a helper that wraps the logger should itself be
`#[track_caller]` to pass its caller through.

### Panic hook

`install_panic_hook()` logs every panic through the global logger at `Error` —
message `panic`, fields `panic.message`, `panic.location`
(`file:line:column`), `panic.thread` — flushes the logger, and then runs the
previously installed hook (by default the standard stderr report).
`install_panic_hook_with(false)` does not chain, so the panic is reported through
the logger only.

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

### Request-context / trace-ID propagation

`add_context_provider` registers a hook that runs on **every** log call and
returns fields to merge into the record. Providers should return an **empty vec**
when there is no active context, so background logs stay clean.

[turbo](turbo.md)'s `trace_context()` registers a provider for the in-flight
request, so every line logged while handling it carries a `request_id` (an
inbound `X-Request-Id` if it passes a strict check, otherwise generated) and a
`trace_id`. With the `obs` feature the middleware also extracts W3C trace
context, runs a server span for the request, and `trace_id` is that span's W3C
trace id.

```rust
use ferroly::log::{add_context_provider, info, Logger, set_global};
use ferroly::codec::Value;

add_context_provider(|| {
    match current_request_id() {
        Some(id) => vec![("request_id".to_string(), Value::Str(id))],
        None => Vec::new(), // no context -> no field
    }
});

set_global(Logger::json());
info("handling", &[]); // JSON line includes "request_id":"…" when in a request

# fn current_request_id() -> Option<String> { None }
```

Provider rules:

- Providers are called **without any logger lock held** (the list is snapshotted
  first), so a provider may log or register another provider.
- A record logged **from inside a provider** is emitted without provider fields
  — the providers are not consulted again — which bounds the recursion.
- Provider fields go through the same escaping, redaction and reserved-key rules
  as any other field.
- Multiple providers contribute in registration order, ahead of the logger's own
  `with` fields.

### Linking logs to traces (`obs`)

`add_trace_context_provider()` registers `trace_context_fields`, which returns
the `trace_id` and `span_id` of the current [`obs`](obs.md) span. Every record
logged inside a span then carries both ids:

```rust
use ferroly::log::{self, Logger};

log::set_global(Logger::json());
log::add_trace_context_provider();

// Inside an obs span (with the `obs` feature enabled):
//   {"ts":"…","level":"info","msg":"charged",
//    "trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7"}
```

Both functions exist whether or not the `obs` feature is enabled; without it
`trace_context_fields` always returns an empty vec, so the call compiles and is
simply inert. The ids follow the current span across `.await` points when the
span was attached with the task-local scope described in [obs](obs.md).

## Error handling

Logging never returns a `Result` and never panics on I/O errors. Write failures
are ignored; a full non-blocking queue either drops and counts or blocks, per its
`Overflow` policy. A poisoned sink lock (a writer that panicked earlier) keeps
being used rather than silencing the logger. A sink writer that itself logs or
flushes does not deadlock: the nested record is discarded.

## Limitations

- **No bridge from the `log` / `tracing` crates** (see Overview).
- **No syslog/journald/network sinks, no compression of rotated files.**
- **Redaction is by field name**, not by scanning values or message text.
- **Plain-format messages are free text** — `key=value` inside a message is
  indistinguishable from a field. Use JSON for machine parsing.
- **Timestamps cannot follow the system time zone**; only a fixed offset.
- `set_max_level`, `set_level_for`, `add_context_provider` and the global logger
  are **process-global** — set them up once at startup.
- `Overflow::Drop` loses records under sustained back-pressure (by design,
  counted); buffered records are lost at process exit unless you flush.
- Field values are limited to codec [`Value`](codec.md) shapes.

## See also

- [turbo](turbo.md) — its `trace_context()` registers a context provider so every
  request-scoped log line carries a `trace_id`.
- [obs](obs.md) — spans; `add_trace_context_provider` links log lines to them.
- [codec](codec.md) — the [`Value`](codec.md) type used for structured fields.

---
**Related:** [turbo](turbo.md), [obs](obs.md), [codec](codec.md).
