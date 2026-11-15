#![cfg(feature = "grpc")]
//! gRPC between ferroly's own server and client over h2c: the four call
//! shapes, metadata and status both ways, deadlines on both sides,
//! cancellation, limits, protocol rejections, panics, concurrency, the
//! health service and the non-gRPC fallback.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferroly::grpc::health::{
    HealthCheckRequest, HealthCheckResponse, HealthReporter, ServingStatus, CHECK_PATH, WATCH_PATH,
};
use ferroly::grpc::{
    Channel, Code, GrpcServer, Metadata, Request, Response, Status, StreamSender, Streaming,
};
use ferroly::http::{
    serve_with_config, BoxFuture, Client, HeaderMap, Http2Config, HttpHandler, HttpResponse,
    Method, ServerConfig, StatusCode,
};
use tokio::net::TcpListener;

#[derive(ferroly::proto::Message, Clone, PartialEq, Debug, Default)]
struct Msg {
    #[proto(tag = 1)]
    text: String,
    #[proto(tag = 2)]
    n: u32,
    #[proto(tag = 3)]
    blob: Vec<u8>,
}

fn msg(text: &str, n: u32) -> Msg {
    Msg {
        text: text.into(),
        n,
        blob: Vec::new(),
    }
}

/// Set when the value is dropped: proves a handler (or its task) stopped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Flags {
    hang_dropped: Arc<AtomicBool>,
    producer_stopped: Arc<AtomicBool>,
    panics: Arc<AtomicUsize>,
}

