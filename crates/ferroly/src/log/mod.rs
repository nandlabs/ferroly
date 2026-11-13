//! A dependency-free structured logger for enterprise services.
//!
//! Features:
//! - Levels (`Trace..Error`) with a global filter, **per-target overrides**
//!   ([`set_level_for`]) and an optional per-logger floor.
//! - **RFC 3339 timestamps** on every record (UTC milliseconds by default;
//!   precision and a fixed offset are configurable).
//! - **Typed** structured fields (via [`ferroly::codec::Value`]) plus carried
//!   context fields ([`with`](Logger::with)) and process-wide
//!   [context providers](add_context_provider).
//! - Plain (`ts LEVEL msg k=v`) or **JSON-lines** output, both safe against
//!   record forging: control characters and delimiters in keys and values are
//!   escaped or quoted, and a field can never duplicate a record's own keys.
//! - **Redaction** of sensitive field names (`password`, `token`, … — see
//!   [`Redactor`]), on by default.
//! - **Fan-out** to several [`Sink`]s, each with its own format and minimum
//!   level; a synchronous sink by default, or a **non-blocking appender**
//!   ([`async_to`](Logger::async_to), [`Sink::non_blocking`]) that either sheds
//!   load (counting drops) or blocks, per its [`Overflow`] policy.
//! - A [`RotatingFile`] writer (size and daily rotation with retention).
//! - Opt-in [sampling](Logger::sample), [caller capture](Logger::with_caller)
//!   and a [panic hook](install_panic_hook).
//! - A process-global logger + free functions for package-level logging.
//!
//! There is no bridge from the `log` or `tracing` facade crates: wiring one
//! would add those crates as dependencies, which this toolkit does not take.
//!
//! ```
//! use ferroly::log::{Level, Logger};
//!
//! let log = Logger::json().with("service", "api");
//! log.info("request handled", &[("method", "GET".into()), ("status", 200.into())]);
//! assert!(Level::Error > Level::Info);
//! ```

#![deny(missing_docs)]

use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::panic::Location;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use ferroly::codec::Value;

mod format;
mod redact;
mod rotate;
mod sample;
mod sink;

pub use format::Precision;
pub use redact::{Redactor, DEFAULT_MASK, DEFAULT_REDACTED};
pub use rotate::{RotatingFile, Rotation};
pub use sample::Sampler;
pub use sink::{Overflow, Sink};

fn rlock<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn wlock<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

// ---- levels ------------------------------------------------------------------

/// Severity level, ordered `Trace < Debug < Info < Warn < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// The uppercase name used by the plain format (`"INFO"`, …).
    fn as_upper_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn from_u8(v: u8) -> Level {
        match v {
            0 => Level::Trace,
            1 => Level::Debug,
            2 => Level::Info,
            3 => Level::Warn,
            _ => Level::Error,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a level name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLevelError(String);

impl fmt::Display for ParseLevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown log level {:?}", self.0)
    }
}

impl std::error::Error for ParseLevelError {}

impl FromStr for Level {
    type Err = ParseLevelError;

    /// Parses a level name, ignoring case (`"warn"` and `"warning"` are both
    /// accepted) — convenient for reading a level from configuration.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim();
        let table = [
            ("trace", Level::Trace),
            ("debug", Level::Debug),
            ("info", Level::Info),
            ("warn", Level::Warn),
            ("warning", Level::Warn),
            ("error", Level::Error),
        ];
        table
            .iter()
            .find(|(n, _)| name.eq_ignore_ascii_case(n))
            .map(|(_, level)| *level)
            .ok_or_else(|| ParseLevelError(s.to_string()))
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Sets the global minimum level; records below it are dropped by every logger
/// (unless a [target override](set_level_for) says otherwise).
pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The current global minimum level (default [`Level::Info`]).
pub fn max_level() -> Level {
    Level::from_u8(MAX_LEVEL.load(Ordering::Relaxed))
}

/// Whether `level` passes the global filter.
pub fn enabled(level: Level) -> bool {
    (level as u8) >= MAX_LEVEL.load(Ordering::Relaxed)
}

// ---- per-target levels ---------------------------------------------------------

/// `(target prefix, level)`, longest prefix first.
static TARGET_LEVELS: RwLock<Vec<(String, Level)>> = RwLock::new(Vec::new());
/// Mirrors `TARGET_LEVELS.len()` so the common "no overrides" path takes no lock.
static TARGET_COUNT: AtomicUsize = AtomicUsize::new(0);

/// True when `target` is `prefix` itself or lies below it: the prefix must end
/// at a segment boundary (`::`, `.` or `/`), so `app::db` covers
/// `app::db::pool` but not `app::dbx`.
fn target_matches(prefix: &str, target: &str) -> bool {
    match target.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with("::") || rest.starts_with('.') || rest.starts_with('/'),
        None => false,
    }
}

