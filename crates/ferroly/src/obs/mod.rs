//! Distributed span-and-event tracing core for enterprise services.
//!
//! This module records units of work as **spans** (a named, timed operation)
//! and **events** (a point-in-time note attached to a span). Spans nest: a span
//! opened while another is active becomes its child, inheriting the same
//! `trace_id` and pointing back at its parent via `parent_id`, so a whole
//! request forms one causally-linked tree. A root span mints a fresh
//! `trace_id`. Each span carries typed structured fields backed by
//! [`ferroly::codec::Value`], so numbers stay numbers when rendered.
//!
//! Everything is std-only. Identifiers need no randomness source: a per-process
//! seed (nanoseconds since the epoch, captured once) is combined with atomic
//! counters, yielding a 16-byte `trace_id` and an 8-byte `span_id` exposed as
//! lowercase hex. Uniqueness within the process is what matters. The current
//! span is tracked in a thread-local stack, so spans on different threads are
//! independent and no locking is needed on the hot path.
//!
//! A global [`Level`] filter drops spans and events below the threshold cheaply
//! — a filtered span never touches the thread-local stack. When a span finishes
//! (its [`SpanGuard`] is dropped) it is assembled into a [`FinishedSpan`] and
//! handed to the installed [`Exporter`], if any. With no exporter installed,
//! finishing a span is a near-free no-op. A built-in [`JsonExporter`] renders
//! one JSON object per span; back it with any writer, or supply your own
//! exporter to forward spans to a collector.
//!
//! ```
//! use std::sync::{Arc, Mutex};
//! use ferroly::obs::{Exporter, FinishedSpan, Level, Span, set_exporter, set_level};
//!
//! struct Collect(Arc<Mutex<Vec<String>>>);
//! impl Exporter for Collect {
//!     fn export(&self, span: &FinishedSpan) {
//!         self.0.lock().unwrap().push(span.name.clone());
//!     }
//! }
//!
//! let sink = Arc::new(Mutex::new(Vec::new()));
//! set_exporter(Box::new(Collect(sink.clone())));
//! set_level(Level::Debug);
//!
//! {
//!     let root = Span::new("request").level(Level::Info).field("path", "/health").enter();
//!     root.event(Level::Debug, "checking backend", vec![]);
//!     let _child = Span::new("db.query").field("rows", 3).enter();
//! } // `_child` finishes, then `root`
//!
//! let names = sink.lock().unwrap();
//! assert!(names.contains(&"db.query".to_string()));
//! assert!(names.contains(&"request".to_string()));
//! ```

#![deny(missing_docs)]

use std::cell::RefCell;
use std::fmt;
use std::io::Write;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ferroly::codec::{json, Value};

#[cfg(feature = "http")]
mod otlp;
#[cfg(feature = "http")]
pub use otlp::OtlpHttpExporter;

// ---- level ---------------------------------------------------------------

/// Severity level, ordered `Trace < Debug < Info < Warn < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Very fine-grained tracing detail.
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
    /// The lowercase name of this level (`"info"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
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

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Sets the global minimum level; spans and events below it are dropped.
pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Returns the current global minimum level (default [`Level::Info`]).
pub fn level() -> Level {
    Level::from_u8(LEVEL.load(Ordering::Relaxed))
}

/// True when `level` is at or above the global threshold.
fn enabled(level: Level) -> bool {
    (level as u8) >= LEVEL.load(Ordering::Relaxed)
}

// ---- identifiers ---------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Renders bytes as a lowercase hex string.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Nanoseconds since the epoch, captured once per process, used as the high
/// half of every `trace_id` so ids do not collide with another run.
fn seed() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(|| now_unix_nanos() as u64)
}

/// Wall-clock nanoseconds since the Unix epoch (0 if the clock predates it).
fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Mints a fresh 8-byte span identifier as hex (16 chars).
fn new_span_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    hex(&n.to_be_bytes())
}

/// Mints a fresh 16-byte trace identifier as hex (32 chars): the process seed
/// followed by a monotonic counter.
fn new_trace_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&seed().to_be_bytes());
    bytes[8..].copy_from_slice(&n.to_be_bytes());
    hex(&bytes)
}

// ---- finished records ----------------------------------------------------

/// A point-in-time event captured on a span.
#[derive(Debug, Clone)]
pub struct SpanEvent {
    /// Severity of the event.
    pub level: Level,
    /// Human-readable message.
    pub message: String,
    /// Typed structured fields describing the event.
    pub fields: Vec<(String, Value)>,
    /// Wall-clock nanoseconds since the Unix epoch at which it was recorded.
    pub unix_nanos: u128,
}

