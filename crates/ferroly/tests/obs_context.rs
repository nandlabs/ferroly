//! Regression tests for `ferroly::obs`: async-safe context (I6), id quality
//! (I7), W3C trace-context propagation, sampling, span metadata and the
//! exporter registry.
//!
//! The exporter, level and sampler are process-global, so every test that
//! touches them takes `globals()` first.

#![cfg(feature = "obs")]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context as TaskContext, Poll, Wake, Waker};

use ferroly::codec::Value;
use ferroly::obs::{
    self, ActiveSpan, Context, Exporter, FinishedSpan, Instrument, Level, Link, Sampler, Span,
    SpanId, SpanKind, Status, TraceId,
};

// ---- harness -------------------------------------------------------------------

fn globals() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

type Store = Arc<Mutex<Vec<FinishedSpan>>>;
/// What a future observed as the current span id, step by step.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

struct Collect(Store);

impl Exporter for Collect {
    fn export(&self, span: &FinishedSpan) {
        self.0.lock().unwrap().push(span.clone());
    }
}

/// Resets the global state and installs a collecting exporter.
fn install() -> Store {
    let store: Store = Arc::default();
    obs::set_level(Level::Info);
    obs::set_sampler(Sampler::default());
    obs::set_exporter(Box::new(Collect(store.clone())));
    store
}

fn spans(store: &Store) -> Vec<FinishedSpan> {
    store.lock().unwrap().clone()
}

fn find(spans: &[FinishedSpan], name: &str) -> FinishedSpan {
    let mut matching = spans.iter().filter(|s| s.name == name);
    let found = matching
        .next()
        .unwrap_or_else(|| panic!("no span named {name}"))
        .clone();
    assert!(matching.next().is_none(), "more than one span named {name}");
    found
}

fn field<'a>(span: &'a FinishedSpan, key: &str) -> Option<&'a Value> {
    span.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

/// Polls a future exactly once, like one turn of an executor.
fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::from(Arc::new(NoopWake));
    future.poll(&mut TaskContext::from_waker(&waker))
}

/// Returns `Pending` once, then `Ready` — an `.await` point that suspends.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
}

fn yield_once() -> YieldOnce {
    YieldOnce(false)
}

fn assert_send_sync<T: Send + Sync>() {}

// ---- I7: identifiers -------------------------------------------------------------

#[test]
fn ids_are_random_nonzero_and_unique_across_threads() {
    assert_send_sync::<TraceId>();
    assert_send_sync::<SpanId>();
    let per_thread: Vec<Vec<(TraceId, SpanId)>> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                (0..2000)
                    .map(|_| (TraceId::random(), SpanId::random()))
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();

    let mut traces = HashSet::new();
    let mut span_ids = HashSet::new();
    for ids in &per_thread {
        for pair in ids.windows(2) {
            let a = u64::from_be_bytes(pair[0].1.to_bytes());
            let b = u64::from_be_bytes(pair[1].1.to_bytes());
            assert!(a.abs_diff(b) > 1 << 20, "span ids look sequential");
        }
        for (trace, span) in ids {
            assert_ne!(trace.to_bytes(), [0; 16]);
            assert_ne!(span.to_bytes(), [0; 8]);
            assert!(traces.insert(*trace), "trace id repeated");
            assert!(span_ids.insert(*span), "span id repeated");
            assert_eq!(trace.to_string().len(), 32);
            assert_eq!(span.to_string().len(), 16);
            assert_eq!(TraceId::from_hex(&trace.to_string()), Some(*trace));
        }
    }
    // The first ids of different threads differ too (no shared counter or seed).
    let firsts: HashSet<_> = per_thread.iter().map(|ids| ids[0].1).collect();
    assert_eq!(firsts.len(), 8);
}