/// Overrides the minimum level for records whose logger has a
/// [target](Logger::with_target) equal to, or nested below, `target`.
///
/// The override **replaces** the global level for those records, so it can make
/// one module quieter (`set_level_for("ferroly::http", Level::Warn)`) or more
/// verbose (`set_level_for("app::billing", Level::Trace)`) than the rest. When
/// several overrides match, the longest prefix wins. Nesting follows `::`, `.`
/// and `/` segment boundaries. An empty `target` is ignored.
///
/// ```
/// use ferroly::log::{enabled_for, set_level_for, Level};
///
/// set_level_for("docs::example::net", Level::Error);
/// assert!(!enabled_for("docs::example::net::client", Level::Warn));
/// assert!(enabled_for("docs::example::net::client", Level::Error));
/// ```
pub fn set_level_for(target: &str, level: Level) {
    if target.is_empty() {
        return;
    }
    let mut list = wlock(&TARGET_LEVELS);
    match list.iter_mut().find(|(t, _)| t == target) {
        Some(entry) => entry.1 = level,
        None => list.push((target.to_string(), level)),
    }
    list.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
    TARGET_COUNT.store(list.len(), Ordering::Relaxed);
}

/// Removes the override set for exactly `target`; `true` when there was one.
pub fn clear_level_for(target: &str) -> bool {
    let mut list = wlock(&TARGET_LEVELS);
    let before = list.len();
    list.retain(|(t, _)| t != target);
    TARGET_COUNT.store(list.len(), Ordering::Relaxed);
    list.len() != before
}

/// Removes every per-target override.
pub fn clear_target_levels() {
    let mut list = wlock(&TARGET_LEVELS);
    list.clear();
    TARGET_COUNT.store(0, Ordering::Relaxed);
}

/// The minimum level in force for `target`: the longest matching
/// [override](set_level_for), or the global level when none matches.
pub fn level_for(target: &str) -> Level {
    if TARGET_COUNT.load(Ordering::Relaxed) > 0 {
        let list = rlock(&TARGET_LEVELS);
        if let Some((_, level)) = list.iter().find(|(p, _)| target_matches(p, target)) {
            return *level;
        }
    }
    max_level()
}

/// Whether a record at `level` from a logger with `target` passes the level
/// filters (the target override if one matches, otherwise the global level).
pub fn enabled_for(target: &str, level: Level) -> bool {
    level >= level_for(target)
}

// ---- context providers -----------------------------------------------------------

type Provider = Arc<dyn Fn() -> Vec<(String, Value)> + Send + Sync>;

static PROVIDERS: RwLock<Vec<Provider>> = RwLock::new(Vec::new());
/// Mirrors `PROVIDERS.len()` so a process without providers takes no lock.
static PROVIDER_COUNT: AtomicUsize = AtomicUsize::new(0);
static TRACE_PROVIDER: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set while this thread is running context providers.
    static IN_PROVIDERS: Cell<bool> = const { Cell::new(false) };
}

/// Registers a function that supplies extra fields on **every** record — e.g. a
/// request-scoped `trace_id`. Providers are consulted on each log call and
/// should return an empty vec when there is no context (outside a request). This
/// is how request-context / trace-ID correlation is wired (see
/// [`ferroly::turbo::Router::trace_context`](../turbo/struct.Router.html) and
/// [`add_trace_context_provider`]).
///
/// Providers run with no logger lock held, so a provider may register another
/// provider or log. A record logged **from inside a provider** is emitted
/// without provider fields (the providers are not consulted again), which
/// bounds the recursion.
pub fn add_context_provider<F>(f: F)
where
    F: Fn() -> Vec<(String, Value)> + Send + Sync + 'static,
{
    let mut list = wlock(&PROVIDERS);
    list.push(Arc::new(f));
    PROVIDER_COUNT.store(list.len(), Ordering::Relaxed);
}

