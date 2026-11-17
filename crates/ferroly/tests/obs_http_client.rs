//! `obs::TraceInterceptor`: client spans and `traceparent` injection for a raw
//! `http::Client`.
//!
//! The exporter is process-global, so every test takes `globals()` first.

#![cfg(all(feature = "obs", feature = "http"))]

use std::sync::{Arc, Mutex, OnceLock};

use tokio::net::TcpListener;

use ferroly::codec::Value;
use ferroly::http::{
    serve, BoxFuture, Client, HttpHandler, HttpResponse, Interceptor, Method, Request, StatusCode,
};
use ferroly::obs::{
    self, Context, Exporter, FinishedSpan, Level, Sampler, Span, SpanKind, Status, TraceInterceptor,
};

/// Runs the tests one at a time (an async lock: it is held across `.await`).
fn globals() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

type Store = Arc<Mutex<Vec<FinishedSpan>>>;

struct Collect(Store);

impl Exporter for Collect {
    fn export(&self, span: &FinishedSpan) {
        self.0.lock().unwrap().push(span.clone());
    }
}

fn install() -> Store {
    let store: Store = Arc::default();
    obs::set_level(Level::Info);
    obs::set_sampler(Sampler::default());
    obs::set_exporter(Box::new(Collect(store.clone())));
    store
}

fn field<'a>(span: &'a FinishedSpan, key: &str) -> Option<&'a Value> {
    span.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Records the `traceparent` of every request; `/missing` answers 404 and
/// `/redirect` redirects to `/ok`.
struct Server(Arc<Mutex<Vec<Option<String>>>>);

impl HttpHandler for Server {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        let traceparent = req.headers.get("traceparent").map(str::to_string);
        self.0.lock().unwrap().push(traceparent);
        Box::pin(async move {
            match req.uri.path.as_str() {
                "/missing" => HttpResponse::text(StatusCode::NOT_FOUND, "no"),
                "/redirect" => HttpResponse::new(StatusCode(302)).header("location", "/ok"),
                _ => HttpResponse::text(StatusCode::OK, "ok"),
            }
        })
    }
}

async fn server() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let handler = Arc::new(Server(seen.clone()));
    tokio::spawn(serve(listener, handler, std::future::pending::<()>()));
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

fn get(url: &str) -> Request {
    Request::builder(Method::Get, url).unwrap().build()
}

fn client_spans(store: &Store) -> Vec<FinishedSpan> {
    store
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.kind == SpanKind::Client)
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_request_gets_a_client_span_that_parents_the_server_side() {
    let _g = globals().lock().await;
    let store = install();
    let (base, seen) = server().await;
    let client = Client::builder()
        .interceptor(TraceInterceptor::new())
        .build()
        .unwrap();

    // Inside a parent span, so the client span is its child.
    let parent = Span::new("job").start();
    let parent_ctx = parent.context().unwrap();
    let resp = obs::with_context(parent_ctx.clone(), client.send(get(&format!("{base}/ok"))))
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "ok");
    parent.end();

    let header = seen.lock().unwrap()[0].clone().expect("traceparent sent");
    let sent = Context::from_traceparent(&header).unwrap();
    let spans = client_spans(&store);
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.name, "GET");
    assert_eq!(span.span_id, sent.span_id().to_string());
    assert_eq!(span.trace_id, parent_ctx.trace_id().to_string());
    assert_eq!(span.parent_id, Some(parent_ctx.span_id().to_string()));
    assert_eq!(
        field(span, "http.response.status_code"),
        Some(&Value::UInt(200))
    );
    assert_eq!(span.status, Status::Unset);
    assert_eq!(
        field(span, "url.full").and_then(Value::as_str),
        Some(format!("{base}/ok").as_str())
    );
}

#[tokio::test]
async fn error_statuses_and_failures_mark_the_span() {
    let _g = globals().lock().await;
    let store = install();
    let (base, _seen) = server().await;
    let tracer = Arc::new(TraceInterceptor::new());
    struct Shared(Arc<TraceInterceptor>);
    impl Interceptor for Shared {
        fn on_request(&self, req: &mut Request) -> Result<(), ferroly::http::HttpError> {
            self.0.on_request(req)
        }
        fn on_response(&self, req: &Request, s: StatusCode, h: &ferroly::http::HeaderMap) {
            self.0.on_response(req, s, h)
        }
        fn on_error(&self, req: &Request, e: &ferroly::http::HttpError) {
            self.0.on_error(req, e)
        }
    }
    let client = Client::builder()
        .interceptor(Shared(tracer.clone()))
        .build()
        .unwrap();

    let resp = client.send(get(&format!("{base}/missing"))).await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // Nothing listens on this port: a connection error.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}/x", closed.local_addr().unwrap());
    drop(closed);
    assert!(client.send(get(&dead)).await.is_err());

    let spans = client_spans(&store);
    assert_eq!(spans.len(), 2);
    assert_eq!(
        field(&spans[0], "http.response.status_code"),
        Some(&Value::UInt(404))
    );
    assert!(spans[0].status.is_error());
    assert!(spans[1].status.is_error());
    assert!(field(&spans[1], "http.response.status_code").is_none());
    assert_eq!(tracer.open_spans(), 0);
}

