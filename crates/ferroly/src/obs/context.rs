//! The trace context: what identifies "the current span" and how it travels —
//! across threads and `.await` points inside a process, and across services as
//! a W3C `traceparent` header.

use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use super::id::{SpanId, TraceId};
use super::span::SpanShared;

/// The W3C Trace Context header carrying the trace id, parent span id and flags.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// The W3C Trace Context header carrying vendor-specific state.
pub const TRACESTATE_HEADER: &str = "tracestate";

/// The longest `tracestate` value that is carried along; longer values are
/// dropped rather than truncated mid-entry.
const MAX_TRACESTATE_LEN: usize = 1024;

/// Why a `traceparent` header was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceParentError {
    /// Not of the form `version-traceid-spanid-flags`, or version `00` with
    /// trailing data.
    Malformed,
    /// The version is not two lowercase hex digits, or is the forbidden `ff`.
    InvalidVersion,
    /// The trace id is not 32 lowercase hex digits, or is all zeroes.
    InvalidTraceId,
    /// The parent span id is not 16 lowercase hex digits, or is all zeroes.
    InvalidSpanId,
    /// The flags are not two lowercase hex digits.
    InvalidFlags,
}

impl fmt::Display for TraceParentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TraceParentError::Malformed => "malformed traceparent",
            TraceParentError::InvalidVersion => "invalid traceparent version",
            TraceParentError::InvalidTraceId => "invalid trace id in traceparent",
            TraceParentError::InvalidSpanId => "invalid parent id in traceparent",
            TraceParentError::InvalidFlags => "invalid trace flags in traceparent",
        })
    }
}

impl std::error::Error for TraceParentError {}

/// Two lowercase hex digits → a byte.
fn hex_byte(text: &str) -> Option<u8> {
    let b = text.as_bytes();
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    if b.len() != 2 {
        return None;
    }
    Some((nibble(b[0])? << 4) | nibble(b[1])?)
}

/// The identity of a span — trace id, span id, sampled flag and `tracestate` —
/// as a plain value that is `Send + Sync + Clone`.
///
/// A context names a **parent**: hand it to
/// [`Span::child_of`](super::Span::child_of), make it current with
/// [`attach`](Self::attach) / [`scope`](Self::scope) /
/// [`with_context`], or send it to another service with
/// [`to_traceparent`](Self::to_traceparent) / [`inject`].
///
/// ```
/// use ferroly::obs::Context;
///
/// let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
/// let ctx = Context::from_traceparent(header).unwrap();
/// assert!(ctx.is_sampled() && ctx.is_remote());
/// assert_eq!(ctx.to_traceparent(), header);
/// ```
#[derive(Clone)]
pub struct Context {
    trace_id: TraceId,
    span_id: SpanId,
    sampled: bool,
    remote: bool,
    trace_state: Option<Arc<str>>,
    /// The live span behind this context, when it is a local one.
    pub(super) span: Option<Arc<SpanShared>>,
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("trace_id", &self.trace_id)
            .field("span_id", &self.span_id)
            .field("sampled", &self.sampled)
            .field("remote", &self.remote)
            .field("trace_state", &self.trace_state)
            .finish()
    }
}

impl PartialEq for Context {
    /// Two contexts are equal when they name the same span with the same
    /// flags and `tracestate`.
    fn eq(&self, other: &Self) -> bool {
        self.trace_id == other.trace_id
            && self.span_id == other.span_id
            && self.sampled == other.sampled
            && self.remote == other.remote
            && self.trace_state == other.trace_state
    }
}

impl Eq for Context {}

impl Context {
    /// A context for a span in this process.
    pub fn new(trace_id: TraceId, span_id: SpanId, sampled: bool) -> Context {
        Context {
            trace_id,
            span_id,
            sampled,
            remote: false,
            trace_state: None,
            span: None,
        }
    }

    /// A context received from another process.
    pub fn new_remote(trace_id: TraceId, span_id: SpanId, sampled: bool) -> Context {
        Context {
            remote: true,
            ..Context::new(trace_id, span_id, sampled)
        }
    }

    pub(super) fn for_span(span: &Arc<SpanShared>) -> Context {
        Context {
            trace_id: span.trace_id,
            span_id: span.span_id,
            sampled: span.sampled,
            remote: false,
            trace_state: span.trace_state.clone(),
            span: Some(Arc::clone(span)),
        }
    }