/// Removes every registered context provider (including the one installed by
/// [`add_trace_context_provider`]). Mainly useful in tests.
pub fn clear_context_providers() {
    let mut list = wlock(&PROVIDERS);
    list.clear();
    PROVIDER_COUNT.store(0, Ordering::Relaxed);
    TRACE_PROVIDER.store(false, Ordering::Relaxed);
}

fn context_fields() -> Vec<(String, Value)> {
    if PROVIDER_COUNT.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    // Re-entrancy guard: a provider that logs must not recurse into the
    // providers again.
    if IN_PROVIDERS.with(|flag| flag.replace(true)) {
        return Vec::new();
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IN_PROVIDERS.with(|flag| flag.set(false));
        }
    }
    let _reset = Reset;
    // Snapshot the list so no lock is held while user callbacks run.
    let snapshot: Vec<Provider> = rlock(&PROVIDERS).clone();
    let mut out = Vec::new();
    for provider in &snapshot {
        out.extend(provider());
    }
    out
}

/// The current trace context as log fields: `trace_id` and `span_id` of the
/// active [`ferroly::obs`](../obs/index.html) span, or an empty vec when no
/// span is active.
///
/// Without the `obs` feature this always returns an empty vec, so code that
/// calls it compiles either way.
pub fn trace_context_fields() -> Vec<(String, Value)> {
    #[cfg(feature = "obs")]
    {
        if let Some(ctx) = ferroly::obs::current() {
            return vec![
                (
                    "trace_id".to_string(),
                    Value::Str(ctx.trace_id().to_string()),
                ),
                ("span_id".to_string(), Value::Str(ctx.span_id().to_string())),
            ];
        }
    }
    Vec::new()
}

/// Registers [`trace_context_fields`] as a context provider, so every record
/// logged inside an `obs` span carries that span's `trace_id` and `span_id` —
/// linking logs to traces. Idempotent. Without the `obs` feature the provider
/// is still registered but never adds a field.
pub fn add_trace_context_provider() {
    if !TRACE_PROVIDER.swap(true, Ordering::Relaxed) {
        add_context_provider(trace_context_fields);
    }
}

// ---- format --------------------------------------------------------------------

/// The output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `2026-07-05T…Z INFO  message key=value …`
    Plain,
    /// One JSON object per line (typed field values preserved).
    Json,
}

// ---- logger --------------------------------------------------------------------

fn default_redactor() -> Arc<Redactor> {
    static DEFAULT: OnceLock<Arc<Redactor>> = OnceLock::new();
    Arc::clone(DEFAULT.get_or_init(|| Arc::new(Redactor::default())))
}

/// A cheaply-cloneable structured logger. Clones share the sinks and carry
/// their own accumulated context fields, target and level floor.
#[derive(Clone)]
pub struct Logger {
    format: Format,
    min: Level,
    fields: Vec<(String, Value)>,
    target: Option<Arc<str>>,
    sinks: Arc<[Arc<Sink>]>,
    redactor: Arc<Redactor>,
    sampler: Option<Arc<Sampler>>,
    caller: bool,
    precision: Precision,
    offset_secs: i32,
}

impl Default for Logger {
    fn default() -> Self {
        Self {
            format: Format::Plain,
            min: Level::Trace,
            fields: Vec::new(),
            target: None,
            sinks: Arc::from(vec![Arc::new(Sink::stderr())]),
            redactor: default_redactor(),
            sampler: None,
            caller: false,
            precision: Precision::Millis,
            offset_secs: 0,
        }
    }
}

