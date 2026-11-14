//! Log ↔ trace correlation: `ferroly::log`'s trace-context provider reads the
//! current `ferroly::obs` span, including across `.await` points.

#![cfg(all(feature = "log", feature = "obs"))]

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Wake, Waker};

use ferroly::codec::{json, Value};
use ferroly::log::{self, Logger};
use ferroly::obs::{self, Span};

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
    fn lines(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| json::from_str(l).expect("a JSON log line"))
            .collect()
    }
}

// The only test in this binary, so it owns the process-global provider list.
#[test]
fn log_records_carry_the_current_trace_and_span_id() {
    let ids = |ctx: &obs::Context| {
        vec![
            (
                "trace_id".to_string(),
                Value::Str(ctx.trace_id().to_string()),
            ),
            ("span_id".to_string(), Value::Str(ctx.span_id().to_string())),
        ]
    };

    // The field source itself.
    assert!(log::trace_context_fields().is_empty(), "no span, no fields");
    let span = Span::new("log.span").enter();
    let ctx = span.context().unwrap();
    assert_eq!(log::trace_context_fields(), ids(&ctx));
    drop(span);
    assert!(log::trace_context_fields().is_empty());

    // Installed as a provider it decorates every record logged inside a span,
    // on either side of an `.await` in an instrumented future.
    log::add_trace_context_provider();
    log::add_trace_context_provider(); // idempotent
    let buf = Buf::default();
    let logger = Logger::json().to_writer(buf.clone());

    logger.info("outside", &[]);
    let fut_span = Span::new("log.future").start();
    let fut_ctx = fut_span.context().unwrap();
    let mut fut = Box::pin(obs::instrument(
        {
            let logger = logger.clone();
            async move {
                logger.info("before await", &[]);
                YieldOnce(false).await;
                logger.info("after await", &[]);
                let _child = Span::new("log.child").enter();
                logger.info("in child", &[]);
            }
        },
        fut_span,
    ));
    assert!(poll_once(fut.as_mut()).is_pending());
    logger.info("while suspended", &[]);
    assert!(poll_once(fut.as_mut()).is_ready());
    log::clear_context_providers();

    let lines = buf.lines();
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let by_msg = |msg: &str| {
        lines
            .iter()
            .find(|v| text(v, "msg").as_deref() == Some(msg))
            .unwrap_or_else(|| panic!("no record {msg:?}"))
    };
    for msg in ["outside", "while suspended"] {
        assert!(by_msg(msg).get("trace_id").is_none(), "{msg}");
        assert!(by_msg(msg).get("span_id").is_none(), "{msg}");
    }
    let trace = Some(fut_ctx.trace_id().to_string());
    for msg in ["before await", "after await"] {
        assert_eq!(text(by_msg(msg), "trace_id"), trace, "{msg}");
        assert_eq!(
            text(by_msg(msg), "span_id"),
            Some(fut_ctx.span_id().to_string()),
            "{msg}"
        );
    }
    // A nested span: same trace, its own span id.
    let child = by_msg("in child");
    assert_eq!(text(child, "trace_id"), trace);
    let child_span = text(child, "span_id").unwrap();
    assert_eq!(child_span.len(), 16);
    assert_ne!(child_span, fut_ctx.span_id().to_string());
}