#[test]
fn spans_get_random_ids_not_a_counter_from_one() {
    let _g = globals();
    let store = install();
    for i in 0..50 {
        Span::new(format!("ids-{i}")).start().end();
    }
    let spans = spans(&store);
    assert_eq!(spans.len(), 50);
    let mut seen = HashSet::new();
    for s in &spans {
        let span_id = SpanId::from_hex(&s.span_id).expect("valid span id");
        let trace_id = TraceId::from_hex(&s.trace_id).expect("valid trace id");
        let n = u64::from_be_bytes(span_id.to_bytes());
        assert!(n > 1 << 16, "span id {n} is suspiciously small");
        assert!(seen.insert(s.span_id.clone()));
        assert!(
            seen.insert(s.trace_id.clone()),
            "each root has its own trace"
        );
        // Trace ids are not "timestamp + counter": the low half is random too.
        let low = &trace_id.to_bytes()[8..];
        assert!(low.iter().filter(|b| **b != 0).count() >= 3, "{trace_id}");
    }
    let highs: HashSet<_> = spans.iter().map(|s| s.trace_id[..16].to_string()).collect();
    assert_eq!(
        highs.len(),
        50,
        "the high half is not a shared per-process seed"
    );
}

// ---- I6: interleaved futures -------------------------------------------------------

#[test]
fn guards_interleaved_across_await_keep_their_own_span() {
    let _g = globals();
    let store = install();

    // Two bare futures on one thread, each holding a guard across `.await` —
    // the shape `join!` produces. Each must finish as itself.
    async fn branch(name: &'static str) {
        let guard = Span::new(name).enter();
        yield_once().await;
        guard.record("who", name);
        guard.event(Level::Info, name, vec![]);
        yield_once().await;
        drop(guard);
    }
    let mut a = Box::pin(branch("bare-a"));
    let mut b = Box::pin(branch("bare-b"));

    assert!(poll_once(a.as_mut()).is_pending()); // a entered
    assert!(poll_once(b.as_mut()).is_pending()); // b entered above a
    assert!(poll_once(a.as_mut()).is_pending()); // a records while b is on top
    assert!(poll_once(b.as_mut()).is_pending());
    assert!(poll_once(a.as_mut()).is_ready()); // a ends first: out of order
    let a_id = find(&spans(&store), "bare-a").span_id;
    let still_current = obs::current().map(|c| c.span_id().to_string());
    assert!(
        still_current.is_some() && still_current != Some(a_id),
        "b is still current after a ended"
    );
    assert!(poll_once(b.as_mut()).is_ready());
    assert!(obs::current().is_none(), "the stack is empty again");

    let spans = spans(&store);
    assert_eq!(spans.len(), 2);
    for name in ["bare-a", "bare-b"] {
        let s = find(&spans, name);
        assert_eq!(
            s.fields,
            vec![("who".to_string(), Value::Str(name.into()))],
            "{name} has exactly its own field"
        );
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].message, name, "{name} has its own event");
    }
    assert_ne!(spans[0].span_id, spans[1].span_id);
    // The span that ended first was exported first, as itself.
    assert_eq!(spans[0].name, "bare-a");
}

#[test]
fn instrumented_branches_never_see_each_other() {
    let _g = globals();
    let store = install();

    // What each branch observed as "current" at each step.
    let seen: Seen = Arc::default();
    let note = |seen: &Seen, at: &str| {
        seen.lock().unwrap().push((
            at.to_string(),
            obs::current().map(|c| c.span_id().to_string()),
        ));
    };

    let branch = |name: &'static str| {
        let seen = seen.clone();
        async move {
            note(&seen, &format!("{name}:start"));
            // A guard held across an await *inside* an instrumented future.
            let child = Span::new(format!("{name}.child")).enter();
            yield_once().await;
            note(&seen, &format!("{name}:child"));
            let grandchild = Span::new(format!("{name}.grandchild")).start();
            yield_once().await;
            grandchild.end();
            drop(child);
            note(&seen, &format!("{name}:end"));
        }
    };

    let root = Span::new("join-root").enter();
    let root_id = root.context().unwrap().span_id().to_string();
    let mut a = Box::pin(obs::instrument(branch("a"), Span::new("a")));
    let mut b = Box::pin(branch("b").instrument(Span::new("b")));

    for _ in 0..2 {
        assert!(poll_once(a.as_mut()).is_pending());
        // Between polls nothing of the branch is left on the thread.
        assert_eq!(
            obs::current().map(|c| c.span_id().to_string()),
            Some(root_id.clone())
        );
        assert!(poll_once(b.as_mut()).is_pending());
        assert_eq!(
            obs::current().map(|c| c.span_id().to_string()),
            Some(root_id.clone())
        );
    }
    assert!(poll_once(b.as_mut()).is_ready());
    assert!(poll_once(a.as_mut()).is_ready());
    drop(root);
    assert!(obs::current().is_none());

    let spans = spans(&store);
    assert_eq!(spans.len(), 7);
    let root = find(&spans, "join-root");
    for name in ["a", "b"] {
        let top = find(&spans, name);
        let child = find(&spans, &format!("{name}.child"));
        let grandchild = find(&spans, &format!("{name}.grandchild"));
        assert_eq!(top.parent_id.as_deref(), Some(root.span_id.as_str()));
        assert_eq!(child.parent_id.as_deref(), Some(top.span_id.as_str()));
        assert_eq!(
            grandchild.parent_id.as_deref(),
            Some(child.span_id.as_str()),
            "{name}.grandchild must hang off {name}.child, not the other branch"
        );
        for s in [&top, &child, &grandchild] {
            assert_eq!(s.trace_id, root.trace_id);
        }
        // What the branch saw as current: itself, then its child, then itself.
        let seen = seen.lock().unwrap();
        let at = |step: &str| {
            seen.iter()
                .find(|(k, _)| k == &format!("{name}:{step}"))
                .and_then(|(_, v)| v.clone())
        };
        assert_eq!(at("start"), Some(top.span_id.clone()));
        assert_eq!(at("child"), Some(child.span_id.clone()));
        assert_eq!(at("end"), Some(top.span_id.clone()));
    }
}