impl fmt::Debug for Logger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logger")
            .field("format", &self.format)
            .field("level", &self.min)
            .field("target", &self.target)
            .field("fields", &self.fields.len())
            .field("sinks", &self.sinks.len())
            .finish()
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

    /// Sets the output format used by every sink that has no format of its own.
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

    /// Directs output synchronously to `writer` (a file, a buffer, …),
    /// replacing every sink configured so far.
    ///
    /// The writer is flushed after each record at [`Level::Warn`] or above and
    /// by [`flush`](Self::flush); see [`Sink::writer`] for the full buffering
    /// contract.
    pub fn to_writer<W: Write + Send + 'static>(self, writer: W) -> Self {
        self.to_sink(Sink::writer(writer))
    }

    /// Directs output to a **background writer thread** over a bounded channel of
    /// `capacity` records, replacing every sink configured so far. The logging
    /// call never blocks on I/O; when the buffer is full records are dropped and
    /// counted (see [`dropped`](Self::dropped)). For a blocking policy use
    /// [`to_sink`](Self::to_sink) with [`Sink::non_blocking`] and
    /// [`Overflow::Block`].
    pub fn async_to<W: Write + Send + 'static>(self, writer: W, capacity: usize) -> Self {
        self.to_sink(Sink::non_blocking(writer, capacity))
    }

    /// Directs output to `sink` only, replacing every sink configured so far.
    pub fn to_sink(mut self, sink: Sink) -> Self {
        self.sinks = Arc::from(vec![Arc::new(sink)]);
        self
    }

    /// Adds `sink` alongside the existing ones: every record is written to each
    /// sink whose own minimum level admits it, in that sink's format.
    pub fn add_sink(mut self, sink: Sink) -> Self {
        let mut sinks: Vec<Arc<Sink>> = self.sinks.to_vec();
        sinks.push(Arc::new(sink));
        self.sinks = Arc::from(sinks);
        self
    }

    /// Flushes every sink. A synchronous sink flushes its writer (so a
    /// `BufWriter` reaches the file); a non-blocking sink blocks until its
    /// background thread has written and flushed everything queued so far. Call
    /// it before process exit so buffered records aren't lost.
    pub fn flush(&self) {
        // A writer that logs (or flushes) from inside its own `write` must not
        // re-enter the sink it is running under.
        if sink::writing() {
            return;
        }
        for sink in self.sinks.iter() {
            sink.flush();
        }
    }

    /// The number of records dropped by non-blocking sinks — because the buffer
    /// was full under [`Overflow::Drop`], or the writer thread is gone. Always 0
    /// for synchronous sinks.
    pub fn dropped(&self) -> u64 {
        self.sinks.iter().map(|s| s.dropped()).sum()
    }

    /// The number of records suppressed by [`sample`](Self::sample) (0 when
    /// sampling is off).
    pub fn suppressed(&self) -> u64 {
        self.sampler.as_ref().map_or(0, |s| s.suppressed())
    }

    /// Adds a context field carried on every record from this logger.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }

    /// Names the component this logger speaks for — usually a module path
    /// (`module_path!()`). The target is printed on every record (`target` key)
    /// and selects the [per-target level override](set_level_for) that applies.
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        let target: String = target.into();
        self.target = if target.is_empty() {
            None
        } else {
            Some(Arc::from(target))
        };
        self
    }

    /// This logger's target, if one was set.
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// Replaces the redaction policy (default: [`Redactor::default`]). Pass
    /// [`Redactor::none`] to print every value as given.
    pub fn redact(mut self, redactor: Redactor) -> Self {
        self.redactor = Arc::new(redactor);
        self
    }

    /// Rate-limits repeated records: at most `max` records per `interval` for
    /// each distinct (level, target, message) — field values do not count.
    /// Excess records are suppressed and counted ([`suppressed`](Self::suppressed));
    /// the first record of the next window carries a `suppressed` field with the
    /// number withheld. Clones made after this call share the budget.
    pub fn sample(mut self, max: u32, interval: Duration) -> Self {
        self.sampler = Some(Arc::new(Sampler::new(max, interval)));
        self
    }

    /// Turns caller capture on or off (default off). When on, each record
    /// carries a `caller` key with the `file:line` of the logging call, taken
    /// from `#[track_caller]` — no backtrace, no macro.
    pub fn with_caller(mut self, on: bool) -> Self {
        self.caller = on;
        self
    }

    /// Sets the timestamp precision (default [`Precision::Millis`]).
    pub fn time_precision(mut self, precision: Precision) -> Self {
        self.precision = precision;
        self
    }

    /// Prints timestamps at a fixed offset from UTC, in seconds east (default
    /// 0, printed as `Z`). The offset is rounded toward zero to whole minutes
    /// and limited to ±23:59. The standard library cannot read the system time
    /// zone, so the offset is whatever the application configures.
    pub fn utc_offset(mut self, seconds_east: i32) -> Self {
        self.offset_secs = seconds_east;
        self
    }

    /// Whether a record at `level` would currently be emitted by this logger
    /// (its floor, the global or target level, and at least one sink admit it).
    pub fn enabled(&self, level: Level) -> bool {
        if level < self.min {
            return false;
        }
        let passes = match self.target.as_deref() {
            Some(target) => enabled_for(target, level),
            None => enabled(level),
        };
        passes && self.sinks.iter().any(|s| level >= s.min)
    }

    /// Emits a record at `level` with `msg` and extra typed `fields`.
    #[track_caller]
    pub fn log(&self, level: Level, msg: &str, fields: &[(&str, Value)]) {
        self.emit(level, msg, fields, Location::caller());
    }

    fn emit(&self, level: Level, msg: &str, fields: &[(&str, Value)], caller: &Location<'_>) {
        if !self.enabled(level) {
            return;
        }
        // A sink writer that logs would deadlock on its own lock: drop the
        // nested record instead.
        if sink::writing() {
            return;
        }
        let target = self.target.as_deref();
        let mut suppressed = 0;
        if let Some(sampler) = &self.sampler {
            let mut hasher = DefaultHasher::new();
            (level, target, msg).hash(&mut hasher);
            match sampler.check_hashed(hasher.finish(), Instant::now()) {
                Some(carried) => suppressed = carried,
                None => return,
            }
        }

        let ts = format::rfc3339(SystemTime::now(), self.precision, self.offset_secs);
        let provided = context_fields();
        let caller = self
            .caller
            .then(|| format!("{}:{}", caller.file(), caller.line()));
        let suppressed_value = Value::UInt(suppressed);
        let sources = (suppressed > 0)
            .then_some(("suppressed", &suppressed_value))
            .into_iter()
            .chain(provided.iter().map(|(k, v)| (k.as_str(), v)))
            .chain(self.fields.iter().map(|(k, v)| (k.as_str(), v)))
            .chain(fields.iter().map(|(k, v)| (*k, v)));
        // A field may not reuse a key the record itself prints.
        let reserved = |key: &str| match key {
            "ts" | "level" | "msg" => true,
            "target" => target.is_some(),
            "caller" => caller.is_some(),
            _ => false,
        };
        let (top, nested) = format::resolve(&reserved, &self.redactor, sources);
        let record = format::Record {
            ts: &ts,
            level,
            msg,
            target,
            caller: caller.as_deref(),
            top: &top,
            nested: &nested,
        };

        // Each format is rendered at most once, however many sinks use it.
        let (mut plain, mut json) = (None, None);
        for sink in self.sinks.iter() {
            if level < sink.min {
                continue;
            }
            let line: &String = match sink.format.unwrap_or(self.format) {
                Format::Plain => plain.get_or_insert_with(|| format::plain_line(&record)),
                Format::Json => json.get_or_insert_with(|| format::json_line(&record)),
            };
            sink.write(level, line.as_bytes());
        }
    }

    /// Logs at [`Level::Trace`].
    #[track_caller]
    pub fn trace(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Trace, msg, fields);
    }
    /// Logs at [`Level::Debug`].
    #[track_caller]
    pub fn debug(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Debug, msg, fields);
    }
    /// Logs at [`Level::Info`].
    #[track_caller]
    pub fn info(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Info, msg, fields);
    }
    /// Logs at [`Level::Warn`].
    #[track_caller]
    pub fn warn(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Warn, msg, fields);
    }
    /// Logs at [`Level::Error`].
    #[track_caller]
    pub fn error(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Error, msg, fields);
    }
}

