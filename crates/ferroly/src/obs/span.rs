//! Spans: the builder, the live handles, and the finished record.

use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context as TaskContext, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ferroly::codec::Value;

use super::context::{self, Context, ContextGuard, WithContext};
use super::id::{SpanId, TraceId};
use super::{enabled, sampler, Level};

/// The most fields (attributes) one span keeps; later ones are discarded.
pub const MAX_SPAN_FIELDS: usize = 256;
/// The most events one span keeps; later ones are discarded.
pub const MAX_SPAN_EVENTS: usize = 1024;
/// The most links one span keeps; later ones are discarded.
pub const MAX_SPAN_LINKS: usize = 128;

/// Wall-clock nanoseconds since the Unix epoch (0 if the clock predates it).
pub(super) fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ---- kind, status, links -----------------------------------------------------

/// The role a span plays in a trace (the OpenTelemetry span kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum SpanKind {
    /// An operation internal to the service (the default).
    #[default]
    Internal,
    /// Handling of an inbound request.
    Server,
    /// An outbound request to another service.
    Client,
    /// Publishing a message for later, asynchronous processing.
    Producer,
    /// Processing a message received from a producer.
    Consumer,
}

impl SpanKind {
    /// The lowercase name (`"server"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            SpanKind::Internal => "internal",
            SpanKind::Server => "server",
            SpanKind::Client => "client",
            SpanKind::Producer => "producer",
            SpanKind::Consumer => "consumer",
        }
    }
}

/// The outcome of a span.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    /// No outcome was recorded (the default).
    #[default]
    Unset,
    /// The operation was explicitly marked successful.
    Ok,
    /// The operation failed; the string describes how.
    Error(String),
}

impl Status {
    /// An error status with a description.
    pub fn error(message: impl Into<String>) -> Status {
        Status::Error(message.into())
    }

    /// True for [`Status::Error`].
    pub fn is_error(&self) -> bool {
        matches!(self, Status::Error(_))
    }

    /// `"unset"`, `"ok"` or `"error"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Unset => "unset",
            Status::Ok => "ok",
            Status::Error(_) => "error",
        }
    }

    /// The error description, for [`Status::Error`].
    pub fn message(&self) -> Option<&str> {
        match self {
            Status::Error(message) => Some(message),
            _ => None,
        }
    }
}

/// A causal reference from one span to another that is not its parent — for
/// example from a batch-processing span to the spans that enqueued its items.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// The linked span's trace id (32 hex characters).
    pub trace_id: String,
    /// The linked span's id (16 hex characters).
    pub span_id: String,
    /// The linked span's `tracestate`, if it had one.
    pub trace_state: Option<String>,
    /// Fields describing the link.
    pub fields: Vec<(String, Value)>,
}

impl Link {
    /// A link to the span named by `ctx`.
    pub fn new(ctx: &Context) -> Link {
        Link {
            trace_id: ctx.trace_id().to_string(),
            span_id: ctx.span_id().to_string(),
            trace_state: ctx.trace_state().map(str::to_string),
            fields: Vec::new(),
        }
    }

    /// Attaches a typed field to the link.
    pub fn field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Link {
        self.fields.push((key.into(), value.into()));
        self
    }
}

// ---- finished records --------------------------------------------------------

/// A point-in-time event captured on a span.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
    /// Elapsed nanoseconds between starting and finishing the span.
    pub duration_nanos: u128,
    /// Typed structured fields attached to the span.
    pub fields: Vec<(String, Value)>,
    /// Events recorded on the span, in order.
    pub events: Vec<SpanEvent>,
    /// The span's role in the trace.
    pub kind: SpanKind,
    /// The span's outcome.
    pub status: Status,
    /// Links to other spans.
    pub links: Vec<Link>,
    /// The W3C `tracestate` travelling with the trace, if any.
    pub trace_state: Option<String>,
    /// True when the parent is a span in another process (the context was
    /// extracted from a `traceparent` header).
    pub parent_remote: bool,
}