    /// The trace this context belongs to.
    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    /// The span this context names (the parent of any child created from it).
    pub fn span_id(&self) -> SpanId {
        self.span_id
    }

    /// Whether the trace is sampled (recorded and exported).
    pub fn is_sampled(&self) -> bool {
        self.sampled
    }

    /// Whether the context came from another process.
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// The W3C `tracestate` value travelling with the trace, if any.
    pub fn trace_state(&self) -> Option<&str> {
        self.trace_state.as_deref()
    }

    pub(super) fn trace_state_arc(&self) -> Option<Arc<str>> {
        self.trace_state.clone()
    }

    /// Attaches a `tracestate` value, which is passed along unchanged. A value
    /// that is empty, longer than 1024 bytes, or contains control characters
    /// is discarded.
    pub fn with_trace_state(mut self, state: &str) -> Context {
        let state = state.trim_matches([' ', '\t']);
        self.trace_state = if state.is_empty()
            || state.len() > MAX_TRACESTATE_LEN
            || state.chars().any(char::is_control)
        {
            None
        } else {
            Some(Arc::from(state))
        };
        self
    }

    /// Parses a W3C `traceparent` header value.
    ///
    /// Validation is strict: `version-traceid-spanid-flags` with two, 32, 16
    /// and two **lowercase** hex digits, non-zero ids, and version `ff`
    /// rejected. Version `00` must have exactly those four fields. As the
    /// specification asks, a *higher* version is accepted when its first four
    /// fields are valid (later fields are ignored). Surrounding spaces and tabs
    /// are ignored. The result [is remote](Self::is_remote).
    pub fn from_traceparent(header: &str) -> Result<Context, TraceParentError> {
        let header = header.trim_matches([' ', '\t']);
        let mut parts = header.split('-');
        let mut next = || parts.next().ok_or(TraceParentError::Malformed);
        let version = hex_byte(next()?).ok_or(TraceParentError::InvalidVersion)?;
        if version == 0xff {
            return Err(TraceParentError::InvalidVersion);
        }
        let trace_id = TraceId::from_hex(next()?).ok_or(TraceParentError::InvalidTraceId)?;
        let span_id = SpanId::from_hex(next()?).ok_or(TraceParentError::InvalidSpanId)?;
        let flags = hex_byte(next()?).ok_or(TraceParentError::InvalidFlags)?;
        if version == 0 && parts.next().is_some() {
            return Err(TraceParentError::Malformed);
        }
        Ok(Context::new_remote(trace_id, span_id, flags & 0x01 == 0x01))
    }

    /// Formats this context as a W3C `traceparent` header value (version `00`).
    /// Only the sampled flag is propagated.
    pub fn to_traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id,
            self.span_id,
            u8::from(self.sampled)
        )
    }

    /// Reads a context from a carrier — see the free function [`extract`].
    pub fn extract<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Context> {
        extract(get)
    }

    /// Writes this context into a carrier — see the free function [`inject`].
    pub fn inject(&self, set: &mut impl FnMut(&str, &str)) {
        inject(self, set);
    }

    /// Makes this context current **on this thread** until the returned guard
    /// drops. Spans started meanwhile (without an explicit parent) become its
    /// children.
    ///
    /// The guard is `!Send` and must not be held across an `.await`; in async
    /// code use [`with_context`] or [`instrument`](super::instrument), which
    /// re-attach on every poll.
    pub fn attach(&self) -> ContextGuard {
        ContextGuard {
            token: Some(push(Some(self.clone()), false)),
            _not_send: PhantomData,
        }
    }

    /// Runs `f` with this context current on this thread.
    pub fn scope<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.attach();
        f()
    }

    /// Wraps `future` so this context is current whenever it is polled; see
    /// [`with_context`].
    pub fn wrap<F: Future>(&self, future: F) -> WithContext<F> {
        with_context(self.clone(), future)
    }
}

// ---- carriers ------------------------------------------------------------------