// ---- process-global logger -----------------------------------------------

static GLOBAL: RwLock<Option<Arc<Logger>>> = RwLock::new(None);

/// Installs the process-global logger used by the free [`info`] / [`error`] /
/// etc. functions. May be called again to reconfigure.
pub fn set_global(logger: Logger) {
    *wlock(&GLOBAL) = Some(Arc::new(logger));
}

/// The process-global logger as an `Arc` — cloning it per free-function call is
/// cheap (a refcount bump), avoiding a deep copy of its context fields.
fn global() -> Arc<Logger> {
    if let Some(logger) = rlock(&GLOBAL).as_ref() {
        return Arc::clone(logger);
    }
    static FALLBACK: OnceLock<Arc<Logger>> = OnceLock::new();
    Arc::clone(FALLBACK.get_or_init(|| Arc::new(Logger::default())))
}

/// A clone of the process-global logger (the stderr default when none was
/// installed) — the starting point for a child with its own target or fields:
/// `ferroly::log::logger().with_target(module_path!())`. The clone shares the
/// global logger's sinks as they are **now**; it does not follow a later
/// [`set_global`].
pub fn logger() -> Logger {
    (*global()).clone()
}

/// Flushes the process-global logger (see [`Logger::flush`]). Best-effort and
/// safe to call at any time — call it on the shutdown path, or hold a
/// [`flush_guard`] in `main`.
pub fn flush() {
    global().flush();
}