#[test]
fn a_cancelled_instrumented_future_ends_its_spans_and_cleans_up() {
    let _g = globals();
    let store = install();

    let mut fut = Box::pin(obs::instrument(
        async {
            let _held = Span::new("cancel.held").enter();
            obs::record("progress", "started");
            yield_once().await;
            unreachable!("never polled again");
        },
        Span::new("cancel.outer").kind(SpanKind::Server),
    ));
    assert!(poll_once(fut.as_mut()).is_pending());
    assert!(obs::current().is_none(), "nothing leaks out of the poll");
    assert!(spans(&store).is_empty(), "still running");
    drop(fut); // cancelled

    assert!(obs::current().is_none());
    let spans = spans(&store);
    let outer = find(&spans, "cancel.outer");
    let held = find(&spans, "cancel.held");
    assert_eq!(held.parent_id.as_deref(), Some(outer.span_id.as_str()));
    assert_eq!(outer.kind, SpanKind::Server);
    // `record` inside the future reached the innermost current span.
    assert_eq!(
        field(&held, "progress"),
        Some(&Value::Str("started".into()))
    );
    assert_eq!(spans[0].name, "cancel.held", "inner ends before outer");
}

#[test]
fn context_follows_tasks_across_worker_threads() {
    let _g = globals();
    let store = install();
    const TASKS: usize = 48;
    const STEPS: usize = 4;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = Span::new("mt-root").start();
        let root_ctx = root.context().unwrap();
        let mut handles = Vec::new();
        for task in 0..TASKS {
            let work = async move {
                for step in 0..STEPS {
                    async {
                        tokio::task::yield_now().await;
                        obs::record("task", task as u64);
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        obs::record("step", step as u64);
                    }
                    .instrument(Span::new(format!("mt-step-{task}-{step}")))
                    .await;
                    tokio::task::yield_now().await;
                }
            };
            // The explicit parent crosses the spawn; `instrument` keeps it
            // current on whichever worker polls the task.
            handles.push(tokio::spawn(obs::instrument(
                work,
                Span::new(format!("mt-task-{task}")).child_of(&root_ctx),
            )));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Inheriting the *current* context into a spawned task.
        let inherited = root_ctx
            .wrap(async {
                tokio::spawn(obs::with_current_context(async {
                    tokio::task::yield_now().await;
                    Span::new("mt-inherited").start().end();
                }))
                .await
                .unwrap();
            })
            .await;
        let () = inherited;
        root.end();
    });

    let spans = spans(&store);
    assert_eq!(spans.len(), 1 + TASKS + TASKS * STEPS + 1);
    let root = find(&spans, "mt-root");
    assert_eq!(
        find(&spans, "mt-inherited").parent_id.as_deref(),
        Some(root.span_id.as_str())
    );
    for task in 0..TASKS {
        let task_span = find(&spans, &format!("mt-task-{task}"));
        assert_eq!(task_span.parent_id.as_deref(), Some(root.span_id.as_str()));
        assert_eq!(task_span.trace_id, root.trace_id);
        for step in 0..STEPS {
            let s = find(&spans, &format!("mt-step-{task}-{step}"));
            assert_eq!(
                s.parent_id.as_deref(),
                Some(task_span.span_id.as_str()),
                "step {task}/{step} must belong to its own task"
            );
            assert_eq!(s.trace_id, root.trace_id);
            // Fields recorded after `.await` landed on the right span.
            assert_eq!(field(&s, "task"), Some(&Value::UInt(task as u64)));
            assert_eq!(field(&s, "step"), Some(&Value::UInt(step as u64)));
            assert_eq!(s.fields.len(), 2);
        }
    }
}

