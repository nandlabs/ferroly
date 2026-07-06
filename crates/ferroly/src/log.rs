//! A dependency-free structured logger for enterprise services.
//!
//! Features:
//! - Levels (`Trace..Error`) with a global filter **and** an optional per-logger
//!   floor.
//! - **RFC 3339 UTC timestamps** on every record.
//! - **Typed** structured fields (via [`ferroly::codec::Value`]) plus carried
//!   context fields ([`with`](Logger::with)).
//! - Plain (`ts LEVEL msg k=v`) or **JSON-lines** output (numbers stay numbers).
//! - A synchronous sink by default, or an **opt-in non-blocking appender**
//!   ([`async_to`](Logger::async_to)) that offloads I/O to a background thread and
//!   sheds load (counting drops) instead of stalling the hot path.
//! - A process-global logger + free functions for package-level logging.
//!
//! Rotation, sampling, and OpenTelemetry export are deliberately left to the
//! sink / the surrounding app — the module stays small and vendor-neutral.
//!
//! ```
//! use ferroly::log::{Level, Logger};
//!
//! let log = Logger::json().with("service", "api");
//! log.info("request handled", &[("method", "GET".into()), ("status", 200.into())]);
//! assert!(Level::Error > Level::Info);
//! ```

#![deny(missing_docs)]

use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ferroly::codec::{json, Value};

/// Severity level, ordered `Trace < Debug < Info < Warn < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Very fine-grained tracing.
    Trace = 0,
    /// Debugging detail.
    Debug = 1,
    /// Normal operational events.
    Info = 2,
    /// Recoverable problems.
    Warn = 3,
    /// Errors.
    Error = 4,
}

impl Level {
    /// The lowercase name (`"info"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Sets the global minimum level; records below it are dropped by every logger.
pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

type Provider = Arc<dyn Fn() -> Vec<(String, Value)> + Send + Sync>;
static PROVIDERS: RwLock<Vec<Provider>> = RwLock::new(Vec::new());

/// Registers a function that supplies extra fields on **every** record — e.g. a
/// request-scoped `trace_id`. Providers are consulted on each log call and
/// should return an empty vec when there is no context (outside a request). This
/// is how request-context / trace-ID correlation is wired (see
/// [`ferroly::turbo::Router::trace_context`](../turbo/struct.Router.html)).
pub fn add_context_provider<F>(f: F)
where
    F: Fn() -> Vec<(String, Value)> + Send + Sync + 'static,
{
    PROVIDERS.write().unwrap().push(Arc::new(f));
}

fn context_fields() -> Vec<(String, Value)> {
    let providers = PROVIDERS.read().unwrap();
    if providers.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for provider in providers.iter() {
        out.extend(provider());
    }
    out
}

/// Whether `level` passes the global filter.
pub fn enabled(level: Level) -> bool {
    (level as u8) >= MAX_LEVEL.load(Ordering::Relaxed)
}

/// The output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `2026-07-05T…Z INFO  message key=value …`
    Plain,
    /// One JSON object per line (typed field values preserved).
    Json,
}

/// A command sent to the async appender thread.
enum Cmd {
    /// Write a formatted record.
    Write(Vec<u8>),
    /// Flush the writer, then acknowledge on the given channel.
    Flush(std::sync::mpsc::SyncSender<()>),
}

enum Sink {
    Sync(Mutex<Box<dyn Write + Send>>),
    Async {
        tx: std::sync::mpsc::SyncSender<Cmd>,
        dropped: AtomicU64,
    },
}

/// A cheaply-cloneable structured logger. Clones share the sink and carry their
/// own accumulated context fields and level floor.
#[derive(Clone)]
pub struct Logger {
    format: Format,
    min: Level,
    fields: Vec<(String, Value)>,
    sink: Arc<Sink>,
}

impl Default for Logger {
    fn default() -> Self {
        Self {
            format: Format::Plain,
            min: Level::Trace,
            fields: Vec::new(),
            sink: Arc::new(Sink::Sync(Mutex::new(Box::new(std::io::stderr())))),
        }
    }
}

impl Logger {
    /// A plain-text logger writing synchronously to stderr.
    pub fn new() -> Self {
        Self::default()
    }

    /// A JSON-lines logger writing synchronously to stderr.
    pub fn json() -> Self {
        Self {
            format: Format::Json,
            ..Self::default()
        }
    }

    /// Sets the output format.
    pub fn format(mut self, format: Format) -> Self {
        self.format = format;
        self
    }

    /// Sets a per-logger level floor (in addition to the global filter): records
    /// below it are dropped even if the global level would pass them.
    pub fn level(mut self, level: Level) -> Self {
        self.min = level;
        self
    }