/// Flushes the process-global logger when dropped. See [`flush_guard`].
#[must_use = "the global logger is flushed when the guard is dropped"]
#[derive(Debug)]
pub struct FlushGuard(());

impl Drop for FlushGuard {
    fn drop(&mut self) {
        flush();
    }
}

/// Returns a guard that [`flush`]es the global logger when it goes out of
/// scope — hold it in `main` so buffered records are written on every return
/// path:
///
/// ```
/// let _flush = ferroly::log::flush_guard();
/// // … run the application …
/// ```
pub fn flush_guard() -> FlushGuard {
    FlushGuard(())
}

/// Logs via the global logger at [`Level::Trace`].
#[track_caller]
pub fn trace(msg: &str, fields: &[(&str, Value)]) {
    global().trace(msg, fields);
}
/// Logs via the global logger at [`Level::Debug`].
#[track_caller]
pub fn debug(msg: &str, fields: &[(&str, Value)]) {
    global().debug(msg, fields);
}
/// Logs via the global logger at [`Level::Info`].
#[track_caller]
pub fn info(msg: &str, fields: &[(&str, Value)]) {
    global().info(msg, fields);
}
/// Logs via the global logger at [`Level::Warn`].
#[track_caller]
pub fn warn(msg: &str, fields: &[(&str, Value)]) {
    global().warn(msg, fields);
}
/// Logs via the global logger at [`Level::Error`].
#[track_caller]
pub fn error(msg: &str, fields: &[(&str, Value)]) {
    global().error(msg, fields);
}

// ---- panic hook ------------------------------------------------------------------

/// Installs a panic hook that logs every panic through the global logger and
/// then runs the previously installed hook (by default the standard one that
/// prints to stderr). Equivalent to `install_panic_hook_with(true)`.
pub fn install_panic_hook() {
    install_panic_hook_with(true);
}

/// Installs a panic hook that logs every panic through the global logger at
/// [`Level::Error`] — message `panic`, fields `panic.message`,
/// `panic.location` (`file:line:column`) and `panic.thread` — and then
/// [`flush`]es it, so the record survives an abort.
///
/// With `chain_previous` the hook that was installed before runs afterwards;
/// without it the panic is reported through the logger only.
///
/// A panic raised inside a synchronous sink's own writer is not logged (that
/// sink is busy on the panicking thread); it still reaches the chained hook.
pub fn install_panic_hook_with(chain_previous: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            s
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.as_str()
        } else {
            "non-string panic payload"
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let thread = std::thread::current();
        let logger = global();
        logger.error(
            "panic",
            &[
                ("panic.message", message.into()),
                ("panic.location", location.into()),
                ("panic.thread", thread.name().unwrap_or("<unnamed>").into()),
            ],
        );
        logger.flush();
        if chain_previous {
            previous(info);
        }
    }));
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
    fn level_names_round_trip() {
        for level in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
        ] {
            assert_eq!(level.as_str().parse::<Level>(), Ok(level));
            assert_eq!(level.to_string().to_uppercase(), level.as_upper_str());
            assert_eq!(Level::from_u8(level as u8), level);
        }
        assert_eq!(" WARNING ".parse::<Level>(), Ok(Level::Warn));
        assert!("loud".parse::<Level>().is_err());
    }

    #[test]
    fn target_prefixes_end_at_segment_boundaries() {
        assert!(target_matches("app::db", "app::db"));
        assert!(target_matches("app::db", "app::db::pool"));
        assert!(target_matches("app.db", "app.db.pool"));
        assert!(target_matches("app/db", "app/db/pool"));
        assert!(!target_matches("app::db", "app::dbx"));
        assert!(!target_matches("app::db", "app"));
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
