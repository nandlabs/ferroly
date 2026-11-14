//! Distributed span-and-event tracing core for enterprise services.
//!
//! This module records units of work as **spans** (a named, timed operation)
//! and **events** (a point-in-time note attached to a span). Spans nest: a span
//! started while another is current becomes its child, inheriting the same
//! trace id and pointing back at its parent, so a whole request forms one
//! causally-linked tree — across threads, `.await` points and, through the W3C
//! `traceparent` header, across services. Each span carries typed structured
//! fields backed by [`ferroly::codec::Value`], a [kind](SpanKind), a
//! [status](Status) and optional [links](Link).
//!
//! Everything in the core is std-only.
//!
//! # Identifiers
//!
//! A [`TraceId`] is 16 random bytes and a [`SpanId`] 8, shown as lowercase hex.
//! They come from a per-thread xoshiro256** generator seeded from the wall
//! clock, a monotonic clock, the process and thread ids, stack and heap
//! addresses and `RandomState` (operating-system entropy). They are never
//! all-zero and never sequential, so traces from different processes and
//! restarts do not collide.
//!
//! # The current context
//!
//! "The current span" is a [`Context`] — a small `Send + Sync + Clone` value —
//! kept in a per-thread stack of attachments. There are three ways to put a
//! span there, for three situations:
//!
//! | Code | Use | How |
//! |---|---|---|
//! | synchronous | [`Span::enter`] → [`SpanGuard`] | current on this thread until the guard drops |
//! | asynchronous | [`instrument`]`(future, span)` | current **during each poll** of the future, on whichever thread polls it |
//! | anything | [`Span::child_of`]`(&ctx)` + [`Span::start`] | no ambient state at all: the parent is passed explicitly |
//!
//! [`with_context`] and [`with_current_context`] carry a context into a future
//! without opening a span — that is how a spawned task or one branch of a
//! `join!` inherits its parent.
//!
//! Every attachment is removed **by its own identity**, never by position:
//! guards can be dropped in any order, and a span's fields, events and duration
//! live in the span itself rather than in the stack, so one span can never end
//! up with another's data.
//!
//! ## Async rule of thumb
//!
//! Do not hold a [`SpanGuard`] across an `.await` in a bare future: while that
//! future is suspended its span would still look current to whatever else is
//! polled on the same thread. Wrap the future with [`instrument`] (or
//! [`with_current_context`]) instead. A wrapped future owns its part of the
//! context stack — including any guards it does hold across awaits — and
//! installs it only while it is being polled, so sibling futures under
//! `join!`/`select!` and tasks on a work-stealing runtime never see each
//! other's spans. This works with every executor; it does not use task-locals.
//!
//! # Sampling, filtering, export
//!
//! A global [`Level`] filter drops spans and events below the threshold
//! cheaply. A [`Sampler`] decides per trace whether spans are recorded; the
//! decision propagates through [`Context::is_sampled`] and the `traceparent`
//! flags. When a recorded span finishes it is assembled into a
//! [`FinishedSpan`] and handed to the installed [`Exporter`]s — a built-in
//! [`JsonExporter`], the OTLP/HTTP exporter (with the `http` feature), or your
//! own. With no exporter installed, finishing a span is a near-free no-op.
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

use std::cell::Cell;
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ferroly::codec::{json, Value};

mod context;
mod id;
#[cfg(feature = "http")]
mod otlp;
mod resource;
mod sampler;
mod semconv;
mod span;

pub use context::{
    current, extract, inject, inject_current, suppress, with_context, with_current_context,
    Context, ContextGuard, TraceParentError, WithContext, TRACEPARENT_HEADER, TRACESTATE_HEADER,
};
pub use id::{SpanId, TraceId};
#[cfg(feature = "http")]
pub use otlp::{OtlpBuilder, OtlpHttpExporter, OtlpStats};
pub use resource::Resource;
pub use sampler::{sampler, set_sampler, Sampler};
pub use semconv::{genai, http, messaging};
pub use span::{
    event, instrument, record, set_error, set_status, ActiveSpan, FinishedSpan, Instrument,
    Instrumented, Link, Span, SpanEvent, SpanGuard, SpanKind, Status, MAX_SPAN_EVENTS,
    MAX_SPAN_FIELDS, MAX_SPAN_LINKS,
};

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

// ---- exporter ------------------------------------------------------------

/// A sink for finished spans. Implementations must be safe to share across
/// threads because exporters are installed process-wide.
///
/// Exporters are called with **no internal lock held**, so an exporter may
/// itself call [`set_exporter`], [`flush`] or start spans. Spans that finish
/// on a thread while it is inside an exporter call are not exported (which
/// keeps an exporter's own instrumentation from feeding back into it).
pub trait Exporter: Send + Sync {
    /// Consumes one finished span. Called once per span, when it ends.
    fn export(&self, span: &FinishedSpan);

    /// Flushes any buffered output. The default does nothing.
    fn flush(&self) {}

    /// Flushes and releases resources; no span is delivered afterwards. The
    /// default calls [`flush`](Exporter::flush).
    fn shutdown(&self) {
        self.flush();
    }
}

type ExporterList = Arc<[Arc<dyn Exporter>]>;

static HAS_EXPORTER: AtomicBool = AtomicBool::new(false);
static EXPORTERS: RwLock<Option<ExporterList>> = RwLock::new(None);

thread_local! {
    /// Set while this thread is inside an exporter callback.
    static EXPORTING: Cell<bool> = const { Cell::new(false) };
}