    /// Directs output synchronously to `writer` (a file, a buffer, …).
    pub fn to_writer<W: Write + Send + 'static>(mut self, writer: W) -> Self {
        self.sink = Arc::new(Sink::Sync(Mutex::new(Box::new(writer))));
        self
    }

    /// Directs output to a **background writer thread** over a bounded channel of
    /// `capacity` records. The logging call never blocks on I/O; when the buffer
    /// is full records are dropped and counted (see [`dropped`](Self::dropped)).
    pub fn async_to<W: Write + Send + 'static>(mut self, writer: W, capacity: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Cmd>(capacity);
        std::thread::spawn(move || {
            let mut w = writer;
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Cmd::Write(bytes) => {
                        if w.write_all(&bytes).is_err() {
                            break;
                        }
                        let _ = w.flush();
                    }
                    Cmd::Flush(ack) => {
                        let _ = w.flush();
                        let _ = ack.send(());
                    }
                }
            }
        });
        self.sink = Arc::new(Sink::Async {
            tx,
            dropped: AtomicU64::new(0),
        });
        self
    }

    /// Flushes buffered output. For an [`async_to`](Self::async_to) logger this
    /// blocks until the background thread has written everything queued so far —
    /// call it before process exit so buffered records aren't lost. No-op for a
    /// synchronous logger.
    pub fn flush(&self) {
        if let Sink::Async { tx, .. } = &*self.sink {
            let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
            // Blocking send: queue the marker behind all pending writes, then
            // wait for the thread to process it.
            if tx.send(Cmd::Flush(ack_tx)).is_ok() {
                let _ = ack_rx.recv();
            }
        }
    }

    /// The number of records dropped due to a full async buffer (always 0 for a
    /// synchronous logger).
    pub fn dropped(&self) -> u64 {
        match &*self.sink {
            Sink::Async { dropped, .. } => dropped.load(Ordering::Relaxed),
            Sink::Sync(_) => 0,
        }
    }

    /// Adds a context field carried on every record from this logger.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }

    /// Emits a record at `level` with `msg` and extra typed `fields`.
    pub fn log(&self, level: Level, msg: &str, fields: &[(&str, Value)]) {
        if (level as u8) < (self.min as u8) || !enabled(level) {
            return;
        }
        let ts = rfc3339(SystemTime::now());
        let provided = context_fields();
        let line = match self.format {
            Format::Plain => self.format_plain(&ts, level, msg, &provided, fields),
            Format::Json => self.format_json(&ts, level, msg, &provided, fields),
        };
        match &*self.sink {
            Sink::Sync(m) => {
                if let Ok(mut w) = m.lock() {
                    let _ = w.write_all(line.as_bytes());
                    let _ = w.write_all(b"\n");
                }
            }
            Sink::Async { tx, dropped } => {
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                if tx.try_send(Cmd::Write(bytes)).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Logs at [`Level::Trace`].
    pub fn trace(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Trace, msg, fields);
    }
    /// Logs at [`Level::Debug`].
    pub fn debug(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Debug, msg, fields);
    }
    /// Logs at [`Level::Info`].
    pub fn info(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Info, msg, fields);
    }
    /// Logs at [`Level::Warn`].
    pub fn warn(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Warn, msg, fields);
    }
    /// Logs at [`Level::Error`].
    pub fn error(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Error, msg, fields);
    }

    fn format_plain(
        &self,
        ts: &str,
        level: Level,
        msg: &str,
        provided: &[(String, Value)],
        fields: &[(&str, Value)],
    ) -> String {
        let mut s = format!(
            "{ts} {:<5} {}",
            level.as_str().to_uppercase(),
            sanitize(msg)
        );
        for (k, v) in provided.iter().chain(self.fields.iter()) {
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(&value_plain(v));
        }
        for (k, v) in fields {
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(&value_plain(v));
        }
        s
    }

    fn format_json(
        &self,
        ts: &str,
        level: Level,
        msg: &str,
        provided: &[(String, Value)],
        fields: &[(&str, Value)],
    ) -> String {
        let mut obj: Vec<(String, Value)> =
            Vec::with_capacity(3 + provided.len() + self.fields.len() + fields.len());
        obj.push(("ts".into(), Value::Str(ts.to_string())));
        obj.push(("level".into(), Value::Str(level.as_str().to_string())));
        obj.push(("msg".into(), Value::Str(msg.to_string())));
        for (k, v) in provided.iter().chain(self.fields.iter()) {
            obj.push((k.clone(), v.clone()));
        }
        for (k, v) in fields {
            obj.push((k.to_string(), v.clone()));
        }
        json::to_string(&Value::Object(obj))
    }
}