/// A completed span, ready to be exported.
#[derive(Debug, Clone)]
pub struct FinishedSpan {
    /// The span's name.
    pub name: String,
    /// The 32-char hex trace identifier shared by every span in the trace.
    pub trace_id: String,
    /// The 16-char hex identifier unique to this span.
    pub span_id: String,
    /// The parent span's id, or `None` for a root span.
    pub parent_id: Option<String>,
    /// The span's severity level.
    pub level: Level,
    /// Wall-clock nanoseconds since the Unix epoch at which the span started.
    pub start_unix_nanos: u128,
    /// Elapsed nanoseconds between entering and finishing the span.
    pub duration_nanos: u128,
    /// Typed structured fields attached to the span.
    pub fields: Vec<(String, Value)>,
    /// Events recorded on the span, in order.
    pub events: Vec<SpanEvent>,
}

// ---- exporter ------------------------------------------------------------

/// A sink for finished spans. Implementations must be safe to share across
/// threads because a single exporter is installed process-wide.
pub trait Exporter: Send + Sync {
    /// Consumes one finished span. Called once per span, when its guard drops.
    fn export(&self, span: &FinishedSpan);

    /// Flushes any buffered output. The default does nothing.
    fn flush(&self) {}
}

static HAS_EXPORTER: AtomicBool = AtomicBool::new(false);

fn exporter_slot() -> &'static Mutex<Option<Box<dyn Exporter>>> {
    static SLOT: OnceLock<Mutex<Option<Box<dyn Exporter>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Installs the process-wide exporter, replacing any previous one. Until one is
/// installed, finishing a span is a cheap no-op.
pub fn set_exporter(exporter: Box<dyn Exporter>) {
    let mut slot = exporter_slot().lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(exporter);
    HAS_EXPORTER.store(true, Ordering::Release);
}

/// Flushes the installed exporter, if any.
pub fn flush() {
    if !HAS_EXPORTER.load(Ordering::Acquire) {
        return;
    }
    let slot = exporter_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(exporter) = slot.as_ref() {
        exporter.flush();
    }
}

/// Hands a finished span to the installed exporter, gated on a cheap atomic so
/// the common "no exporter" path never touches the lock.
fn export(span: &FinishedSpan) {
    if !HAS_EXPORTER.load(Ordering::Acquire) {
        return;
    }
    let slot = exporter_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(exporter) = slot.as_ref() {
        exporter.export(span);
    }
}

// ---- thread-local current-span stack -------------------------------------

/// A span that is currently entered on this thread. Fields and events are
/// accumulated here until the span finishes.
struct Live {
    name: String,
    level: Level,
    trace_id: String,
    span_id: String,
    parent_id: Option<String>,
    start: Instant,
    start_unix_nanos: u128,
    fields: Vec<(String, Value)>,
    events: Vec<SpanEvent>,
}

thread_local! {
    static STACK: RefCell<Vec<Live>> = const { RefCell::new(Vec::new()) };
}

// ---- span builder --------------------------------------------------------

/// A builder for a span. Configure it with [`level`](Span::level) and
/// [`field`](Span::field), then call [`enter`](Span::enter) to make it the
/// current span on this thread.
pub struct Span {
    name: String,
    level: Level,
    fields: Vec<(String, Value)>,
}

impl Span {
    /// Starts building a span with the given name at [`Level::Info`].
    pub fn new(name: impl Into<String>) -> Span {
        Span {
            name: name.into(),
            level: Level::Info,
            fields: Vec::new(),
        }
    }

    /// Sets the span's severity level.
    pub fn level(mut self, level: Level) -> Span {
        self.level = level;
        self
    }

    /// Attaches a typed field. Any type convertible into a
    /// [`ferroly::codec::Value`] works, so `("k", "v")`, `("k", 5)`, and
    /// `("k", true)` are all accepted.
    pub fn field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Span {
        self.fields.push((key.into(), value.into()));
        self
    }