impl Default for FinishedSpan {
    /// An empty span at [`Level::Info`] — a convenient base for struct-update
    /// syntax in tests and custom exporters.
    fn default() -> Self {
        FinishedSpan {
            name: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_id: None,
            level: Level::Info,
            start_unix_nanos: 0,
            duration_nanos: 0,
            fields: Vec::new(),
            events: Vec::new(),
            kind: SpanKind::Internal,
            status: Status::Unset,
            links: Vec::new(),
            trace_state: None,
            parent_remote: false,
        }
    }
}

// ---- the live span -------------------------------------------------------------

struct SpanData {
    name: String,
    fields: Vec<(String, Value)>,
    events: Vec<SpanEvent>,
    links: Vec<Link>,
    status: Status,
    ended: bool,
}

/// A started span. It owns its own data — nothing about it lives in a
/// thread-local — so it can be updated from any thread and finishes with
/// exactly the fields, events and duration that belong to it.
pub(super) struct SpanShared {
    pub(super) trace_id: TraceId,
    pub(super) span_id: SpanId,
    pub(super) sampled: bool,
    pub(super) trace_state: Option<Arc<str>>,
    parent_id: Option<SpanId>,
    parent_remote: bool,
    level: Level,
    kind: SpanKind,
    start: Instant,
    start_unix_nanos: u128,
    data: Mutex<SpanData>,
}

impl SpanShared {
    fn data(&self) -> MutexGuard<'_, SpanData> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` on the span's data if it is still recording.
    fn update(&self, f: impl FnOnce(&mut SpanData)) {
        if !self.sampled {
            return;
        }
        let mut data = self.data();
        if !data.ended {
            f(&mut data);
        }
    }

    fn record(&self, key: String, value: Value) {
        self.update(|d| {
            if d.fields.len() < MAX_SPAN_FIELDS {
                d.fields.push((key, value));
            }
        });
    }

    fn event(&self, event: SpanEvent) {
        self.update(|d| {
            if d.events.len() < MAX_SPAN_EVENTS {
                d.events.push(event);
            }
        });
    }

    fn set_status(&self, status: Status) {
        self.update(|d| {
            // `Ok` is final, and `Unset` never overwrites an outcome.
            if d.status != Status::Ok && status != Status::Unset {
                d.status = status;
            }
        });
    }

    /// Ends the span (once) and hands it to the exporters.
    fn finish(&self) {
        let duration_nanos = self.start.elapsed().as_nanos();
        let (name, fields, events, links, status) = {
            let mut data = self.data();
            if data.ended {
                return;
            }
            data.ended = true;
            (
                std::mem::take(&mut data.name),
                std::mem::take(&mut data.fields),
                std::mem::take(&mut data.events),
                std::mem::take(&mut data.links),
                std::mem::take(&mut data.status),
            )
        };
        if !self.sampled || !super::has_exporter() {
            return;
        }
        // The lock is released: exporters run without any span lock held.
        super::export(&FinishedSpan {
            name,
            trace_id: self.trace_id.to_string(),
            span_id: self.span_id.to_string(),
            parent_id: self.parent_id.map(|p| p.to_string()),
            level: self.level,
            start_unix_nanos: self.start_unix_nanos,
            duration_nanos,
            fields,
            events,
            kind: self.kind,
            status,
            links,
            trace_state: self.trace_state.as_deref().map(str::to_string),
            parent_remote: self.parent_remote,
        });
    }
}

// ---- span builder --------------------------------------------------------------

enum Parent {
    /// Whatever context is current when the span starts.
    Current,
    Explicit(Context),
    Root,
}

/// A builder for a span. Configure it, then either
///
/// - [`enter`](Span::enter) it — it becomes the current span **on this
///   thread** until the returned [`SpanGuard`] drops (synchronous code), or
/// - [`start`](Span::start) it — you get an [`ActiveSpan`] handle that is
///   `Send`, is not tied to a thread, and ends when dropped, or
/// - hand it to [`instrument`] to cover a future (async code).
pub struct Span {
    name: String,
    level: Level,
    kind: SpanKind,
    fields: Vec<(String, Value)>,
    links: Vec<Link>,
    parent: Parent,
}