/// Reads a [`Context`] from any header-like carrier.
///
/// `get` is called with the lowercase header names
/// [`TRACEPARENT_HEADER`] and [`TRACESTATE_HEADER`] and returns the value, if
/// present. `None` is returned when there is no `traceparent` or it is invalid
/// (an invalid header is ignored, as the specification requires — the request
/// then starts a new trace). The function knows nothing about HTTP types, so it
/// works with any map:
///
/// ```
/// use std::collections::HashMap;
/// use ferroly::obs;
///
/// let mut headers = HashMap::new();
/// headers.insert("traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
/// headers.insert("tracestate", "vendor=value");
///
/// let parent = obs::extract(|name| headers.get(name).copied()).unwrap();
/// assert_eq!(parent.trace_state(), Some("vendor=value"));
/// ```
pub fn extract<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Context> {
    let ctx = Context::from_traceparent(get(TRACEPARENT_HEADER)?).ok()?;
    Some(match get(TRACESTATE_HEADER) {
        Some(state) => ctx.with_trace_state(state),
        None => ctx,
    })
}

/// Writes `ctx` into any header-like carrier: calls `set` with
/// [`TRACEPARENT_HEADER`] and, when the context has one, [`TRACESTATE_HEADER`].
///
/// ```
/// use ferroly::obs::{self, Span};
///
/// let span = Span::new("call backend").start();
/// let mut headers: Vec<(String, String)> = Vec::new();
/// if let Some(ctx) = span.context() {
///     obs::inject(&ctx, &mut |name, value| headers.push((name.into(), value.into())));
///     assert_eq!(headers[0].0, "traceparent");
/// }
/// ```
pub fn inject(ctx: &Context, set: &mut impl FnMut(&str, &str)) {
    set(TRACEPARENT_HEADER, &ctx.to_traceparent());
    if let Some(state) = ctx.trace_state() {
        set(TRACESTATE_HEADER, state);
    }
}

/// [`inject`]s the [current](current) context, if there is one. Returns whether
/// anything was written.
pub fn inject_current(set: &mut impl FnMut(&str, &str)) -> bool {
    match current() {
        Some(ctx) => {
            inject(&ctx, set);
            true
        }
        None => false,
    }
}

// ---- the per-thread context stack ------------------------------------------------

/// One attachment. Every guard and every future wrapper owns exactly one, and
/// removes it **by token** — never by position — so out-of-order drops and
/// interleaved futures cannot disturb each other's entries.
pub(super) struct Entry {
    token: u64,
    /// `None` marks a scope that has no context at all.
    ctx: Option<Context>,
    /// Spans started below this entry are not recorded.
    suppress: bool,
}