    /// Enters the span, making it current on this thread and returning an RAII
    /// [`SpanGuard`] that finishes and exports it on drop.
    ///
    /// If the span's level is below the global threshold it is not entered: the
    /// returned guard is inert and the thread-local stack is untouched. When
    /// entered, the span links to the innermost currently-entered span as its
    /// parent (inheriting that span's `trace_id`); with none active it becomes
    /// a root with a fresh `trace_id`.
    pub fn enter(self) -> SpanGuard {
        if !enabled(self.level) {
            return SpanGuard {
                active: false,
                _not_send: PhantomData,
            };
        }
        STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let (trace_id, parent_id) = match stack.last() {
                Some(top) => (top.trace_id.clone(), Some(top.span_id.clone())),
                None => (new_trace_id(), None),
            };
            stack.push(Live {
                name: self.name,
                level: self.level,
                trace_id,
                span_id: new_span_id(),
                parent_id,
                start: Instant::now(),
                start_unix_nanos: now_unix_nanos(),
                fields: self.fields,
                events: Vec::new(),
            });
        });
        SpanGuard {
            active: true,
            _not_send: PhantomData,
        }
    }
}

// ---- span guard ----------------------------------------------------------

/// An RAII handle to an entered span. While it is alive its span is the current
/// span on this thread; dropping it finishes the span and exports it.
///
/// Guards must be dropped in reverse order of entry (the usual scope-based
/// nesting). The type is deliberately neither `Send` nor `Sync`: a span belongs
/// to the thread that entered it.
pub struct SpanGuard {
    active: bool,
    _not_send: PhantomData<*const ()>,
}

impl SpanGuard {
    /// Adds a field to this span after it has been entered. A no-op on an inert
    /// (filtered-out) guard.
    pub fn record(&self, key: impl Into<String>, value: impl Into<Value>) {
        if !self.active {
            return;
        }
        STACK.with(|cell| {
            if let Some(top) = cell.borrow_mut().last_mut() {
                top.fields.push((key.into(), value.into()));
            }
        });
    }

    /// Records an event on this span. Dropped if `level` is below the global
    /// threshold or the guard is inert.
    pub fn event(&self, level: Level, message: impl Into<String>, fields: Vec<(String, Value)>) {
        if !self.active || !enabled(level) {
            return;
        }
        let ev = SpanEvent {
            level,
            message: message.into(),
            fields,
            unix_nanos: now_unix_nanos(),
        };
        STACK.with(|cell| {
            if let Some(top) = cell.borrow_mut().last_mut() {
                top.events.push(ev);
            }
        });
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let live = STACK.with(|cell| cell.borrow_mut().pop());
        let live = match live {
            Some(l) => l,
            None => return,
        };
        let finished = FinishedSpan {
            name: live.name,
            trace_id: live.trace_id,
            span_id: live.span_id,
            parent_id: live.parent_id,
            level: live.level,
            start_unix_nanos: live.start_unix_nanos,
            duration_nanos: live.start.elapsed().as_nanos(),
            fields: live.fields,
            events: live.events,
        };
        export(&finished);
    }
}

// ---- free-standing events ------------------------------------------------

/// Records a point-in-time event on the current span.
///
/// If a span is active on this thread the event is attached to it. If there is
/// no current span and the level passes the filter, the event is emitted as a
/// standalone zero-duration [`FinishedSpan`] named `"event"` carrying the
/// single event, so it still reaches the exporter. Events below the global
/// threshold are dropped.
pub fn event(level: Level, message: impl Into<String>, fields: Vec<(String, Value)>) {
    if !enabled(level) {
        return;
    }
    let mut ev = Some(SpanEvent {
        level,
        message: message.into(),
        fields,
        unix_nanos: now_unix_nanos(),
    });
    let attached = STACK.with(|cell| {
        let mut stack = cell.borrow_mut();
        if let Some(top) = stack.last_mut() {
            top.events.push(ev.take().expect("event present"));
            true
        } else {
            false
        }
    });
    if attached {
        return;
    }
    // No current span: emit a synthetic span so the event is not lost.
    let ev = ev.expect("event present");
    let finished = FinishedSpan {
        name: "event".to_string(),
        trace_id: new_trace_id(),
        span_id: new_span_id(),
        parent_id: None,
        level,
        start_unix_nanos: ev.unix_nanos,
        duration_nanos: 0,
        fields: Vec::new(),
        events: vec![ev],
    };
    export(&finished);
}

// ---- built-in JSON exporter ----------------------------------------------

/// An [`Exporter`] that writes one compact JSON object per finished span,
/// newline-terminated, to any writer. The writer is guarded by a mutex, so
/// spans from many threads interleave line-by-line rather than byte-by-byte.
pub struct JsonExporter {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl JsonExporter {
    /// Builds an exporter that writes to `writer` (for example an open file or
    /// `std::io::stdout()`).
    pub fn new(writer: impl Write + Send + 'static) -> JsonExporter {
        JsonExporter {
            writer: Mutex::new(Box::new(writer)),
        }
    }
}

impl Exporter for JsonExporter {
    fn export(&self, span: &FinishedSpan) {
        let mut line = json::to_string(&span_to_value(span));
        line.push('\n');
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.write_all(line.as_bytes());
    }