impl Span {
    /// Starts building a span with the given name at [`Level::Info`].
    pub fn new(name: impl Into<String>) -> Span {
        Span {
            name: name.into(),
            level: Level::Info,
            kind: SpanKind::Internal,
            fields: Vec::new(),
            links: Vec::new(),
            parent: Parent::Current,
        }
    }

    /// Sets the span's severity level.
    pub fn level(mut self, level: Level) -> Span {
        self.level = level;
        self
    }

    /// Sets the span's [kind](SpanKind) (default [`SpanKind::Internal`]).
    pub fn kind(mut self, kind: SpanKind) -> Span {
        self.kind = kind;
        self
    }

    /// Attaches a typed field. Any type convertible into a
    /// [`ferroly::codec::Value`] works, so `("k", "v")`, `("k", 5)`, and
    /// `("k", true)` are all accepted.
    pub fn field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Span {
        self.fields.push((key.into(), value.into()));
        self
    }

    /// Adds a [`Link`] to another span.
    pub fn link(mut self, link: Link) -> Span {
        self.links.push(link);
        self
    }

    /// Makes the span a child of `parent`, whatever is current when it starts.
    /// This is the explicit-parent API: it needs no ambient state, so it works
    /// on any thread and in any executor. `parent` may be a local span's
    /// context or one [extracted](super::extract) from a request.
    pub fn child_of(mut self, parent: &Context) -> Span {
        self.parent = Parent::Explicit(parent.clone());
        self
    }

    /// Makes the span a child of `parent` when there is one, and leaves the
    /// default (the current context) otherwise — convenient with
    /// [`extract`](super::extract), which returns an `Option`.
    pub fn child_of_opt(self, parent: Option<&Context>) -> Span {
        match parent {
            Some(parent) => self.child_of(parent),
            None => self,
        }
    }

    /// Makes the span the root of a new trace, ignoring the current context.
    pub fn root(mut self) -> Span {
        self.parent = Parent::Root;
        self
    }

    /// Starts the span **without** making it current, and returns a handle
    /// that ends it when dropped.
    ///
    /// The parent is the explicit one ([`child_of`](Span::child_of)), else the
    /// context current on this thread, else none (a new trace). If the span's
    /// level is below the global threshold, or spans are
    /// [suppressed](super::suppress) here, the handle is inert.
    pub fn start(self) -> ActiveSpan {
        if !enabled(self.level) || context::suppressed() {
            let passthrough = match self.parent {
                Parent::Explicit(ctx) => Some(ctx),
                _ => None,
            };
            return ActiveSpan {
                inner: None,
                passthrough,
            };
        }
        let parent = match self.parent {
            Parent::Current => context::current(),
            Parent::Explicit(ctx) => Some(ctx),
            Parent::Root => None,
        };
        let trace_id = parent
            .as_ref()
            .map_or_else(TraceId::random, Context::trace_id);
        let sampled = sampler::should_sample(parent.as_ref(), trace_id);
        // An unsampled span keeps its identity (so the decision propagates)
        // but records nothing.
        let (fields, links) = if sampled {
            let mut fields = self.fields;
            fields.truncate(MAX_SPAN_FIELDS);
            let mut links = self.links;
            links.truncate(MAX_SPAN_LINKS);
            (fields, links)
        } else {
            (Vec::new(), Vec::new())
        };
        ActiveSpan {
            inner: Some(Arc::new(SpanShared {
                trace_id,
                span_id: SpanId::random(),
                sampled,
                trace_state: parent.as_ref().and_then(Context::trace_state_arc),
                parent_id: parent.as_ref().map(Context::span_id),
                parent_remote: parent.as_ref().is_some_and(Context::is_remote),
                level: self.level,
                kind: self.kind,
                start: Instant::now(),
                start_unix_nanos: now_unix_nanos(),
                data: Mutex::new(SpanData {
                    name: self.name,
                    fields,
                    events: Vec::new(),
                    links,
                    status: Status::Unset,
                    ended: false,
                }),
            })),
            passthrough: None,
        }
    }

