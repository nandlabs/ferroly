//! [`TraceInterceptor`]: client spans and `traceparent` injection for a raw
//! [`http::Client`](ferroly::http::Client).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use ferroly::http::{HeaderMap, HttpError, Interceptor, Request, StatusCode};

use super::{inject_current, ActiveSpan, Context, TRACEPARENT_HEADER};

/// The default number of requests whose spans are tracked at once.
const DEFAULT_CAPACITY: usize = 1024;

/// An [`Interceptor`] that traces the requests a raw
/// [`http::Client`](ferroly::http::Client) sends.
///
/// For each request (each redirect hop, when redirects are followed) it
/// starts a [`SpanKind::Client`](super::SpanKind::Client) span with
/// [`obs::http::client_span`](super::http::client_span) — a child of the
/// current context — and injects that span's `traceparent` (and
/// `tracestate`) into the request, so the server's span becomes its child.
/// The span ends when the response head arrives, with
/// `http.response.status_code` recorded (a `4xx`/`5xx` marks it as an error;
/// see [`record_client_status`](super::http::record_client_status)), or when
/// the request fails, with the error as its status.
///
/// - A request that already carries a `traceparent` set by the caller is
///   left alone: no span, no header change.
/// - When no span is recorded (spans are filtered out by level, or
///   [suppressed](super::suppress)), the current context, if any, is still
///   injected.
/// - The span measures the time to the response **head**; reading the body
///   is not included.
/// - At most [`capacity`](Self::with_capacity) spans are open at once (1024
///   by default). A request that never completes (its future was dropped,
///   or a later interceptor failed it before it was sent) cannot leak: when
///   the limit is reached, the oldest open span is ended early, with the
///   attribute `ferroly.span.evicted = true` and its status left unset (it
///   may still succeed; its outcome is just not recorded).
///   [`evicted_spans`](Self::evicted_spans) counts them. Size the capacity
///   above the most requests one client has in flight at once: past it,
///   spans of live requests are evicted too.
///
/// ```no_run
/// use ferroly::http::{Client, Method, Request};
/// use ferroly::obs::TraceInterceptor;
///
/// # async fn ex() -> Result<(), ferroly::http::HttpError> {
/// let client = Client::builder().interceptor(TraceInterceptor::new()).build()?;
/// let req = Request::builder(Method::Get, "http://127.0.0.1:8080/health")?.build();
/// let resp = client.send(req).await?; // traced as a client span
/// # let _ = resp;
/// # Ok(())
/// # }
/// ```
pub struct TraceInterceptor {
    capacity: usize,
    state: Mutex<State>,
    evicted: AtomicU64,
}

/// The attribute set on a span ended early because the open-span limit was
/// reached.
pub const EVICTED_ATTRIBUTE: &str = "ferroly.span.evicted";

#[derive(Default)]
struct State {
    /// Open spans by the span id injected into their request.
    open: HashMap<String, ActiveSpan>,
    /// Insertion order of `open` (may hold ids already closed).
    order: VecDeque<String>,
    /// Span ids this interceptor injected recently (the last
    /// `2 × capacity`, so an id outlives its span's eviction from `open`): a
    /// redirect hop copies the previous hop's `traceparent`, which must not
    /// count as the caller's own. An id still in `open` counts as ours too.
    ours: HashSet<String>,
    ours_order: VecDeque<String>,
}

impl State {
    fn remember_ours(&mut self, id: &str, capacity: usize) {
        if self.ours.insert(id.to_string()) {
            self.ours_order.push_back(id.to_string());
        }
        while self.ours_order.len() > capacity.saturating_mul(2) {
            if let Some(old) = self.ours_order.pop_front() {
                self.ours.remove(&old);
            }
        }
    }
}

impl Default for TraceInterceptor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TraceInterceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceInterceptor")
            .field("capacity", &self.capacity)
            .field("open", &self.lock().open.len())
            .finish()
    }
}

impl TraceInterceptor {
    /// An interceptor tracking up to 1024 open spans.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// An interceptor tracking up to `capacity` open spans (at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(State::default()),
            evicted: AtomicU64::new(0),
        }
    }

    /// The number of spans currently open (requests sent whose response head
    /// has not arrived yet).
    pub fn open_spans(&self) -> usize {
        self.lock().open.len()
    }

    /// How many spans were ended early because the open-span limit was
    /// reached, since the interceptor was created. A count that keeps
    /// growing while requests are not being dropped means the capacity is
    /// below the number of requests in flight.
    pub fn evicted_spans(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The span id in the request's `traceparent`, if it has a valid one.
    fn injected_id(req: &Request) -> Option<String> {
        let header = req.headers.get(TRACEPARENT_HEADER)?;
        Context::from_traceparent(header)
            .ok()
            .map(|ctx| ctx.span_id().to_string())
    }

    /// Removes the span `req` carries, if this interceptor opened it.
    fn take(&self, req: &Request) -> Option<ActiveSpan> {
        let id = Self::injected_id(req)?;
        self.lock().open.remove(&id)
    }
}

impl Interceptor for TraceInterceptor {
    fn on_request(&self, req: &mut Request) -> Result<(), HttpError> {
        if req.headers.contains(TRACEPARENT_HEADER) {
            let ours = Self::injected_id(req).is_some_and(|id| {
                let state = self.lock();
                state.ours.contains(&id) || state.open.contains_key(&id)
            });
            if !ours {
                // The caller propagates the trace itself.
                return Ok(());
            }
            req.headers.remove(TRACEPARENT_HEADER);
            req.headers.remove(super::TRACESTATE_HEADER);
        }
        let span = super::http::client_span(req.method.as_str(), &req.uri.absolute()).start();
        let Some(ctx) = span.context() else {
            inject_current(&mut |name, value| req.headers.set(name, value));
            return Ok(());
        };
        super::inject(&ctx, &mut |name, value| req.headers.set(name, value));
        let id = ctx.span_id().to_string();
        let evicted = {
            let mut state = self.lock();
            state.remember_ours(&id, self.capacity);
            let mut evicted = Vec::new();
            while state.open.len() >= self.capacity {
                let Some(old) = state.order.pop_front() else {
                    break;
                };
                if let Some(span) = state.open.remove(&old) {
                    evicted.push(span);
                }
            }
            state.open.insert(id.clone(), span);
            state.order.push_back(id);
            if state.order.len() > self.capacity.saturating_mul(2) {
                let State { open, order, .. } = &mut *state;
                order.retain(|id| open.contains_key(id));
            }
            evicted
        };
        // Ended outside the lock: ending exports the span. The request may
        // still be in flight, so its outcome is unknown: no status.
        if !evicted.is_empty() {
            self.evicted
                .fetch_add(evicted.len() as u64, Ordering::Relaxed);
        }
        for span in evicted {
            span.record(EVICTED_ATTRIBUTE, true);
            span.end();
        }
        Ok(())
    }

    fn on_response(&self, req: &Request, status: StatusCode, _headers: &HeaderMap) {
        if let Some(span) = self.take(req) {
            super::http::record_client_status(&span, status.as_u16());
            span.end();
        }
    }

    fn on_error(&self, req: &Request, error: &HttpError) {
        if let Some(span) = self.take(req) {
            span.set_error(error.to_string());
            span.end();
        }
    }
}