    fn flush(&self) {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.flush();
    }
}

/// Represents a `u128` as a numeric [`Value`] when it fits in a `u64`, falling
/// back to a decimal string for values beyond that range.
fn u128_value(v: u128) -> Value {
    match u64::try_from(v) {
        Ok(u) => Value::UInt(u),
        Err(_) => Value::Str(v.to_string()),
    }
}

/// Copies a field list into a [`Value::Object`].
fn fields_value(fields: &[(String, Value)]) -> Value {
    Value::Object(fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// Builds the JSON [`Value`] tree for a finished span.
fn span_to_value(span: &FinishedSpan) -> Value {
    let mut obj: Vec<(String, Value)> = Vec::with_capacity(9);
    obj.push(("name".to_string(), Value::Str(span.name.clone())));
    obj.push(("trace_id".to_string(), Value::Str(span.trace_id.clone())));
    obj.push(("span_id".to_string(), Value::Str(span.span_id.clone())));
    obj.push((
        "parent_id".to_string(),
        match &span.parent_id {
            Some(p) => Value::Str(p.clone()),
            None => Value::Null,
        },
    ));
    obj.push(("level".to_string(), Value::Str(span.level.to_string())));
    obj.push((
        "start_unix_nanos".to_string(),
        u128_value(span.start_unix_nanos),
    ));
    obj.push((
        "duration_nanos".to_string(),
        u128_value(span.duration_nanos),
    ));
    obj.push(("fields".to_string(), fields_value(&span.fields)));

    let events: Vec<Value> = span
        .events
        .iter()
        .map(|ev| {
            Value::Object(vec![
                ("level".to_string(), Value::Str(ev.level.to_string())),
                ("message".to_string(), Value::Str(ev.message.clone())),
                ("fields".to_string(), fields_value(&ev.fields)),
                ("unix_nanos".to_string(), u128_value(ev.unix_nanos)),
            ])
        })
        .collect();
    obj.push(("events".to_string(), Value::Array(events)));

    Value::Object(obj)
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, MutexGuard};
    use std::thread::sleep;
    use std::time::Duration;

    /// Runs tests that mutate the global exporter/level one at a time so the
    /// shared state does not race under the parallel test harness.
    fn lock_globals() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[derive(Clone)]
    struct VecExporter(Arc<Mutex<Vec<FinishedSpan>>>);

    impl Exporter for VecExporter {
        fn export(&self, span: &FinishedSpan) {
            self.0.lock().unwrap().push(span.clone());
        }
    }

    fn install() -> Arc<Mutex<Vec<FinishedSpan>>> {
        let store = Arc::new(Mutex::new(Vec::new()));
        set_exporter(Box::new(VecExporter(store.clone())));
        store
    }

    fn find<'a>(spans: &'a [FinishedSpan], name: &str) -> &'a FinishedSpan {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no span named {name}"))
    }