    /// Enters the span, making it current on this thread and returning an RAII
    /// [`SpanGuard`] that finishes and exports it on drop.
    ///
    /// If the span's level is below the global threshold it is not entered: the
    /// returned guard is inert. When entered, the span's parent is chosen as
    /// for [`start`](Span::start).
    pub fn enter(self) -> SpanGuard {
        let span = self.start();
        let token = span.effective_context().map(context::push_context);
        SpanGuard {
            span,
            token,
            _not_send: PhantomData,
        }
    }

    /// Runs `f` inside the span (entered on this thread) and ends it afterwards.
    pub fn in_scope<R>(self, f: impl FnOnce() -> R) -> R {
        let _guard = self.enter();
        f()
    }
}

// ---- handles -------------------------------------------------------------------

/// A started span that is not tied to any thread. It is `Send + Sync`, can be
/// updated from anywhere, and **ends when dropped** (or when
/// [`end`](ActiveSpan::end) is called).
///
/// ```
/// use ferroly::obs::{Span, SpanKind, Status};
///
/// let span = Span::new("charge card").kind(SpanKind::Client).start();
/// span.record("amount_cents", 4999);
/// span.set_status(Status::error("card declined"));
/// let ctx = span.context(); // hand this to children or to `inject`
/// # let _ = ctx;
/// span.end();
/// ```
#[must_use = "the span ends as soon as the handle is dropped"]
pub struct ActiveSpan {
    inner: Option<Arc<SpanShared>>,
    /// For an inert span with an explicit parent: the parent, so the span is
    /// transparent to its would-be children.
    passthrough: Option<Context>,
}

impl ActiveSpan {
    /// This span's [`Context`] — the value to give children
    /// ([`Span::child_of`]), other tasks ([`with_context`](super::with_context))
    /// or other services ([`inject`](super::inject)). `None` for an inert span.
    pub fn context(&self) -> Option<Context> {
        self.inner.as_ref().map(Context::for_span)
    }

    /// The context children should see: this span's, or for an inert span its
    /// explicit parent's.
    fn effective_context(&self) -> Option<Context> {
        self.context().or_else(|| self.passthrough.clone())
    }

    /// True while the span accepts fields and events: it is live, sampled and
    /// not yet ended. Use it to skip computing expensive field values.
    pub fn is_recording(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|s| s.sampled && !s.data().ended)
    }

    /// Adds a field to this span. A no-op when the span is not recording.
    pub fn record(&self, key: impl Into<String>, value: impl Into<Value>) {
        if let Some(span) = &self.inner {
            if span.sampled {
                span.record(key.into(), value.into());
            }
        }
    }

    /// Records an event on this span. Dropped if `level` is below the global
    /// threshold or the span is not recording.
    pub fn event(&self, level: Level, message: impl Into<String>, fields: Vec<(String, Value)>) {
        if let Some(span) = &self.inner {
            if span.sampled && enabled(level) {
                span.event(SpanEvent {
                    level,
                    message: message.into(),
                    fields,
                    unix_nanos: now_unix_nanos(),
                });
            }
        }
    }

    /// Sets the span's outcome. [`Status::Ok`] is final; [`Status::Unset`]
    /// never replaces a recorded outcome.
    pub fn set_status(&self, status: Status) {
        if let Some(span) = &self.inner {
            span.set_status(status);
        }
    }

    /// Shorthand for `set_status(Status::error(message))`.
    pub fn set_error(&self, message: impl Into<String>) {
        self.set_status(Status::Error(message.into()));
    }

    /// Renames the span — for when the best name is only known later (an HTTP
    /// server span learns its route after routing).
    pub fn set_name(&self, name: impl Into<String>) {
        if let Some(span) = &self.inner {
            let name = name.into();
            span.update(|d| d.name = name);
        }
    }

    /// Adds a [`Link`] to another span.
    pub fn add_link(&self, link: Link) {
        if let Some(span) = &self.inner {
            span.update(|d| {
                if d.links.len() < MAX_SPAN_LINKS {
                    d.links.push(link);
                }
            });
        }
    }

    /// Makes this span current **on this thread** until the returned guard
    /// drops, without ending it. The guard is `!Send`; do not hold it across
    /// an `.await` — use [`instrument`] for futures.
    pub fn enter(&self) -> ContextGuard {
        match self.effective_context() {
            Some(ctx) => ctx.attach(),
            None => ContextGuard::noop(),
        }
    }

    /// Ends the span now and exports it. Dropping the handle does the same.
    pub fn end(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(span) = self.inner.take() {
            span.finish();
        }
    }
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        self.finish();
    }
}