/// Replaces newlines so a message can't break line-oriented (plain) output.
fn sanitize(msg: &str) -> String {
    if msg.contains(['\n', '\r']) {
        msg.replace(['\n', '\r'], " ")
    } else {
        msg.to_string()
    }
}

/// A field's plain-text rendering: strings raw, everything else as JSON.
fn value_plain(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        _ => json::to_string(v),
    }
}

/// Formats a `SystemTime` as RFC 3339 UTC with millisecond precision, dep-free.
fn rfc3339(t: SystemTime) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

/// Days-since-1970 → `(year, month, day)` (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { y + 1 } else { y }, month, day)
}

// ---- process-global logger -----------------------------------------------

static GLOBAL: RwLock<Option<Arc<Logger>>> = RwLock::new(None);

/// Installs the process-global logger used by the free [`info`] / [`error`] /
/// etc. functions. May be called again to reconfigure.
pub fn set_global(logger: Logger) {
    *GLOBAL.write().unwrap() = Some(Arc::new(logger));
}

/// The process-global logger as an `Arc` — cloning it per free-function call is
/// cheap (a refcount bump), avoiding a deep copy of its context fields.
fn global() -> Arc<Logger> {
    GLOBAL
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| Arc::new(Logger::default()))
}

/// Logs via the global logger at [`Level::Trace`].
pub fn trace(msg: &str, fields: &[(&str, Value)]) {
    global().trace(msg, fields);
}
/// Logs via the global logger at [`Level::Debug`].
pub fn debug(msg: &str, fields: &[(&str, Value)]) {
    global().debug(msg, fields);
}
/// Logs via the global logger at [`Level::Info`].
pub fn info(msg: &str, fields: &[(&str, Value)]) {
    global().info(msg, fields);
}
/// Logs via the global logger at [`Level::Warn`].
pub fn warn(msg: &str, fields: &[(&str, Value)]) {
    global().warn(msg, fields);
}
/// Logs via the global logger at [`Level::Error`].
pub fn error(msg: &str, fields: &[(&str, Value)]) {
    global().error(msg, fields);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn civil_date_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(18_993), (2022, 1, 1));
    }

    // Single serial test: `set_max_level` is global state.
    #[test]
    fn timestamps_typed_fields_levels_and_async() {
        set_max_level(Level::Trace);

        // JSON: typed values (number stays a number), timestamp + level present.
        let buf = Buf::default();
        Logger::json()
            .to_writer(buf.clone())
            .with("svc", "api")
            .info("hello", &[("status", 200.into()), ("ok", true.into())]);
        let out = buf.text();
        assert!(out.contains(r#""status":200"#), "out={out:?}"); // not "200"
        assert!(out.contains(r#""ok":true"#));
        assert!(out.contains(r#""svc":"api""#));
        assert!(out.contains(r#""level":"info""#));
        assert!(out.contains(r#""ts":"2"#)); // RFC3339 year 2xxx

        // Plain: strings raw, numbers plain, newline-safe message.
        let buf = Buf::default();
        Logger::new()
            .to_writer(buf.clone())
            .warn("multi\nline", &[("n", 7.into())]);
        let line = buf.text();
        assert!(line.contains("WARN  multi line n=7"), "line={line:?}");

        // Per-logger level floor drops below-floor records.
        let buf = Buf::default();
        let log = Logger::new().to_writer(buf.clone()).level(Level::Warn);
        log.info("dropped", &[]);
        log.error("kept", &[]);
        assert!(!buf.text().contains("dropped"));
        assert!(buf.text().contains("kept"));

        // Async appender delivers (drain by joining via a short sleep-free flush).
        let buf = Buf::default();
        let log = Logger::new().to_writer(buf.clone());
        // (async path exercised for drop-counting semantics)
        let full = Logger::new().async_to(SinkThatBlocks, 1);
        full.info("a", &[]);
        full.info("b", &[]);
        full.info("c", &[]);
        assert!(full.dropped() >= 1);
        log.info("sync-ok", &[]);
        assert!(buf.text().contains("sync-ok"));

        // Async appender to a real (non-blocking) writer: `flush` blocks until
        // the background thread has drained the queued records.
        let abuf = Buf::default();
        let alog = Logger::new().async_to(abuf.clone(), 16);
        alog.info("async-one", &[]);
        alog.info("async-two", &[]);
        alog.flush();
        let text = abuf.text();
        assert!(
            text.contains("async-one") && text.contains("async-two"),
            "text={text:?}"
        );
    }

    /// A writer that never returns, so the async buffer fills and drops occur.
    struct SinkThatBlocks;
    impl Write for SinkThatBlocks {
        fn write(&mut self, _b: &[u8]) -> std::io::Result<usize> {
            loop {
                std::thread::park();
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