    #[test]
    fn span_creates_hex_ids() {
        let _g = lock_globals();
        set_level(Level::Trace);
        let store = install();
        {
            let _s = Span::new("op").enter();
        }
        let spans = store.lock().unwrap();
        let s = find(&spans, "op");
        assert_eq!(s.trace_id.len(), 32, "trace_id is 16 bytes of hex");
        assert_eq!(s.span_id.len(), 16, "span_id is 8 bytes of hex");
        assert!(s.trace_id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(s.span_id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(s.parent_id.is_none());
    }

    #[test]
    fn child_links_to_parent_and_shares_trace() {
        let _g = lock_globals();
        set_level(Level::Trace);
        let store = install();
        {
            let _root = Span::new("root").enter();
            {
                let _child = Span::new("child").enter();
            }
        }
        let spans = store.lock().unwrap();
        let root = find(&spans, "root");
        let child = find(&spans, "child");
        assert_eq!(child.trace_id, root.trace_id, "child inherits trace_id");
        assert_eq!(
            child.parent_id.as_deref(),
            Some(root.span_id.as_str()),
            "child links to its parent"
        );
        assert_ne!(child.span_id, root.span_id, "distinct span ids");
    }

    #[test]
    fn fields_are_captured() {
        let _g = lock_globals();
        set_level(Level::Trace);
        let store = install();
        {
            let s = Span::new("work")
                .field("path", "/x")
                .field("count", 7)
                .enter();
            s.record("done", true);
        }
        let spans = store.lock().unwrap();
        let s = find(&spans, "work");
        assert_eq!(s.fields.len(), 3);
        assert_eq!(s.fields[0], ("path".to_string(), Value::Str("/x".into())));
        assert_eq!(s.fields[1], ("count".to_string(), Value::Int(7)));
        assert_eq!(s.fields[2], ("done".to_string(), Value::Bool(true)));
    }

    #[test]
    fn level_filter_drops_below_threshold() {
        let _g = lock_globals();
        set_level(Level::Info);
        let store = install();
        {
            let _low = Span::new("debug.span").level(Level::Debug).enter();
        }
        {
            let _high = Span::new("warn.span").level(Level::Warn).enter();
        }
        let spans = store.lock().unwrap();
        assert!(
            spans.iter().all(|s| s.name != "debug.span"),
            "below-threshold span is dropped"
        );
        assert!(
            spans.iter().any(|s| s.name == "warn.span"),
            "at-or-above-threshold span is kept"
        );
    }

    #[test]
    fn custom_exporter_receives_finished_spans() {
        let _g = lock_globals();
        set_level(Level::Trace);
        let store = install();
        {
            let _a = Span::new("a").enter();
        }
        {
            let _b = Span::new("b").enter();
        }
        let spans = store.lock().unwrap();
        assert!(spans.iter().any(|s| s.name == "a"));
        assert!(spans.iter().any(|s| s.name == "b"));
    }

    #[test]
    fn event_recorded_on_current_span() {
        let _g = lock_globals();
        set_level(Level::Trace);
        let store = install();
        {
            let _s = Span::new("outer").enter();
            event(
                Level::Info,
                "midway",
                vec![("n".to_string(), Value::Int(1))],
            );
        }
        let spans = store.lock().unwrap();
        let s = find(&spans, "outer");
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].message, "midway");
        assert_eq!(s.events[0].fields[0].1, Value::Int(1));
    }

    #[test]
    fn event_without_span_is_standalone() {
        let _g = lock_globals();
        set_level(Level::Trace);
        let store = install();
        // No span entered on this thread.
        event(Level::Warn, "orphan", vec![]);
        let spans = store.lock().unwrap();
        let s = find(&spans, "event");
        assert_eq!(s.duration_nanos, 0);
        assert!(s.parent_id.is_none());
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].message, "orphan");
        assert_eq!(s.events[0].level, Level::Warn);
    }

    #[test]
    fn duration_is_measured() {
        let _g = lock_globals();
        set_level(Level::Trace);
        let store = install();
        {
            let _s = Span::new("slow").enter();
            sleep(Duration::from_millis(2));
        }
        let spans = store.lock().unwrap();
        let s = find(&spans, "slow");
        assert!(s.duration_nanos > 0, "elapsed time is recorded");
        assert!(s.start_unix_nanos > 0, "start wall-clock is recorded");
    }

    #[test]
    fn json_exporter_renders_fields_as_numbers() {
        let span = FinishedSpan {
            name: "req".to_string(),
            trace_id: "0".repeat(32),
            span_id: "1".repeat(16),
            parent_id: None,
            level: Level::Info,
            start_unix_nanos: 1_000,
            duration_nanos: 42,
            fields: vec![("count".to_string(), Value::Int(9))],
            events: vec![SpanEvent {
                level: Level::Debug,
                message: "hi".to_string(),
                fields: vec![],
                unix_nanos: 1_500,
            }],
        };
        let rendered = json::to_string(&span_to_value(&span));
        assert!(rendered.contains(r#""name":"req""#));
        assert!(rendered.contains(r#""level":"info""#));
        assert!(rendered.contains(r#""count":9"#), "numbers stay numeric");
        assert!(rendered.contains(r#""duration_nanos":42"#));
        assert!(rendered.contains(r#""parent_id":null"#));
        assert!(rendered.contains(r#""message":"hi""#));
    }

    #[test]
    fn levels_are_ordered_and_lowercase() {
        assert!(Level::Trace < Level::Debug);
        assert!(Level::Info < Level::Warn);
        assert!(Level::Warn < Level::Error);
        assert_eq!(Level::Error.to_string(), "error");
        assert_eq!(Level::Trace.to_string(), "trace");
    }
}