fn test_server(flags: &Flags) -> GrpcServer {
    let hang = flags.hang_dropped.clone();
    let producer = flags.producer_stopped.clone();
    let panics = flags.panics.clone();
    GrpcServer::new()
        .max_decoding_message_size(64 * 1024)
        .on_handler_panic(move |path, message| {
            assert_eq!(path, "/test.Svc/Echo");
            assert!(message.contains("boom"));
            panics.fetch_add(1, Ordering::SeqCst);
        })
        .unary("/test.Svc/Echo", |req: Request<Msg>| async move {
            let md = req.metadata().clone();
            let port = req.peer_addr().map(|a| a.port()).unwrap_or(0);
            let timeout = req.timeout();
            let m = req.into_inner();
            match m.text.as_str() {
                "fail" => {
                    let mut status = Status::invalid_argument("bad ✓ 100%\nvalue")
                        .with_details(vec![1, 2, 3, 0xff]);
                    status.metadata_mut().insert("x-why", "because").unwrap();
                    return Err(status);
                }
                "panic" => panic!("boom"),
                "sleep" => tokio::time::sleep(Duration::from_millis(u64::from(m.n))).await,
                "big" => {
                    return Ok(Response::new(Msg {
                        blob: vec![7; m.n as usize],
                        ..Msg::default()
                    }))
                }
                _ => {}
            }
            let mut resp = Response::new(Msg {
                text: m.text,
                n: m.n + 1,
                blob: m.blob,
            });
            if let Some(v) = md.get("x-in") {
                resp.metadata_mut().insert("x-echo", v).unwrap();
            }
            if let Some(b) = md.get_bin("x-in-bin") {
                resp.metadata_mut().insert_bin("x-echo-bin", b).unwrap();
            }
            resp.metadata_mut()
                .insert("x-peer-port", port.to_string())
                .unwrap();
            if let Some(t) = timeout {
                resp.metadata_mut()
                    .insert("x-timeout-ms", t.as_millis().to_string())
                    .unwrap();
            }
            resp.trailers_mut().insert("x-trailer", "t").unwrap();
            Ok(resp)
        })
        .unary("/test.Svc/Hang", move |_req: Request<Msg>| {
            let flag = DropFlag(hang.clone());
            async move {
                let _flag = flag;
                std::future::pending::<()>().await;
                Ok(Response::new(Msg::default()))
            }
        })
        .server_streaming("/test.Svc/Count", move |req: Request<Msg>| {
            let producer = producer.clone();
            async move {
                let m = req.into_inner();
                if m.text == "refuse" {
                    return Err(Status::permission_denied("not you"));
                }
                let (tx, stream) = Streaming::channel(2);
                tokio::spawn(async move {
                    let _flag = DropFlag(producer);
                    for i in 0..m.n {
                        if m.text == "fail-mid" && i == 2 {
                            let _ = tx.fail(Status::data_loss("lost it")).await;
                            return;
                        }
                        if m.text == "slow" {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        if m.text == "drop-mid" && i == 1 {
                            return; // dropped without finishing
                        }
                        if tx.send(msg("item", i)).await.is_err() {
                            return;
                        }
                    }
                    if m.text == "forever" {
                        tx.closed().await;
                        return;
                    }
                    let _ = tx.finish().await;
                });
                let mut resp = Response::new(stream);
                resp.trailers_mut()
                    .insert("x-count", m.n.to_string())
                    .unwrap();
                Ok(resp)
            }
        })
        .client_streaming("/test.Svc/Sum", |req: Request<Streaming<Msg>>| async move {
            let mut input = req.into_inner();
            let mut total = 0;
            let mut text = String::new();
            while let Some(m) = input.message().await? {
                total += m.n;
                text.push_str(&m.text);
            }
            Ok(Response::new(msg(&text, total)))
        })
        .bidi_streaming(
            "/test.Svc/Chat",
            |req: Request<Streaming<Msg>>| async move {
                let mut input = req.into_inner();
                let (tx, stream) = Streaming::channel(2);
                tokio::spawn(async move {
                    let mut seen = 0;
                    loop {
                        match input.message().await {
                            Ok(Some(m)) => {
                                seen += 1;
                                if tx
                                    .send(msg(&format!("echo:{}", m.text), m.n))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(status) => {
                                let _ = tx.fail(status).await;
                                return;
                            }
                        }
                    }
                    let _ = tx.send(msg("done", seen)).await;
                    let _ = tx.finish().await;
                });
                Ok(Response::new(stream))
            },
        )
}

fn h2_config() -> ServerConfig {
    ServerConfig {
        http2: Some(Http2Config::default()),
        ..ServerConfig::default()
    }
}

async fn start(handler: Arc<dyn HttpHandler>) -> SocketAddr {
    start_with(handler, h2_config()).await
}

async fn start_with(handler: Arc<dyn HttpHandler>, config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_with_config(
        listener,
        handler,
        std::future::pending(),
        config,
    ));
    addr
}

async fn setup() -> (Channel, Flags) {
    let flags = Flags::default();
    let addr = start(Arc::new(test_server(&flags))).await;
    (Channel::new(&format!("http://{addr}")).unwrap(), flags)
}

async fn wait_for(flag: &AtomicBool) {
    let start = Instant::now();
    while !flag.load(Ordering::SeqCst) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn unary_round_trip_with_metadata_and_trailers() {
    let (channel, _) = setup().await;
    let mut req = Request::new(msg("hello", 41));
    req.metadata_mut().insert("x-in", "value 1").unwrap();
    req.metadata_mut()
        .insert_bin("x-in-bin", vec![0, 1, 2, 0xfe, 0xff])
        .unwrap();
    let resp = channel
        .unary::<Msg, Msg>("/test.Svc/Echo", req)
        .await
        .unwrap();
    assert_eq!(resp.metadata().get("x-echo"), Some("value 1"));
    assert_eq!(
        resp.metadata().get_bin("x-echo-bin"),
        Some(&[0u8, 1, 2, 0xfe, 0xff][..])
    );
    assert_eq!(resp.trailers().get("x-trailer"), Some("t"));
    assert!(!resp.trailers().contains("grpc-status"));
    assert_eq!(resp.into_inner(), msg("hello", 42));
}

#[tokio::test]
async fn error_status_with_message_details_and_trailers() {
    let (channel, _) = setup().await;
    let err = channel
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("fail", 0)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(err.message(), "bad ✓ 100%\nvalue");
    assert_eq!(err.details(), Some(&[1u8, 2, 3, 0xff][..]));
    assert_eq!(err.metadata().get("x-why"), Some("because"));
    let as_error: ferroly::Error = err.into();
    assert!(matches!(as_error, ferroly::Error::Grpc(_)));
}

#[tokio::test]
async fn server_streaming_all_messages_then_trailers() {
    let (channel, _) = setup().await;
    let resp = channel
        .server_streaming::<Msg, Msg>("/test.Svc/Count", Request::new(msg("go", 5)))
        .await
        .unwrap();
    let mut stream = resp.into_inner();
    let mut got = Vec::new();
    while let Some(m) = stream.message().await.unwrap() {
        got.push(m.n);
    }
    assert_eq!(got, [0, 1, 2, 3, 4]);
    assert_eq!(stream.trailers().unwrap().get("x-count"), Some("5"));
    assert_eq!(stream.message().await.unwrap(), None);
}

#[tokio::test]
async fn server_streaming_errors() {
    let (channel, _) = setup().await;
    // An error before the first message: trailers-only, failing the call.
    let err = channel
        .server_streaming::<Msg, Msg>("/test.Svc/Count", Request::new(msg("refuse", 3)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
    assert_eq!(err.message(), "not you");

    // An error mid-stream: the messages, then the status.
    let mut stream = channel
        .server_streaming::<Msg, Msg>("/test.Svc/Count", Request::new(msg("fail-mid", 5)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.message().await.unwrap().unwrap().n, 0);
    assert_eq!(stream.message().await.unwrap().unwrap().n, 1);
    let err = stream.message().await.unwrap_err();
    assert_eq!(err.code(), Code::DataLoss);
    assert_eq!(stream.message().await.unwrap(), None);

    // A producer that drops its sender without finishing: INTERNAL, never
    // a clean end.
    let mut stream = channel
        .server_streaming::<Msg, Msg>("/test.Svc/Count", Request::new(msg("drop-mid", 5)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.message().await.unwrap().unwrap().n, 0);
    assert_eq!(stream.message().await.unwrap_err().code(), Code::Internal);
}

#[tokio::test]
async fn client_streaming_from_a_channel_and_from_messages() {
    let (channel, _) = setup().await;
    let (tx, stream) = Streaming::channel(1);
    tokio::spawn(async move {
        for (i, s) in ["a", "b", "c"].into_iter().enumerate() {
            tx.send(msg(s, i as u32 + 1)).await.unwrap();
        }
        tx.finish().await.unwrap();
    });
    let resp = channel
        .client_streaming::<Msg, Msg>("/test.Svc/Sum", Request::new(stream))
        .await
        .unwrap();
    assert_eq!(resp.into_inner(), msg("abc", 6));

    let resp = channel
        .client_streaming::<Msg, Msg>(
            "/test.Svc/Sum",
            Request::new(Streaming::from_messages(vec![msg("x", 10), msg("y", 5)])),
        )
        .await
        .unwrap();
    assert_eq!(resp.into_inner(), msg("xy", 15));

    // An empty stream is a valid client stream.
    let resp = channel
        .client_streaming::<Msg, Msg>("/test.Svc/Sum", Request::new(Streaming::from_messages([])))
        .await
        .unwrap();
    assert_eq!(resp.into_inner(), msg("", 0));
}

#[tokio::test]
async fn client_stream_failure_cancels_the_call_with_its_status() {
    let (channel, _) = setup().await;
    let (tx, stream) = Streaming::channel(1);
    tokio::spawn(async move {
        tx.send(msg("a", 1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.fail(Status::aborted("client gave up")).await;
    });
    let err = channel
        .client_streaming::<Msg, Msg>("/test.Svc/Sum", Request::new(stream))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Aborted, "{err}");
    assert_eq!(err.message(), "client gave up");
}

#[tokio::test]
async fn bidi_ping_pong() {
    let (channel, _) = setup().await;
    let (tx, stream): (StreamSender<Msg>, _) = Streaming::channel(1);
    tx.send(msg("one", 1)).await.unwrap();
    let resp = channel
        .bidi_streaming::<Msg, Msg>("/test.Svc/Chat", Request::new(stream))
        .await
        .unwrap();
    let mut replies = resp.into_inner();
    // Each reply arrives before the next request is sent: truly interleaved.
    assert_eq!(replies.message().await.unwrap().unwrap().text, "echo:one");
    tx.send(msg("two", 2)).await.unwrap();
    assert_eq!(replies.message().await.unwrap().unwrap().text, "echo:two");
    tx.send(msg("three", 3)).await.unwrap();
    assert_eq!(replies.message().await.unwrap().unwrap().text, "echo:three");
    tx.finish().await.unwrap();
    assert_eq!(replies.message().await.unwrap().unwrap(), msg("done", 3));
    assert_eq!(replies.message().await.unwrap(), None);
    assert!(replies.trailers().is_some());
}

#[tokio::test]
async fn unknown_service_and_method_are_unimplemented() {
    let (channel, _) = setup().await;
    let err = channel
        .unary::<Msg, Msg>("/test.Svc/Missing", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
    assert!(err.message().contains("unknown method Missing"), "{err}");
    let err = channel
        .unary::<Msg, Msg>("/other.Svc/Echo", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
    assert!(err.message().contains("unknown service other.Svc"), "{err}");
    let err = channel
        .server_streaming::<Msg, Msg>("/x/y/z", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
    let err = channel
        .unary::<Msg, Msg>("no-slash", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn handler_panic_is_internal_and_reported() {
    let (channel, flags) = setup().await;
    let err = channel
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("panic", 0)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Internal);
    assert!(!err.message().contains("boom"), "panic text must not leak");
    assert_eq!(flags.panics.load(Ordering::SeqCst), 1);
    // The server keeps serving.
    let ok = channel
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("after", 1)))
        .await
        .unwrap();
    assert_eq!(ok.get_ref().n, 2);
}

#[tokio::test]
async fn oversized_messages_both_directions() {
    let (channel, _) = setup().await;
    // Request over the server's 64 KiB limit.
    let big = Msg {
        blob: vec![1; 70 * 1024],
        ..Msg::default()
    };
    let err = channel
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(big.clone()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{err}");

    // Response over the client's limit.
    let addr = channel.target().to_string();
    let small = Channel::builder(&addr)
        .unwrap()
        .max_decoding_message_size(1000)
        .build()
        .unwrap();
    let err = small
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("big", 2000)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{err}");
    let ok = small
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("big", 900)))
        .await
        .unwrap();
    assert_eq!(ok.get_ref().blob.len(), 900);

    // Request over the client's own encoding limit: nothing is sent.
    let tight = Channel::builder(&addr)
        .unwrap()
        .max_encoding_message_size(10)
        .build()
        .unwrap();
    let err = tight
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(big))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted);
}

#[tokio::test]
async fn client_deadline_is_sent_and_enforced() {
    let (channel, flags) = setup().await;
    // The timeout travels as grpc-timeout.
    let resp = channel
        .unary::<Msg, Msg>(
            "/test.Svc/Echo",
            Request::new(msg("x", 0)).with_timeout(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    let ms: u64 = resp
        .metadata()
        .get("x-timeout-ms")
        .unwrap()
        .parse()
        .unwrap();
    assert!((29_000..=30_000).contains(&ms), "{ms}");

    // A handler that never answers: the call fails at the deadline, and the
    // server drops the handler.
    let started = Instant::now();
    let err = channel
        .unary::<Msg, Msg>(
            "/test.Svc/Hang",
            Request::new(Msg::default()).with_timeout(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::DeadlineExceeded, "{err}");
    assert!(started.elapsed() < Duration::from_secs(3));
    wait_for(&flags.hang_dropped).await;

    // A channel-wide default timeout applies to streams too.
    let addr = channel.target().to_string();
    let quick = Channel::builder(&addr)
        .unwrap()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let mut stream = quick
        .server_streaming::<Msg, Msg>("/test.Svc/Count", Request::new(msg("slow", 1000)))
        .await
        .unwrap()
        .into_inner();
    let mut code = None;
    for _ in 0..1000 {
        match stream.message().await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) => {
                code = Some(e.code());
                break;
            }
        }
    }
    assert_eq!(code, Some(Code::DeadlineExceeded));
    wait_for(&flags.producer_stopped).await;
}

/// A server that never answers: only the client can enforce the deadline.
struct Silent(Arc<std::sync::Mutex<Option<String>>>);

impl HttpHandler for Silent {
    fn handle(&self, req: ferroly::http::Request) -> BoxFuture<'_, HttpResponse> {
        *self.0.lock().unwrap() = req.headers.get("grpc-timeout").map(str::to_string);
        Box::pin(async {
            std::future::pending::<()>().await;
            HttpResponse::ok()
        })
    }
}

#[tokio::test]
async fn client_enforces_its_deadline_locally() {
    let seen = Arc::new(std::sync::Mutex::new(None));
    let addr = start(Arc::new(Silent(seen.clone()))).await;
    let channel = Channel::new(&format!("http://{addr}")).unwrap();
    let started = Instant::now();
    let err = channel
        .unary::<Msg, Msg>(
            "/a.B/C",
            Request::new(Msg::default()).with_timeout(Duration::from_millis(250)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::DeadlineExceeded);
    assert!(started.elapsed() >= Duration::from_millis(240));
    assert!(started.elapsed() < Duration::from_secs(3));
    let header = seen.lock().unwrap().clone().unwrap();
    assert_eq!(
        ferroly::grpc::parse_timeout(&header).map(|d| d.as_millis() <= 250),
        Some(true),
        "{header}"
    );
}

#[tokio::test]
async fn server_enforces_grpc_timeout() {
    let (channel, flags) = setup().await;
    let addr = channel.target().trim_start_matches("http://").to_string();
    // Raw request: no client-side deadline, only the header.
    let resp = raw(&addr, "/test.Svc/Hang", |h| {
        h.set("grpc-timeout", "150m");
    })
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers().get("grpc-status"), Some("4"));
    wait_for(&flags.hang_dropped).await;
}

#[tokio::test]
async fn cancellation_by_dropping_reaches_the_server() {
    let (channel, flags) = setup().await;
    // Unary: drop the call future.
    let c = channel.clone();
    let call = tokio::spawn(async move {
        c.unary::<Msg, Msg>("/test.Svc/Hang", Request::new(Msg::default()))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    call.abort();
    wait_for(&flags.hang_dropped).await;

    // Server streaming: drop the response stream; the producer's next send
    // (or its `closed()`) sees it.
    let stream = channel
        .server_streaming::<Msg, Msg>("/test.Svc/Count", Request::new(msg("forever", 1)))
        .await
        .unwrap()
        .into_inner();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(stream);
    wait_for(&flags.producer_stopped).await;
}

#[tokio::test]
async fn concurrent_calls_share_one_connection() {
    let (channel, _) = setup().await;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..40u32 {
        let c = channel.clone();
        tasks.spawn(async move {
            let resp = c
                .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("c", i)))
                .await
                .unwrap();
            assert_eq!(resp.get_ref().n, i + 1);
            resp.metadata().get("x-peer-port").unwrap().to_string()
        });
    }
    let mut ports = std::collections::HashSet::new();
    while let Some(port) = tasks.join_next().await {
        ports.insert(port.unwrap());
    }
    assert_eq!(ports.len(), 1, "{ports:?}");
}

/// An h2c request built by hand, for what the [`Channel`] would never send.
async fn raw(addr: &str, path: &str, edit: impl FnOnce(&mut HeaderMap)) -> ferroly::http::Response {
    raw_with(
        addr,
        Method::Post,
        path,
        frame_of(&Msg::default()),
        true,
        edit,
    )
    .await
}

fn frame_of(m: &Msg) -> Vec<u8> {
    ferroly::grpc::encode_frame(m, usize::MAX).unwrap()
}

async fn raw_with(
    addr: &str,
    method: Method,
    path: &str,
    body: Vec<u8>,
    h2: bool,
    edit: impl FnOnce(&mut HeaderMap),
) -> ferroly::http::Response {
    let client = Client::builder().http2_prior_knowledge(h2).build().unwrap();
    let mut req = ferroly::http::Request::builder(method, &format!("http://{addr}{path}"))
        .unwrap()
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(body)
        .build();
    edit(&mut req.headers);
    client.send(req).await.unwrap()
}

#[tokio::test]
async fn protocol_rejections() {
    let (channel, _) = setup().await;
    let addr = channel.target().trim_start_matches("http://").to_string();

    let resp = raw(&addr, "/test.Svc/Echo", |h| {
        h.set("content-type", "application/json");
    })
    .await;
    assert_eq!(resp.status().as_u16(), 415);
    assert_eq!(resp.headers().get("grpc-status"), Some("13"));

    let resp = raw(&addr, "/test.Svc/Echo", |h| {
        h.set("content-type", "application/grpc+json");
    })
    .await;
    assert_eq!(resp.status().as_u16(), 415);

    let resp = raw_with(
        &addr,
        Method::Get,
        "/test.Svc/Echo",
        Vec::new(),
        true,
        |_| {},
    )
    .await;
    assert_eq!(resp.status().as_u16(), 405);
    assert_eq!(resp.headers().get("grpc-status"), Some("13"));

    // HTTP/1.1 is not gRPC.
    let resp = raw_with(
        &addr,
        Method::Post,
        "/test.Svc/Echo",
        frame_of(&msg("x", 1)),
        false,
        |_| {},
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);

    // Compression other than identity, gzip and deflate.
    let resp = raw(&addr, "/test.Svc/Echo", |h| {
        h.set("grpc-encoding", "snappy");
    })
    .await;
    assert_eq!(resp.headers().get("grpc-status"), Some("12"));
    assert_eq!(
        resp.headers().get("grpc-accept-encoding"),
        Some("identity,gzip,deflate")
    );

    // A set compressed flag, a bad flag, a truncated message, two messages
    // for a unary method, none at all, a malformed body, a bad grpc-timeout.
    let mut two = frame_of(&msg("a", 1));
    two.extend(frame_of(&msg("b", 2)));
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (vec![1, 0, 0, 0, 1, 8], "13"),
        (vec![9, 0, 0, 0, 0], "13"),
        (vec![0, 0, 0, 0, 9, 8], "13"),
        (two, "13"),
        (Vec::new(), "13"),
        (vec![0, 0, 0, 0, 2, 0xff, 0xff], "13"),
        // 1 MiB declared, over the 64 KiB limit, with no body sent.
        (vec![0, 0, 0x10, 0, 0], "8"),
    ];
    for (body, code) in cases {
        let resp = raw_with(
            &addr,
            Method::Post,
            "/test.Svc/Echo",
            body.clone(),
            true,
            |_| {},
        )
        .await;
        let (_, trailers) = {
            let headers = resp.headers().clone();
            let (b, t) = resp.bytes_with_trailers().await.unwrap();
            assert!(b.is_empty(), "{body:?}");
            (headers.clone(), t.unwrap_or(headers))
        };
        assert_eq!(trailers.get("grpc-status"), Some(code), "{body:?}");
    }
    let resp = raw(&addr, "/test.Svc/Echo", |h| {
        h.set("grpc-timeout", "soon");
    })
    .await;
    assert_eq!(resp.headers().get("grpc-status"), Some("13"));
}

/// Plain HTTP answers mapped to gRPC codes.
struct NotGrpc;

impl HttpHandler for NotGrpc {
    fn handle(&self, req: ferroly::http::Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            match req.uri.path.as_str() {
                "/a.B/NotFound" => HttpResponse::text(StatusCode::NOT_FOUND, "nope"),
                "/a.B/Busy" => HttpResponse::text(StatusCode(503), "busy"),
                "/a.B/Denied" => HttpResponse::text(StatusCode(403), "no"),
                "/a.B/Teapot" => HttpResponse::text(StatusCode(418), "tea"),
                _ => HttpResponse::text(StatusCode::OK, "plain text"),
            }
        })
    }
}

#[tokio::test]
async fn non_grpc_responses_map_to_codes() {
    let addr = start(Arc::new(NotGrpc)).await;
    let channel = Channel::new(&format!("http://{addr}")).unwrap();
    for (path, code) in [
        ("/a.B/NotFound", Code::Unimplemented),
        ("/a.B/Busy", Code::Unavailable),
        ("/a.B/Denied", Code::PermissionDenied),
        ("/a.B/Teapot", Code::Unknown),
        ("/a.B/Ok", Code::Unknown),
    ] {
        let err = channel
            .unary::<Msg, Msg>(path, Request::new(Msg::default()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), code, "{path}: {err}");
    }
}

#[tokio::test]
async fn http1_only_server_is_unavailable() {
    // No `http2` in the config: the h2c preface is not understood.
    let addr = start_with(Arc::new(NotGrpc), ServerConfig::default()).await;
    let channel = Channel::new(&format!("http://{addr}")).unwrap();
    let err = channel
        .unary::<Msg, Msg>("/a.B/C", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert!(
        matches!(err.code(), Code::Unavailable | Code::Internal),
        "{err}"
    );

    // A client that is not set up for HTTP/2 is refused when the channel is
    // built, before anything is sent (GRPC-15).
    let err = Channel::from_client(Client::new(), &format!("http://{addr}"))
        .unwrap()
        .build()
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err}");
    // So is one that sets connection settings its client already fixes.
    let h2c = Client::builder()
        .http2_prior_knowledge(true)
        .build()
        .unwrap();
    let err = Channel::from_client(h2c.clone(), &format!("http://{addr}"))
        .unwrap()
        .user_agent("x")
        .build()
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err}");
    // An h2c client against an HTTP/1.1-only server: its answer is not
    // HTTP/2 framing.
    let channel = Channel::from_client(h2c, &format!("http://{addr}"))
        .unwrap()
        .build()
        .unwrap();
    let err = channel
        .unary::<Msg, Msg>("/a.B/C", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert!(
        matches!(err.code(), Code::Unavailable | Code::Internal),
        "{err}"
    );
}

#[tokio::test]
async fn health_check_and_watch() {
    let health = HealthReporter::new();
    health.set_status("test.Svc", ServingStatus::Serving);
    let addr = start(Arc::new(GrpcServer::new().add_service(health.service()))).await;
    let channel = Channel::new(&format!("http://{addr}")).unwrap();

    let check = |service: &str| {
        let channel = channel.clone();
        let req = HealthCheckRequest::new(service);
        async move {
            channel
                .unary::<HealthCheckRequest, HealthCheckResponse>(CHECK_PATH, Request::new(req))
                .await
        }
    };
    assert_eq!(
        check("").await.unwrap().into_inner(),
        HealthCheckResponse::new(ServingStatus::Serving)
    );
    assert_eq!(check("nope").await.unwrap_err().code(), Code::NotFound);

    let mut watch = channel
        .server_streaming::<HealthCheckRequest, HealthCheckResponse>(
            WATCH_PATH,
            Request::new(HealthCheckRequest::new("test.Svc")),
        )
        .await
        .unwrap()
        .into_inner();
    let first = watch.message().await.unwrap().unwrap();
    assert_eq!(first, HealthCheckResponse::new(ServingStatus::Serving));
    health.set_status("test.Svc", ServingStatus::NotServing);
    let second = watch.message().await.unwrap().unwrap();
    assert_eq!(second, HealthCheckResponse::new(ServingStatus::NotServing));
    health.clear("test.Svc");
    let third = watch.message().await.unwrap().unwrap();
    assert_eq!(
        third,
        HealthCheckResponse::new(ServingStatus::ServiceUnknown)
    );
    assert_eq!(check("test.Svc").await.unwrap_err().code(), Code::NotFound);

    // After shutdown, a draining server keeps reporting NOT_SERVING: later
    // status changes are ignored, as in grpc-go.
    health.set_status("other.Svc", ServingStatus::Serving);
    health.shutdown();
    health.set_status("", ServingStatus::Serving);
    health.set_status("other.Svc", ServingStatus::Serving);
    health.set_status("late.Svc", ServingStatus::Serving);
    health.clear("other.Svc");
    let not_serving = HealthCheckResponse::new(ServingStatus::NotServing);
    assert_eq!(check("").await.unwrap().into_inner(), not_serving);
    assert_eq!(check("other.Svc").await.unwrap().into_inner(), not_serving);
    assert_eq!(check("late.Svc").await.unwrap_err().code(), Code::NotFound);
}

#[tokio::test]
async fn fallback_serves_plain_http_on_the_same_port() {
    let flags = Flags::default();
    let server = test_server(&flags).fallback(Arc::new(NotGrpc));
    let addr = start(Arc::new(server)).await;
    let channel = Channel::new(&format!("http://{addr}")).unwrap();
    let ok = channel
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("x", 1)))
        .await
        .unwrap();
    assert_eq!(ok.get_ref().n, 2);
    // HTTP/1.1 and h2 plain requests reach the fallback.
    for h2 in [false, true] {
        let client = Client::builder().http2_prior_knowledge(h2).build().unwrap();
        let req = ferroly::http::Request::builder(Method::Get, &format!("http://{addr}/a.B/Ok"))
            .unwrap()
            .build();
        let resp = client.send(req).await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "plain text");
    }
}