impl std::fmt::Debug for ActiveSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            Some(span) => f
                .debug_struct("ActiveSpan")
                .field("trace_id", &span.trace_id)
                .field("span_id", &span.span_id)
                .field("sampled", &span.sampled)
                .finish(),
            None => f.write_str("ActiveSpan(inert)"),
        }
    }
}

impl From<Span> for ActiveSpan {
    fn from(span: Span) -> ActiveSpan {
        span.start()
    }
}

/// An RAII handle to an entered span. While it is alive its span is the current
/// span on this thread; dropping it finishes the span and exports it.
///
/// The guard dereferences to [`ActiveSpan`], so `record`, `event`,
/// `set_status`, `context` and the rest are available on it.
///
/// Each guard removes **its own** entry from the thread's context stack, so
/// guards may be dropped in any order without disturbing one another, and a
/// span always finishes with its own fields, events and duration. The type is
/// deliberately neither `Send` nor `Sync`: an entered span belongs to the
/// thread that entered it. In async code do not hold a guard across an
/// `.await` unless the future is wrapped by [`instrument`] or
/// [`with_current_context`](super::with_current_context) — see the module
/// documentation.
pub struct SpanGuard {
    span: ActiveSpan,
    token: Option<u64>,
    _not_send: PhantomData<*const ()>,
}

impl std::fmt::Debug for SpanGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SpanGuard").field(&self.span).finish()
    }
}

impl Deref for SpanGuard {
    type Target = ActiveSpan;

    fn deref(&self) -> &ActiveSpan {
        &self.span
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        // Leave the context stack first, so the span is no longer current when
        // the exporters see it.
        if let Some(token) = self.token.take() {
            context::remove(token);
        }
        self.span.finish();
    }
}

// ---- futures -------------------------------------------------------------------

/// A future covered by a span: created by [`instrument`]. The span is current
/// whenever the future is polled and ends when the future completes or is
/// dropped.
#[must_use = "futures do nothing unless polled"]
pub struct Instrumented<F: Future> {
    // Field order matters: the future is dropped (with its context current)
    // before the span ends.
    future: WithContext<F>,
    span: ActiveSpan,
}

impl<F: Future> Instrumented<F> {
    /// The span covering this future — to record fields or a status from
    /// outside the future.
    pub fn span(&self) -> &ActiveSpan {
        &self.span
    }
}

impl<F: Future> Future for Instrumented<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.future).poll(cx);
        if result.is_ready() {
            this.span.finish();
        }
        result
    }
}