// ---- explicit parents ------------------------------------------------------------

#[test]
fn explicit_parent_works_without_any_ambient_state() {
    let _g = globals();
    let store = install();
    assert_send_sync::<Context>();
    assert_send_sync::<ActiveSpan>();

    let parent = Span::new("explicit.parent").start();
    let ctx = parent.context().unwrap();
    assert!(obs::current().is_none(), "start() does not attach");
    assert!(ctx.is_sampled() && !ctx.is_remote());

    // A plain OS thread: no runtime, no thread-local set up by anyone.
    let on_thread = {
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let child = Span::new("explicit.child").child_of(&ctx).start();
            let child_ctx = child.context().unwrap();
            // attach()/scope() make a context current for synchronous code.
            let seen = ctx.scope(|| {
                Span::new("explicit.scoped").start().end();
                obs::current()
            });
            {
                let _attached = child_ctx.attach();
                Span::new("explicit.attached").start().end();
            }
            assert!(obs::current().is_none());
            child.end();
            seen
        })
        .join()
        .unwrap()
    };
    assert_eq!(on_thread, Some(ctx.clone()));

    // An explicit parent beats the current context; `root()` ignores both.
    {
        let _other = Span::new("explicit.other").enter();
        Span::new("explicit.override").child_of(&ctx).start().end();
        Span::new("explicit.root").root().start().end();
        Span::new("explicit.default")
            .child_of_opt(None)
            .start()
            .end();
    }
    // `ActiveSpan::enter` attaches without ending.
    {
        let _in = parent.enter();
        Span::new("explicit.entered").in_scope(|| ());
    }
    assert!(parent.is_recording());
    parent.end();

    let spans = spans(&store);
    let parent = find(&spans, "explicit.parent");
    let child = find(&spans, "explicit.child");
    let other = find(&spans, "explicit.other");
    for name in [
        "explicit.child",
        "explicit.scoped",
        "explicit.override",
        "explicit.entered",
    ] {
        let s = find(&spans, name);
        assert_eq!(
            s.parent_id.as_deref(),
            Some(parent.span_id.as_str()),
            "{name}"
        );
        assert_eq!(s.trace_id, parent.trace_id, "{name}");
    }
    assert_eq!(
        find(&spans, "explicit.attached").parent_id.as_deref(),
        Some(child.span_id.as_str())
    );
    let root = find(&spans, "explicit.root");
    assert_eq!(root.parent_id, None);
    assert_ne!(root.trace_id, other.trace_id);
    assert_eq!(
        find(&spans, "explicit.default").parent_id.as_deref(),
        Some(other.span_id.as_str())
    );
}

// ---- W3C trace context -----------------------------------------------------------

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