#[cfg(feature = "turbo")]
#[tokio::test]
async fn turbo_router_as_fallback() {
    let router = ferroly::turbo::Router::new().get("/health", |_| async {
        HttpResponse::text(StatusCode::OK, "router ok")
    });
    let flags = Flags::default();
    let server = test_server(&flags).fallback(router.into_handler());
    let addr = start(Arc::new(server)).await;
    let client = Client::new();
    let req = ferroly::http::Request::builder(Method::Get, &format!("http://{addr}/health"))
        .unwrap()
        .build();
    assert_eq!(
        client.send(req).await.unwrap().text().await.unwrap(),
        "router ok"
    );
    let channel = Channel::new(&format!("http://{addr}")).unwrap();
    assert!(channel
        .unary::<Msg, Msg>("/test.Svc/Echo", Request::new(msg("x", 1)))
        .await
        .is_ok());
}

#[test]
fn metadata_is_validated_when_set() {
    let mut md = Metadata::new();
    assert!(md.insert("grpc-timeout", "1S").is_err());
    assert!(md.insert("x-ok", "fine").is_ok());
    assert_eq!(
        md.insert("bad key", "v").unwrap_err().code(),
        Code::InvalidArgument
    );
}

#[test]
fn server_registration_rejects_bad_paths() {
    let result = std::panic::catch_unwind(|| {
        GrpcServer::new().unary("not-a-path", |_req: Request<Msg>| async {
            Ok(Response::new(Msg::default()))
        })
    });
    assert!(result.is_err());
    let server = GrpcServer::new().unary("/a.B/C", |_req: Request<Msg>| async {
        Ok(Response::new(Msg::default()))
    });
    assert!(server.has_method("/a.B/C"));
    assert_eq!(server.services().collect::<Vec<_>>(), ["a.B"]);
}