/// Covers `future` with `span`.
///
/// The span is started now (so its parent is the context current at the call),
/// is made current **on each poll** — on whichever thread polls — and is ended
/// when the future completes or is dropped. Nothing is held across an `.await`,
/// so this is correct under `join!`, `select!`, and work-stealing runtimes, and
/// it needs no particular executor. `span` may be a [`Span`] builder or an
/// already started [`ActiveSpan`].
///
/// Inside the future, the span is reached through the current context:
/// [`record`], [`set_status`], [`event`] and [`current`](super::current).
///
/// ```
/// use ferroly::obs::{self, Span};
///
/// async fn load_user(id: u64) -> Option<String> {
///     obs::record("user.id", id);
///     None
/// }
///
/// # async fn demo() {
/// let user = obs::instrument(load_user(7), Span::new("load_user")).await;
/// # let _ = user;
/// # }
/// ```
pub fn instrument<F: Future>(future: F, span: impl Into<ActiveSpan>) -> Instrumented<F> {
    let span: ActiveSpan = span.into();
    // An inert span is transparent: the future keeps the surrounding context.
    let (ctx, suppress) = match span.effective_context() {
        Some(ctx) => (Some(ctx), context::suppressed()),
        None => (context::current(), context::suppressed()),
    };
    Instrumented {
        future: WithContext::new(future, ctx, suppress),
        span,
    }
}

/// Method-call forms of [`instrument`] and
/// [`with_current_context`](super::with_current_context) for any future.
///
/// ```
/// use ferroly::obs::{Instrument, Span};
///
/// # async fn work() {}
/// # async fn demo() {
/// work().instrument(Span::new("work")).await;
/// # }
/// ```
pub trait Instrument: Future + Sized {
    /// Covers this future with `span`; see [`instrument`].
    fn instrument(self, span: impl Into<ActiveSpan>) -> Instrumented<Self> {
        instrument(self, span)
    }

    /// Carries the context that is current now into this future; see
    /// [`with_current_context`](super::with_current_context).
    fn with_current_context(self) -> WithContext<Self> {
        context::with_current_context(self)
    }
}

impl<F: Future> Instrument for F {}

// ---- operations on the current span ----------------------------------------------

/// Records a point-in-time event on the current span.
///
/// If a span is current the event is attached to it. If there is no current
/// span and the level passes the filter, the event is emitted as a standalone
/// zero-duration [`FinishedSpan`] named `"event"` carrying the single event, so
/// it still reaches the exporter. Events below the global threshold are
/// dropped.
pub fn event(level: Level, message: impl Into<String>, fields: Vec<(String, Value)>) {
    if !enabled(level) || context::suppressed() {
        return;
    }
    let event = SpanEvent {
        level,
        message: message.into(),
        fields,
        unix_nanos: now_unix_nanos(),
    };
    let current = context::current();
    if let Some(span) = current.as_ref().and_then(|ctx| ctx.span.as_ref()) {
        span.event(event);
        return;
    }
    // No live span: emit a synthetic one so the event is not lost. It joins
    // the current trace when a bare (for example remote) context is attached.
    let trace_id = current
        .as_ref()
        .map_or_else(TraceId::random, Context::trace_id);
    if !sampler::should_sample(current.as_ref(), trace_id) || !super::has_exporter() {
        return;
    }
    super::export(&FinishedSpan {
        name: "event".to_string(),
        trace_id: trace_id.to_string(),
        span_id: SpanId::random().to_string(),
        parent_id: current.as_ref().map(|c| c.span_id().to_string()),
        level,
        start_unix_nanos: event.unix_nanos,
        trace_state: current
            .as_ref()
            .and_then(|c| c.trace_state().map(str::to_string)),
        parent_remote: current.as_ref().is_some_and(Context::is_remote),
        events: vec![event],
        ..FinishedSpan::default()
    });
}

/// Adds a field to the current span. Returns `false` when no span is current.
pub fn record(key: impl Into<String>, value: impl Into<Value>) -> bool {
    match context::current().and_then(|ctx| ctx.span) {
        Some(span) => {
            span.record(key.into(), value.into());
            true
        }
        None => false,
    }
}

/// Sets the outcome of the current span. Returns `false` when no span is
/// current.
pub fn set_status(status: Status) -> bool {
    match context::current().and_then(|ctx| ctx.span) {
        Some(span) => {
            span.set_status(status);
            true
        }
        None => false,
    }
}

/// Marks the current span as failed; shorthand for
/// `set_status(Status::error(message))`.
pub fn set_error(message: impl Into<String>) -> bool {
    set_status(Status::Error(message.into()))
}