#[tokio::test]
async fn a_caller_supplied_traceparent_is_left_alone() {
    let _g = globals().lock().await;
    let store = install();
    let (base, seen) = server().await;
    let client = Client::builder()
        .interceptor(TraceInterceptor::new())
        .build()
        .unwrap();
    let own = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let req = Request::builder(Method::Get, &format!("{base}/ok"))
        .unwrap()
        .header("traceparent", own)
        .build();
    client.send(req).await.unwrap().text().await.unwrap();
    assert_eq!(seen.lock().unwrap()[0].as_deref(), Some(own));
    assert!(client_spans(&store).is_empty());
}

#[tokio::test]
async fn each_redirect_hop_is_its_own_span() {
    let _g = globals().lock().await;
    let store = install();
    let (base, seen) = server().await;
    let client = Client::builder()
        .max_redirects(3)
        .interceptor(TraceInterceptor::new())
        .build()
        .unwrap();
    let resp = client.send(get(&format!("{base}/redirect"))).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "ok");

    let spans = client_spans(&store);
    assert_eq!(spans.len(), 2, "{spans:?}");
    let seen = seen.lock().unwrap();
    for (span, header) in spans.iter().zip(seen.iter()) {
        let sent = Context::from_traceparent(header.as_deref().unwrap()).unwrap();
        assert_eq!(span.span_id, sent.span_id().to_string());
    }
    assert_ne!(spans[0].span_id, spans[1].span_id);
    assert_eq!(
        field(&spans[0], "http.response.status_code"),
        Some(&Value::UInt(302))
    );
}

#[test]
fn abandoned_requests_cannot_leak_spans() {
    let _g = globals().blocking_lock();
    let store = install();
    let tracer = TraceInterceptor::with_capacity(2);
    let mut requests: Vec<Request> = (0..3).map(|i| get(&format!("http://h/{i}"))).collect();
    for req in &mut requests {
        tracer.on_request(req).unwrap();
    }
    // The third request evicted (and ended) the first: marked, counted, and
    // not reported as an error, since the request may still succeed.
    assert_eq!(tracer.open_spans(), 2);
    assert_eq!(tracer.evicted_spans(), 1);
    let ended = client_spans(&store);
    assert_eq!(ended.len(), 1);
    assert!(!ended[0].status.is_error(), "{:?}", ended[0].status);
    assert_eq!(
        field(&ended[0], ferroly::obs::EVICTED_ATTRIBUTE),
        Some(&Value::Bool(true))
    );
    // The others still end normally.
    tracer.on_response(&requests[1], StatusCode::OK, &Default::default());
    tracer.on_response(&requests[2], StatusCode::OK, &Default::default());
    assert_eq!(tracer.open_spans(), 0);
    assert_eq!(client_spans(&store).len(), 3);

    // A redirect hop of the evicted request still carries our traceparent:
    // it is recognised as ours (a new span, a fresh header), not left alone
    // as if the caller had set it.
    let mut hop = requests[0].clone();
    let before = hop.headers.get("traceparent").map(str::to_string);
    tracer.on_request(&mut hop).unwrap();
    assert_eq!(tracer.open_spans(), 1);
    assert_ne!(hop.headers.get("traceparent").map(str::to_string), before);
}

#[test]
fn without_a_recorded_span_the_current_context_is_still_injected() {
    let _g = globals().blocking_lock();
    let _store = install();
    obs::set_level(Level::Error); // client spans (Info) are filtered out
    let tracer = TraceInterceptor::new();
    let ctx = Context::from_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        .unwrap();
    let mut req = get("http://h/x");
    ctx.scope(|| tracer.on_request(&mut req)).unwrap();
    assert_eq!(
        req.headers.get("traceparent"),
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
    );
    assert_eq!(tracer.open_spans(), 0);
    obs::set_level(Level::Info);
}