thread_local! {
    static STACK: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` on this thread's stack. Returns `None` once the thread-local has
/// been destroyed (a span touched from another thread-local's destructor while
/// the thread exits), in which case there is simply no current context.
fn with_stack<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> Option<R> {
    STACK.try_with(|stack| f(&mut stack.borrow_mut())).ok()
}

fn next_token() -> u64 {
    // Process-wide, because entries move between threads with their futures.
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn push(ctx: Option<Context>, suppress: bool) -> u64 {
    let token = next_token();
    with_stack(|stack| {
        // Suppression is inherited by everything attached inside it.
        let suppress = suppress || stack.last().is_some_and(|e| e.suppress);
        stack.push(Entry {
            token,
            ctx,
            suppress,
        });
    });
    token
}

/// Makes `ctx` current on this thread; undone by [`remove`].
pub(super) fn push_context(ctx: Context) -> u64 {
    push(Some(ctx), false)
}

/// Removes the entry with `token`, wherever in the stack it is.
pub(super) fn remove(token: u64) {
    let removed = with_stack(|stack| {
        stack
            .iter()
            .rposition(|e| e.token == token)
            .map(|pos| stack.remove(pos))
    });
    // Dropped outside the borrow.
    drop(removed);
}

/// The context that is current on this thread right now: the innermost entered
/// span, attached context, or instrumented future being polled. `None` when
/// nothing is active.
pub fn current() -> Option<Context> {
    with_stack(|stack| stack.last().and_then(|e| e.ctx.clone())).flatten()
}

/// True when the innermost scope asked for spans not to be recorded.
pub(super) fn suppressed() -> bool {
    with_stack(|stack| stack.last().is_some_and(|e| e.suppress)).unwrap_or(false)
}

/// An RAII guard from [`Context::attach`] (or
/// [`ActiveSpan::enter`](super::ActiveSpan::enter)): the context stays current
/// on this thread until the guard drops. `!Send`.
#[must_use = "the context is detached as soon as the guard is dropped"]
pub struct ContextGuard {
    token: Option<u64>,
    _not_send: PhantomData<*const ()>,
}

impl ContextGuard {
    /// A guard that attached nothing.
    pub(super) fn noop() -> ContextGuard {
        ContextGuard {
            token: None,
            _not_send: PhantomData,
        }
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            remove(token);
        }
    }
}

// ---- contexts that follow a future -----------------------------------------------

/// The part of the context stack that belongs to one future: its own context
/// plus whatever guards the future has entered and is still holding. It is
/// installed on the polling thread for the duration of each poll and taken
/// back afterwards, so it travels with the future across `.await` points and
/// worker threads instead of staying behind on a thread.
pub(super) struct Scope {
    ctx: Option<Context>,
    suppress: bool,
    saved: Vec<Entry>,
}

impl Scope {
    pub(super) fn new(ctx: Option<Context>, suppress: bool) -> Scope {
        Scope {
            ctx,
            suppress,
            saved: Vec::new(),
        }
    }

    /// Installs the scope on this thread until the returned guard drops.
    fn enter(&mut self) -> ScopeGuard<'_> {
        let token = next_token();
        with_stack(|stack| {
            // Suppression also comes from whatever is polling this future.
            let suppress = self.suppress || stack.last().is_some_and(|e| e.suppress);
            stack.push(Entry {
                token,
                ctx: self.ctx.clone(),
                suppress,
            });
            stack.append(&mut self.saved);
        });
        ScopeGuard { scope: self, token }
    }
}

struct ScopeGuard<'a> {
    scope: &'a mut Scope,
    token: u64,
}

impl Drop for ScopeGuard<'_> {
    fn drop(&mut self) {
        // Take back our entry and everything entered above it during the poll.
        let token = self.token;
        let scope = &mut *self.scope;
        let own = with_stack(|stack| {
            let pos = stack.iter().rposition(|e| e.token == token)?;
            scope.saved = stack.split_off(pos + 1);
            stack.pop()
        });
        drop(own);
    }
}

/// A future that carries a trace context with it: the context is current on
/// whichever thread polls the future, for exactly the duration of each poll.
/// Created by [`with_context`], [`with_current_context`], [`Context::wrap`]
/// and [`suppress`].
#[must_use = "futures do nothing unless polled"]
pub struct WithContext<F: Future> {
    /// Boxed so it can be polled without `unsafe` pin projection.
    inner: Option<Pin<Box<F>>>,
    scope: Scope,
}

impl<F: Future> WithContext<F> {
    pub(super) fn new(future: F, ctx: Option<Context>, suppress: bool) -> Self {
        WithContext {
            inner: Some(Box::pin(future)),
            scope: Scope::new(ctx, suppress),
        }
    }
}

impl<F: Future> Future for WithContext<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            panic!("`WithContext` polled after completion");
        };
        let guard = this.scope.enter();
        let result = inner.as_mut().poll(cx);
        if result.is_ready() {
            // Drop the finished future while its context is still current.
            this.inner = None;
        }
        drop(guard);
        if result.is_ready() {
            this.scope.saved.clear();
        }
        result
    }
}

impl<F: Future> Drop for WithContext<F> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            // Cancelled: drop the future (and any guards it holds) with its
            // context current, so those guards find their own entries.
            let guard = self.scope.enter();
            self.inner = None;
            drop(guard);
        }
    }
}

/// Wraps `future` so that `ctx` is the current context whenever the future is
/// polled — on any thread, across every `.await`. Use it to carry a context
/// into a spawned task:
///
/// ```
/// use ferroly::obs::{self, Span};
///
/// # async fn work() {}
/// # fn spawn<F: std::future::Future + Send + 'static>(_f: F) {}
/// let request = Span::new("request").start();
/// if let Some(ctx) = request.context() {
///     spawn(obs::with_context(ctx, async {
///         // spans started here are children of `request`
///         work().await;
///     }));
/// }
/// ```
pub fn with_context<F: Future>(ctx: Context, future: F) -> WithContext<F> {
    WithContext::new(future, Some(ctx), suppressed())
}

/// Wraps `future` so that the context that is current **now** (when this
/// function is called) is current whenever the future is polled. This is how a
/// context is inherited by a spawned task or by one branch of a `join!`:
/// `spawn(obs::with_current_context(fut))`.
///
/// Wrapping also isolates the future: guards it holds across an `.await` stay
/// with it and are invisible to other futures polled on the same thread.
pub fn with_current_context<F: Future>(future: F) -> WithContext<F> {
    WithContext::new(future, current(), suppressed())
}

/// Wraps `future` so that spans started while it runs are neither recorded nor
/// exported. Exporters use this around their own I/O so that instrumented
/// clients do not generate telemetry about sending telemetry.
pub fn suppress<F: Future>(future: F) -> WithContext<F> {
    WithContext::new(future, None, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn traceparent_round_trip() {
        let ctx = Context::from_traceparent(HEADER).unwrap();
        assert_eq!(
            ctx.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(ctx.span_id().to_string(), "00f067aa0ba902b7");
        assert!(ctx.is_sampled());
        assert!(ctx.is_remote());
        assert_eq!(ctx.to_traceparent(), HEADER);

        let unsampled =
            Context::from_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
                .unwrap();
        assert!(!unsampled.is_sampled());
        assert!(unsampled.to_traceparent().ends_with("-00"));
        // Unknown flag bits are read but not propagated.
        let flags =
            Context::from_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-ff")
                .unwrap();
        assert!(flags.is_sampled());
        assert!(flags.to_traceparent().ends_with("-01"));
        assert_eq!(Context::from_traceparent(&format!("  {HEADER}\t")), Ok(ctx));
    }

    #[test]
    fn traceparent_validation_is_strict() {
        use TraceParentError::*;
        let cases = [
            ("", InvalidVersion),
            ("00", Malformed),
            ("garbage", InvalidVersion),
            (
                "0-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                InvalidVersion,
            ),
            (
                "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                InvalidVersion,
            ),
            (
                "0G-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                InvalidVersion,
            ),
            (
                "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
                InvalidTraceId,
            ),
            (
                "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
                InvalidTraceId,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
                InvalidTraceId,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
                InvalidSpanId,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00F067AA0BA902B7-01",
                InvalidSpanId,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
                Malformed,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
                InvalidFlags,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0x",
                InvalidFlags,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
                Malformed,
            ),
            (
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-",
                Malformed,
            ),
        ];
        for (header, expected) in cases {
            assert_eq!(
                Context::from_traceparent(header),
                Err(expected),
                "{header:?}"
            );
        }
        // A future version may carry more fields; the known four are used.
        let future =
            Context::from_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x")
                .unwrap();
        assert_eq!(future.to_traceparent(), HEADER);
    }

    #[test]
    fn trace_state_is_carried_or_discarded_whole() {
        let ctx = Context::from_traceparent(HEADER).unwrap();
        assert_eq!(ctx.trace_state(), None);
        let with = ctx.clone().with_trace_state(" a=1,b=2 ");
        assert_eq!(with.trace_state(), Some("a=1,b=2"));
        assert_eq!(ctx.clone().with_trace_state("").trace_state(), None);
        assert_eq!(
            ctx.clone()
                .with_trace_state("a=1\r\nx-evil: 1")
                .trace_state(),
            None
        );
        assert_eq!(
            ctx.clone()
                .with_trace_state(&"a".repeat(MAX_TRACESTATE_LEN + 1))
                .trace_state(),
            None
        );
    }

    #[test]
    fn guards_remove_their_own_entry_in_any_order() {
        let a = Context::from_traceparent(HEADER).unwrap();
        let b = Context::new(TraceId::random(), SpanId::random(), true);
        assert!(current().is_none());
        let ga = a.attach();
        let gb = b.attach();
        assert_eq!(current(), Some(b.clone()));
        drop(ga); // out of order: `b` must stay current
        assert_eq!(current(), Some(b.clone()));
        drop(gb);
        assert!(current().is_none());
        assert_eq!(a.scope(current), Some(a.clone()));
        assert!(current().is_none());
    }
}