/// A snapshot of the installed exporters: the lock is released before any of
/// them is called.
fn exporters() -> Option<ExporterList> {
    EXPORTERS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn store_exporters(list: Vec<Arc<dyn Exporter>>) {
    let mut slot = EXPORTERS.write().unwrap_or_else(|e| e.into_inner());
    HAS_EXPORTER.store(!list.is_empty(), Ordering::Release);
    *slot = if list.is_empty() {
        None
    } else {
        Some(Arc::from(list))
    };
}

/// Installs the process-wide exporter, replacing any previously installed
/// ones. Until one is installed, finishing a span is a cheap no-op.
pub fn set_exporter(exporter: Box<dyn Exporter>) {
    store_exporters(vec![Arc::from(exporter)]);
}

/// Installs an additional exporter next to the existing ones; every finished
/// span is delivered to each.
pub fn add_exporter(exporter: Box<dyn Exporter>) {
    let mut list: Vec<Arc<dyn Exporter>> = exporters().map(|l| l.to_vec()).unwrap_or_default();
    list.push(Arc::from(exporter));
    store_exporters(list);
}

/// Removes every installed exporter without flushing them.
pub fn clear_exporters() {
    store_exporters(Vec::new());
}

/// Flushes the installed exporters, if any.
pub fn flush() {
    if let Some(list) = exporters() {
        for exporter in list.iter() {
            exporter.flush();
        }
    }
}

/// Shuts the installed exporters down (each flushes what it holds) and removes
/// them. Call it once on the way out of the process.
pub fn shutdown() {
    let list = exporters();
    clear_exporters();
    if let Some(list) = list {
        for exporter in list.iter() {
            exporter.shutdown();
        }
    }
}

/// True when at least one exporter is installed (a cheap atomic check).
fn has_exporter() -> bool {
    HAS_EXPORTER.load(Ordering::Acquire)
}

/// Hands a finished span to the installed exporters.
fn export(span: &FinishedSpan) {
    if !has_exporter() || EXPORTING.with(|flag| flag.replace(true)) {
        return;
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            EXPORTING.with(|flag| flag.set(false));
        }
    }
    let _reset = Reset;
    if let Some(list) = exporters() {
        for exporter in list.iter() {
            exporter.export(span);
        }
    }
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

fn opt_str(value: &Option<String>) -> Value {
    match value {
        Some(s) => Value::Str(s.clone()),
        None => Value::Null,
    }
}

/// Builds the JSON [`Value`] tree for a finished span.
fn span_to_value(span: &FinishedSpan) -> Value {
    let mut obj: Vec<(String, Value)> = Vec::with_capacity(14);
    obj.push(("name".to_string(), Value::Str(span.name.clone())));
    obj.push(("trace_id".to_string(), Value::Str(span.trace_id.clone())));
    obj.push(("span_id".to_string(), Value::Str(span.span_id.clone())));
    obj.push(("parent_id".to_string(), opt_str(&span.parent_id)));
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

    obj.push(("kind".to_string(), Value::Str(span.kind.as_str().into())));
    obj.push((
        "status".to_string(),
        Value::Str(span.status.as_str().into()),
    ));
    if let Some(message) = span.status.message() {
        obj.push(("status_message".to_string(), Value::Str(message.into())));
    }
    if !span.links.is_empty() {
        let links: Vec<Value> = span
            .links
            .iter()
            .map(|link| {
                Value::Object(vec![
                    ("trace_id".to_string(), Value::Str(link.trace_id.clone())),
                    ("span_id".to_string(), Value::Str(link.span_id.clone())),
                    ("fields".to_string(), fields_value(&link.fields)),
                ])
            })
            .collect();
        obj.push(("links".to_string(), Value::Array(links)));
    }
    if span.trace_state.is_some() {
        obj.push(("trace_state".to_string(), opt_str(&span.trace_state)));
    }
    if span.parent_remote {
        obj.push(("parent_remote".to_string(), Value::Bool(true)));
    }

    Value::Object(obj)
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
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
        set_sampler(Sampler::default());
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
            start_unix_nanos: 1_000,
            duration_nanos: 42,
            fields: vec![("count".to_string(), Value::Int(9))],
            events: vec![SpanEvent {
                level: Level::Debug,
                message: "hi".to_string(),
                fields: vec![],
                unix_nanos: 1_500,
            }],
            ..FinishedSpan::default()
        };
        let rendered = json::to_string(&span_to_value(&span));
        assert!(rendered.contains(r#""name":"req""#));
        assert!(rendered.contains(r#""level":"info""#));
        assert!(rendered.contains(r#""count":9"#), "numbers stay numeric");
        assert!(rendered.contains(r#""duration_nanos":42"#));
        assert!(rendered.contains(r#""parent_id":null"#));
        assert!(rendered.contains(r#""message":"hi""#));
        assert!(rendered.contains(r#""kind":"internal""#));
        assert!(rendered.contains(r#""status":"unset""#));
        assert!(!rendered.contains("links") && !rendered.contains("status_message"));

        let failed = FinishedSpan {
            kind: SpanKind::Server,
            status: Status::error("boom"),
            trace_state: Some("k=v".into()),
            parent_remote: true,
            links: vec![Link {
                trace_id: "a".repeat(32),
                span_id: "b".repeat(16),
                trace_state: None,
                fields: vec![("why".to_string(), Value::Str("batch".into()))],
            }],
            ..span
        };
        let rendered = json::to_string(&span_to_value(&failed));
        assert!(rendered.contains(r#""kind":"server""#));
        assert!(rendered.contains(r#""status":"error","status_message":"boom""#));
        assert!(rendered.contains(r#""links":[{"trace_id":"aaaa"#));
        assert!(rendered.contains(r#""trace_state":"k=v""#));
        assert!(rendered.contains(r#""parent_remote":true"#));
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