#[test]
fn a_remote_parent_is_adopted_and_propagated() {
    let _g = globals();
    let store = install();

    let mut inbound: HashMap<&str, &str> = HashMap::new();
    inbound.insert("traceparent", TRACEPARENT);
    inbound.insert("tracestate", "vendor=abc,other=1");
    let parent = obs::extract(|name| inbound.get(name).copied()).expect("valid header");
    assert_eq!(
        parent,
        Context::extract(|name| inbound.get(name).copied()).unwrap()
    );
    assert!(parent.is_remote() && parent.is_sampled());
    assert_eq!(parent.trace_state(), Some("vendor=abc,other=1"));

    let server = obs::http::server_span("GET", "/users/:id")
        .child_of(&parent)
        .start();
    let server_ctx = server.context().unwrap();
    assert_eq!(
        server_ctx.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_ne!(server_ctx.span_id().to_string(), "00f067aa0ba902b7");
    assert!(!server_ctx.is_remote());

    // Outbound call: the *client* span's context goes on the wire.
    let client = obs::http::client_span("POST", "https://u:p@billing.internal:8443/charge?k=v")
        .child_of(&server_ctx)
        .start();
    let mut outbound: Vec<(String, String)> = Vec::new();
    obs::inject(&client.context().unwrap(), &mut |k, v| {
        outbound.push((k.to_string(), v.to_string()))
    });
    assert_eq!(outbound.len(), 2);
    assert_eq!(outbound[0].0, "traceparent");
    assert_eq!(
        outbound[0].1,
        format!(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-{}-01",
            client.context().unwrap().span_id()
        )
    );
    assert_eq!(
        outbound[1],
        ("tracestate".to_string(), "vendor=abc,other=1".to_string())
    );
    // The downstream service parses what we sent.
    let downstream = Context::from_traceparent(&outbound[0].1).unwrap();
    assert_eq!(downstream.trace_id(), parent.trace_id());

    // inject_current uses whatever is current.
    let mut none = Vec::new();
    assert!(!obs::inject_current(
        &mut |k: &str, _: &str| none.push(k.to_string())
    ));
    {
        let _in = client.enter();
        assert!(obs::inject_current(
            &mut |k: &str, _: &str| none.push(k.to_string())
        ));
    }
    assert_eq!(none, ["traceparent", "tracestate"]);

    obs::http::record_client_status(&client, 502);
    client.end();
    obs::http::record_server_status(&server, 200);
    server.end();

    let spans = spans(&store);
    let server = find(&spans, "GET /users/:id");
    assert_eq!(server.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(server.parent_id.as_deref(), Some("00f067aa0ba902b7"));
    assert!(server.parent_remote);
    assert_eq!(server.trace_state.as_deref(), Some("vendor=abc,other=1"));
    assert_eq!(server.kind, SpanKind::Server);
    assert_eq!(server.status, Status::Unset);
    assert_eq!(
        field(&server, "http.response.status_code"),
        Some(&Value::UInt(200))
    );
    assert_eq!(
        field(&server, "http.route"),
        Some(&Value::Str("/users/:id".into()))
    );

    let client = find(&spans, "POST");
    assert_eq!(client.kind, SpanKind::Client);
    assert_eq!(client.parent_id.as_deref(), Some(server.span_id.as_str()));
    assert!(!client.parent_remote);
    assert_eq!(client.status, Status::error("HTTP 502"));
    assert_eq!(
        field(&client, "url.full"),
        Some(&Value::Str("https://billing.internal:8443/charge".into())),
        "credentials and query are not recorded"
    );
    assert_eq!(
        field(&client, "server.address"),
        Some(&Value::Str("billing.internal".into()))
    );
    assert_eq!(field(&client, "server.port"), Some(&Value::UInt(8443)));
}

#[test]
fn invalid_or_missing_traceparent_starts_a_new_trace() {
    let _g = globals();
    let store = install();
    for bad in [
        None,
        Some(""),
        Some("garbage"),
        Some("00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01"),
        Some("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"),
        Some("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra"),
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7"),
    ] {
        let parent = obs::extract(|name| match name {
            "traceparent" => bad,
            _ => Some("vendor=1"),
        });
        assert!(parent.is_none(), "{bad:?} must be ignored");
        Span::new("fresh")
            .child_of_opt(parent.as_ref())
            .start()
            .end();
    }
    let spans = spans(&store);
    assert_eq!(spans.len(), 9);
    let traces: HashSet<_> = spans.iter().map(|s| s.trace_id.clone()).collect();
    assert_eq!(traces.len(), 9, "each request got a trace of its own");
    assert!(spans.iter().all(|s| s.parent_id.is_none()));
    assert!(spans.iter().all(|s| s.trace_state.is_none()));
    // A hostile tracestate is dropped, the trace itself is kept.
    let ctx = obs::extract(|name| match name {
        "traceparent" => Some(TRACEPARENT),
        _ => Some("a=1\r\nx-injected: yes"),
    })
    .unwrap();
    assert_eq!(ctx.trace_state(), None);
}

// ---- sampling ----------------------------------------------------------------------

#[test]
fn sampling_decisions_are_made_once_and_propagate() {
    let _g = globals();
    let store = install();
    let unsampled_parent =
        Context::from_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
            .unwrap();
    let sampled_parent = Context::from_traceparent(TRACEPARENT).unwrap();

    // Default (parent-based, always-on roots): an upstream "no" is honoured.
    let span = Span::new("s.default.unsampled")
        .child_of(&unsampled_parent)
        .field("k", 1)
        .start();
    let ctx = span
        .context()
        .expect("an unsampled span still has a context");
    assert!(!ctx.is_sampled());
    assert!(!span.is_recording());
    assert_eq!(ctx.trace_id(), unsampled_parent.trace_id());
    assert!(
        ctx.to_traceparent().ends_with("-00"),
        "{}",
        ctx.to_traceparent()
    );
    span.record("ignored", true);
    span.event(Level::Error, "ignored", vec![]);
    // Children inherit the decision, however deep.
    let grandchild = Span::new("s.default.grandchild").child_of(&ctx).start();
    assert!(!grandchild.context().unwrap().is_sampled());
    grandchild.end();
    span.end();
    Span::new("s.default.root").start().end();
    Span::new("s.default.sampled")
        .child_of(&sampled_parent)
        .start()
        .end();

    // Always off: nothing is exported, contexts say "unsampled".
    obs::set_sampler(Sampler::AlwaysOff);
    assert_eq!(obs::sampler(), Sampler::AlwaysOff);
    let off = Span::new("s.off").child_of(&sampled_parent).start();
    assert!(off.context().unwrap().to_traceparent().ends_with("-00"));
    off.end();
    obs::event(Level::Error, "s.off.event", vec![]);

    // Always on ignores the parent's flag.
    obs::set_sampler(Sampler::AlwaysOn);
    Span::new("s.on").child_of(&unsampled_parent).start().end();

    // Parent-based with roots off: only traces someone upstream sampled.
    obs::set_sampler(Sampler::parent_based(Sampler::AlwaysOff));
    Span::new("s.pb.root").start().end();
    let kept = Span::new("s.pb.sampled").child_of(&sampled_parent).start();
    assert!(kept.context().unwrap().to_traceparent().ends_with("-01"));
    kept.end();

    // Ratio: decided by the trace id, so a whole trace agrees.
    obs::set_sampler(Sampler::TraceIdRatio(0.0));
    Span::new("s.ratio.zero").start().end();
    obs::set_sampler(Sampler::TraceIdRatio(1.0));
    Span::new("s.ratio.one").start().end();
    obs::set_sampler(Sampler::TraceIdRatio(0.5));
    for i in 0..200 {
        let root = Span::new(format!("s.half.{i}")).start();
        let ctx = root.context().unwrap();
        let child = Span::new(format!("s.half.{i}.child"))
            .child_of(&ctx)
            .start();
        assert_eq!(
            child.context().unwrap().is_sampled(),
            ctx.is_sampled(),
            "a child agrees with its root"
        );
        child.end();
        root.end();
    }
    obs::set_sampler(Sampler::default());

    let spans = spans(&store);
    let names: HashSet<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    for kept in [
        "s.default.root",
        "s.default.sampled",
        "s.on",
        "s.pb.sampled",
        "s.ratio.one",
    ] {
        assert!(names.contains(kept), "{kept} should be exported");
    }
    for dropped in [
        "s.default.unsampled",
        "s.default.grandchild",
        "s.off",
        "event",
        "s.pb.root",
        "s.ratio.zero",
    ] {
        assert!(!names.contains(dropped), "{dropped} should not be exported");
    }
    let half_roots = (0..200)
        .filter(|i| names.contains(format!("s.half.{i}").as_str()))
        .count();
    let half_children = (0..200)
        .filter(|i| names.contains(format!("s.half.{i}.child").as_str()))
        .count();
    assert_eq!(half_roots, half_children);
    assert!((50..=150).contains(&half_roots), "kept {half_roots} of 200");
}

// ---- span metadata -----------------------------------------------------------------

#[test]
fn status_kind_links_events_and_renaming() {
    let _g = globals();
    let store = install();

    let upstream = Span::new("meta.upstream").kind(SpanKind::Producer).start();
    let upstream_ctx = upstream.context().unwrap().with_trace_state("q=1");
    upstream.end();

    let span = Span::new("meta.placeholder")
        .kind(SpanKind::Consumer)
        .field("batch.size", 3)
        .link(Link::new(&upstream_ctx).field("reason", "batch"))
        .start();
    span.set_name("meta.process");
    span.record("ok", true);
    span.event(
        Level::Warn,
        "retrying",
        vec![("attempt".to_string(), Value::Int(2))],
    );
    span.add_link(Link::new(&upstream_ctx));
    span.set_status(Status::Unset); // never overwrites
    span.set_error("first failure");
    span.set_status(Status::error("second failure")); // the latest error wins
    span.end();

    let ok = Span::new("meta.ok").start();
    ok.set_status(Status::Ok);
    ok.set_error("too late"); // Ok is final
    ok.end();

    // The free functions act on the current span.
    assert!(!obs::record("nobody", 1));
    assert!(!obs::set_error("nobody"));
    {
        let _g = Span::new("meta.current").enter();
        assert!(obs::record("via", "free fn"));
        assert!(obs::set_status(Status::Ok));
        obs::event(Level::Info, "noted", vec![]);
    }
    // Messaging and model-call helpers.
    obs::messaging::publish_span("local", "orders")
        .start()
        .end();
    obs::messaging::process_span("local", "orders")
        .start()
        .end();
    let chat = obs::genai::chat_span("openai", "gpt-x").start();
    obs::genai::record_usage(&chat, 12, 34);
    chat.end();
    let routed = obs::http::server_span("PURGE", "").start();
    obs::http::set_route(&routed, "GET", "/items/:id");
    obs::http::record_server_status(&routed, 503);
    obs::http::record_server_status(&routed, 404);
    routed.end();

    let spans = spans(&store);
    let s = find(&spans, "meta.process");
    assert!(spans.iter().all(|s| s.name != "meta.placeholder"));
    assert_eq!(s.kind, SpanKind::Consumer);
    assert_eq!(s.status, Status::error("second failure"));
    assert_eq!(s.status.as_str(), "error");
    assert_eq!(s.status.message(), Some("second failure"));
    assert_eq!(
        s.fields,
        vec![
            ("batch.size".to_string(), Value::Int(3)),
            ("ok".to_string(), Value::Bool(true)),
        ]
    );
    assert_eq!(s.events.len(), 1);
    assert_eq!(s.events[0].level, Level::Warn);
    assert_eq!(s.events[0].message, "retrying");
    assert_eq!(
        s.events[0].fields,
        vec![("attempt".to_string(), Value::Int(2))]
    );
    assert!(s.events[0].unix_nanos >= s.start_unix_nanos);
    assert_eq!(s.links.len(), 2);
    assert_eq!(s.links[0].span_id, upstream_ctx.span_id().to_string());
    assert_eq!(s.links[0].trace_id, upstream_ctx.trace_id().to_string());
    assert_eq!(s.links[0].trace_state.as_deref(), Some("q=1"));
    assert_eq!(
        s.links[0].fields,
        vec![("reason".to_string(), Value::Str("batch".into()))]
    );
    assert_eq!(s.parent_id, None, "a link is not a parent");

    assert_eq!(find(&spans, "meta.ok").status, Status::Ok);
    let current = find(&spans, "meta.current");
    assert_eq!(current.status, Status::Ok);
    assert_eq!(field(&current, "via"), Some(&Value::Str("free fn".into())));
    assert_eq!(current.events[0].message, "noted");

    let publish = find(&spans, "send orders");
    assert_eq!(publish.kind, SpanKind::Producer);
    assert_eq!(
        field(&publish, "messaging.destination.name"),
        Some(&Value::Str("orders".into()))
    );
    assert_eq!(find(&spans, "process orders").kind, SpanKind::Consumer);
    let chat = find(&spans, "chat gpt-x");
    assert_eq!(chat.kind, SpanKind::Client);
    assert_eq!(
        field(&chat, "gen_ai.usage.output_tokens"),
        Some(&Value::UInt(34))
    );
    let routed = find(&spans, "GET /items/:id");
    assert_eq!(
        field(&routed, "http.request.method"),
        Some(&Value::Str("_OTHER".into()))
    );
    assert_eq!(routed.status, Status::error("HTTP 503"));
}

#[test]
fn filtered_and_suppressed_spans_are_transparent() {
    let _g = globals();
    let store = install(); // level Info

    let root = Span::new("t.root").enter();
    {
        // Below the threshold: inert, and invisible to its children.
        let quiet = Span::new("t.debug").level(Level::Debug).enter();
        assert!(quiet.context().is_none());
        assert!(!quiet.is_recording());
        quiet.record("ignored", 1);
        Span::new("t.child").start().end();
    }
    // An instrumented future with an inert span keeps the outer context.
    let mut fut = Box::pin(obs::instrument(
        async { Span::new("t.in-future").start().end() },
        Span::new("t.debug-future").level(Level::Debug),
    ));
    assert!(poll_once(fut.as_mut()).is_ready());
    // Suppressed scopes record nothing at all.
    let mut quiet = Box::pin(obs::suppress(async {
        Span::new("t.suppressed").start().end();
        let _g = Span::new("t.suppressed.entered").enter();
        Span::new("t.suppressed.child").start().end();
        obs::event(Level::Error, "t.suppressed.event", vec![]);
    }));
    assert!(poll_once(quiet.as_mut()).is_ready());
    drop(root);

    let spans = spans(&store);
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["t.child", "t.in-future", "t.root"]);
    let root = find(&spans, "t.root");
    assert_eq!(
        find(&spans, "t.child").parent_id.as_deref(),
        Some(root.span_id.as_str())
    );
    assert_eq!(
        find(&spans, "t.in-future").parent_id.as_deref(),
        Some(root.span_id.as_str())
    );
    assert!(root.events.is_empty());
}

#[test]
fn span_collections_are_bounded() {
    let _g = globals();
    let store = install();
    let span = Span::new("bounded").start();
    for i in 0..(obs::MAX_SPAN_EVENTS + 50) {
        span.event(Level::Info, "e", vec![]);
        span.record(format!("f{i}"), i as u64);
    }
    span.end();
    let s = find(&spans(&store), "bounded");
    assert_eq!(s.events.len(), obs::MAX_SPAN_EVENTS);
    assert_eq!(s.fields.len(), obs::MAX_SPAN_FIELDS);
}

// ---- exporter registry ---------------------------------------------------------------

#[test]
fn exporters_are_called_without_the_registry_lock() {
    let _g = globals();
    obs::set_level(Level::Info);
    obs::set_sampler(Sampler::default());

    /// Re-enters the registry and starts spans from inside `export`.
    struct Reentrant {
        seen: Store,
        flushes: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
    }
    impl Exporter for Reentrant {
        fn export(&self, span: &FinishedSpan) {
            self.seen.lock().unwrap().push(span.clone());
            // Each of these would deadlock if `export` ran under the lock.
            obs::flush();
            obs::add_exporter(Box::new(Collect(Arc::default())));
            // A span finished inside an exporter is not fed back into it.
            Span::new("from-exporter").start().end();
        }
        fn flush(&self) {
            self.flushes.fetch_add(1, Ordering::Relaxed);
        }
        fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::Relaxed);
        }
    }

    let (seen, second): (Store, Store) = (Arc::default(), Arc::default());
    let (flushes, shutdowns) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    obs::set_exporter(Box::new(Reentrant {
        seen: seen.clone(),
        flushes: flushes.clone(),
        shutdowns: shutdowns.clone(),
    }));
    obs::add_exporter(Box::new(Collect(second.clone())));

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        Span::new("reg.one").start().end();
        Span::new("reg.two").start().end();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("exporting must not deadlock");
    worker.join().unwrap();

    let names = |store: &Store| -> Vec<String> {
        store
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect()
    };
    assert_eq!(names(&seen), ["reg.one", "reg.two"]);
    assert_eq!(names(&second), ["reg.one", "reg.two"], "fan-out to both");
    assert_eq!(flushes.load(Ordering::Relaxed), 2, "flush() from export");

    obs::shutdown();
    assert_eq!(shutdowns.load(Ordering::Relaxed), 1);
    Span::new("reg.after-shutdown").start().end();
    assert_eq!(names(&seen).len(), 2, "no exporter is installed any more");
    obs::clear_exporters();
}
