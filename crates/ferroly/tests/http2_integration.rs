#![cfg(feature = "http2")]
//! HTTP/2 between ferroly's own server and client: h2c with prior knowledge,
//! TLS with ALPN (and the HTTP/1.1 fallback), multiplexing, flow control
//! across large bodies, trailers, bidirectional streaming, panic isolation,
//! graceful shutdown and turbo routing.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    body_channel, serve_tls_with_config, serve_with_config, BodyStream, BoxFuture, Client,
    ClientTlsConfig, HeaderMap, Http2Config, HttpError, HttpHandler, HttpResponse, Method,
    Protocol, Request, ServerConfig, StatusCode, TlsConfig,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");

/// Records the peer ports it sees, so tests can count connections.
#[derive(Default)]
struct App {
    peers: Mutex<HashSet<u16>>,
}

impl HttpHandler for App {
    fn wants_body_stream(&self, req: &Request) -> bool {
        req.uri.path.starts_with("/stream")
    }

    fn handle(&self, mut req: Request) -> BoxFuture<'_, HttpResponse> {
        if let Some(peer) = req.peer_addr() {
            self.peers.lock().unwrap().insert(peer.port());
        }
        Box::pin(async move {
            match req.uri.path.as_str() {
                "/proto" => HttpResponse::text(
                    StatusCode::OK,
                    format!("{} tls={}", req.protocol(), req.is_tls()),
                ),
                "/echo" => HttpResponse::new(StatusCode::OK)
                    .header("x-method", req.method.as_str())
                    .header("x-host", req.headers.get("host").unwrap_or(""))
                    .header("x-cookie", req.headers.get("cookie").unwrap_or(""))
                    .body(req.body),
                "/big" => {
                    let n: usize = req
                        .uri
                        .query
                        .as_deref()
                        .and_then(|q| q.parse().ok())
                        .unwrap_or(0);
                    HttpResponse::new(StatusCode::OK).body(pattern(n))
                }
                "/trailers" => HttpResponse::text(StatusCode::OK, "with trailers")
                    .header("content-type", "application/grpc")
                    .trailer("grpc-status", "0")
                    .trailer("grpc-message", "ok"),
                "/chunks" => {
                    let (tx, rx) = mpsc::channel(4);
                    tokio::spawn(async move {
                        for i in 0..5 {
                            let _ = tx.send(format!("part{i};").into_bytes()).await;
                        }
                    });
                    HttpResponse::stream(StatusCode::OK, rx).trailer("x-parts", "5")
                }
                "/stream/echo" => {
                    // Echo each request chunk back as it arrives, then end
                    // with the request's trailers plus a count.
                    let mut body = req.take_body_stream().expect("a body stream");
                    let (mut tx, rx) = body_channel(4);
                    tokio::spawn(async move {
                        let mut n = 0;
                        while let Ok(Some(chunk)) = body.chunk().await {
                            n += 1;
                            if tx.send(chunk).await.is_err() {
                                return;
                            }
                        }
                        let mut trailers = body.take_trailers().unwrap_or_default();
                        trailers.set("x-chunks", n.to_string());
                        let _ = tx.send_trailers(trailers).await;
                    });
                    HttpResponse::body_stream(StatusCode::OK, rx)
                }
                "/stream/abort" => {
                    let (mut tx, rx) = body_channel(1);
                    tokio::spawn(async move {
                        let _ = tx.send(b"partial".to_vec()).await;
                        // Dropped without finishing: the stream is cut off.
                    });
                    HttpResponse::body_stream(StatusCode::OK, rx)
                }
                "/panic" => panic!("handler exploded"),
                "/slow" => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    HttpResponse::text(StatusCode::OK, "slow done")
                }
                _ => HttpResponse::text(StatusCode::NOT_FOUND, "nope"),
            }
        })
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn h2_config() -> ServerConfig {
    ServerConfig {
        http2: Some(Http2Config::default()),
        head_timeout: Duration::from_secs(5),
        on_error: None,
        ..ServerConfig::default()
    }
}

async fn spawn(app: Arc<App>, config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(listener, app, std::future::pending::<()>(), config).await;
    });
    addr
}

async fn spawn_tls(app: Arc<App>, config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap();
    tokio::spawn(async move {
        let _ =
            serve_tls_with_config(listener, app, std::future::pending::<()>(), config, tls).await;
    });
    addr
}

fn h2c_client() -> Client {
    Client::builder()
        .http2_prior_knowledge(true)
        .build()
        .unwrap()
}

fn tls_trust() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .without_web_pki_roots()
        .add_root_pem(CA)
        .unwrap()
}

fn get(url: &str) -> Request {
    Request::builder(Method::Get, url).unwrap().build()
}

#[tokio::test]
async fn h2c_prior_knowledge_round_trip() {
    let addr = spawn(Arc::default(), h2_config()).await;
    let client = h2c_client();
    let resp = client
        .send(get(&format!("http://{addr}/proto")))
        .await
        .unwrap();
    assert_eq!(resp.protocol(), Protocol::Http2);
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("date").is_some());
    assert_eq!(resp.text().await.unwrap(), "HTTP/2 tls=false");

    // POST with a body, a Host override and split cookies.
    let req = Request::builder(Method::Post, &format!("http://{addr}/echo"))
        .unwrap()
        .header("cookie", "a=1")
        .body(b"hello h2".to_vec())
        .build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.headers().get("x-method"), Some("POST"));
    assert_eq!(
        resp.headers().get("x-host"),
        Some(addr.to_string().as_str())
    );
    assert_eq!(resp.headers().get("x-cookie"), Some("a=1"));
    assert_eq!(resp.bytes().await.unwrap(), b"hello h2");

    // 404 and HEAD (no body).
    let resp = client
        .send(get(&format!("http://{addr}/missing")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let head = Request::builder(Method::Head, &format!("http://{addr}/proto"))
        .unwrap()
        .build();
    let resp = client.send(head).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn http1_is_unchanged_on_an_h2_server() {
    let addr = spawn(Arc::default(), h2_config()).await;
    // A default client speaks HTTP/1.1 to the same server.
    let resp = Client::new()
        .send(get(&format!("http://{addr}/proto")))
        .await
        .unwrap();
    assert_eq!(resp.protocol(), Protocol::Http11);
    assert_eq!(resp.text().await.unwrap(), "HTTP/1.1 tls=false");
    // A POST (whose first byte, 'P', starts like the preface) too.
    let req = Request::builder(Method::Post, &format!("http://{addr}/echo"))
        .unwrap()
        .body(b"x".to_vec())
        .build();
    let resp = Client::new().send(req).await.unwrap();
    assert_eq!(resp.bytes().await.unwrap(), b"x");
}

#[tokio::test]
async fn h2_over_tls_with_alpn_and_fallback() {
    let addr = spawn_tls(Arc::default(), h2_config()).await;
    let url = format!("https://127.0.0.1:{}/proto", addr.port());
    let client = Client::builder()
        .tls(tls_trust())
        .http2(true)
        .build()
        .unwrap();
    let resp = client.send(get(&url)).await.unwrap();
    assert_eq!(resp.protocol(), Protocol::Http2);
    assert_eq!(resp.text().await.unwrap(), "HTTP/2 tls=true");

    // A client that does not offer h2 gets HTTP/1.1 from the same server.
    let plain = Client::builder().tls(tls_trust()).build().unwrap();
    let resp = plain.send(get(&url)).await.unwrap();
    assert_eq!(resp.protocol(), Protocol::Http11);
    assert_eq!(resp.text().await.unwrap(), "HTTP/1.1 tls=true");

    // An h2 client against a server without HTTP/2 falls back to HTTP/1.1,
    // and keeps doing so.
    let config = ServerConfig {
        http2: None,
        ..h2_config()
    };
    let h1_only = spawn_tls(Arc::default(), config).await;
    let url = format!("https://127.0.0.1:{}/proto", h1_only.port());
    for _ in 0..2 {
        let resp = client.send(get(&url)).await.unwrap();
        assert_eq!(resp.protocol(), Protocol::Http11);
        assert_eq!(resp.text().await.unwrap(), "HTTP/1.1 tls=true");
    }
}

#[tokio::test]
async fn concurrent_requests_share_one_connection() {
    for tls in [false, true] {
        let app = Arc::new(App::default());
        let (addr, client, scheme) = if tls {
            let addr = spawn_tls(app.clone(), h2_config()).await;
            let client = Client::builder()
                .tls(tls_trust())
                .http2(true)
                .build()
                .unwrap();
            (addr, client, "https")
        } else {
            (spawn(app.clone(), h2_config()).await, h2c_client(), "http")
        };
        let mut tasks = Vec::new();
        for i in 0..64 {
            let client = client.clone();
            let url = format!("{scheme}://127.0.0.1:{}/echo", addr.port());
            tasks.push(tokio::spawn(async move {
                let req = Request::builder(Method::Post, &url)
                    .unwrap()
                    .body(format!("req-{i}").into_bytes())
                    .build();
                let resp = client.send(req).await.unwrap();
                assert_eq!(resp.protocol(), Protocol::Http2);
                assert_eq!(resp.text().await.unwrap(), format!("req-{i}"));
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(app.peers.lock().unwrap().len(), 1, "tls={tls}");
    }
}

#[tokio::test]
async fn concurrent_slow_requests_are_multiplexed() {
    let addr = spawn(Arc::default(), h2_config()).await;
    let client = h2c_client();
    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let client = client.clone();
        let url = format!("http://{addr}/slow");
        tasks.push(tokio::spawn(async move {
            client.send(get(&url)).await.unwrap().text().await.unwrap()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), "slow done");
    }
    // Twenty 300 ms handlers ran concurrently, not one after another.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn large_bodies_cross_flow_control_windows() {
    // Small windows force many WINDOW_UPDATE round trips.
    let mut small = Http2Config::default();
    small.initial_stream_window_size = 65_535;
    small.initial_connection_window_size = 65_535;
    let config = ServerConfig {
        http2: Some(small.clone()),
        max_body_bytes: 16 * 1024 * 1024,
        ..h2_config()
    };
    let addr = spawn(Arc::default(), config).await;
    let client = Client::builder()
        .http2_prior_knowledge(true)
        .http2_config(small)
        .build()
        .unwrap();
    // Download 6 MiB.
    let n = 6 * 1024 * 1024;
    let resp = client
        .send(get(&format!("http://{addr}/big?{n}")))
        .await
        .unwrap();
    assert_eq!(resp.headers().content_length(), Some(n as u64));
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), n);
    assert!(body == pattern(n));
    // Upload 5 MiB and get it echoed back.
    let up = pattern(5 * 1024 * 1024 + 7);
    let req = Request::builder(Method::Post, &format!("http://{addr}/echo"))
        .unwrap()
        .body(up.clone())
        .build();
    let back = client.send(req).await.unwrap().bytes().await.unwrap();
    assert!(back == up);
    // Several large transfers at once.
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let client = client.clone();
        let url = format!("http://{addr}/big?{}", 1024 * 1024);
        tasks.push(tokio::spawn(async move {
            client
                .send(get(&url))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .len()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 1024 * 1024);
    }
}

#[tokio::test]
async fn cancelled_downloads_leave_the_connection_usable() {
    let app = Arc::new(App::default());
    let addr = spawn(app.clone(), h2_config()).await;
    let client = h2c_client();
    let url = format!("http://{addr}/big?{}", 8 * 1024 * 1024);
    // Read one chunk of a large body, then drop the response (RST_STREAM
    // CANCEL) while the server still has data in flight — many times.
    for _ in 0..40 {
        let mut resp = client.send(get(&url)).await.unwrap();
        assert!(resp.chunk().await.unwrap().is_some());
        drop(resp);
    }
    // Late DATA and HEADERS for the cancelled streams neither break the
    // connection nor leak its flow-control window.
    let resp = client.send(get(&url)).await.unwrap();
    assert_eq!(resp.bytes().await.unwrap().len(), 8 * 1024 * 1024);
    assert_eq!(app.peers.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn request_body_over_the_limit_is_413() {
    let config = ServerConfig {
        max_body_bytes: 1000,
        ..h2_config()
    };
    let addr = spawn(Arc::default(), config).await;
    let client = h2c_client();
    let req = Request::builder(Method::Post, &format!("http://{addr}/echo"))
        .unwrap()
        .body(vec![b'x'; 5000])
        .build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // The connection is still good.
    let resp = client
        .send(get(&format!("http://{addr}/proto")))
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "HTTP/2 tls=false");
}

#[tokio::test]
async fn response_trailers_reach_the_client() {
    let addr = spawn(Arc::default(), h2_config()).await;
    let client = h2c_client();
    let mut resp = client
        .send(get(&format!("http://{addr}/trailers")))
        .await
        .unwrap();
    assert!(resp.trailers().is_none());
    // No content-length is announced for a response with trailers.
    assert!(resp.headers().get("content-length").is_none());
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.unwrap() {
        body.extend(chunk);
    }
    assert_eq!(body, b"with trailers");
    let trailers = resp.trailers().unwrap();
    assert_eq!(trailers.get("grpc-status"), Some("0"));
    assert_eq!(trailers.get("grpc-message"), Some("ok"));

    // A streamed (mpsc) body with trailers.
    let resp = client
        .send(get(&format!("http://{addr}/chunks")))
        .await
        .unwrap();
    let (body, trailers) = resp.bytes_with_trailers().await.unwrap();
    assert_eq!(body, b"part0;part1;part2;part3;part4;");
    assert_eq!(trailers.unwrap().get("x-parts"), Some("5"));
}

#[tokio::test]
async fn trailers_over_http1_chunked() {
    let addr = spawn(Arc::default(), h2_config()).await;
    let resp = Client::new()
        .send(get(&format!("http://{addr}/trailers")))
        .await
        .unwrap();
    assert_eq!(resp.protocol(), Protocol::Http11);
    let (body, trailers) = resp.bytes_with_trailers().await.unwrap();
    assert_eq!(body, b"with trailers");
    assert_eq!(trailers.unwrap().get("grpc-status"), Some("0"));
}

#[tokio::test]
async fn bidirectional_streaming_on_one_stream() {
    let addr = spawn(Arc::default(), h2_config()).await;
    let client = h2c_client();
    let (mut tx, rx) = body_channel(1);
    let req = Request::builder(Method::Post, &format!("http://{addr}/stream/echo"))
        .unwrap()
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .build();
    // Send the first message before the response head exists: the call must
    // return once the head arrives, while the upload is still open.
    tx.send(b"ping-0".to_vec()).await.unwrap();
    let mut resp = client.send_body_stream(req, rx).await.unwrap();
    assert_eq!(resp.protocol(), Protocol::Http2);
    assert_eq!(resp.chunk().await.unwrap().unwrap(), b"ping-0");
    // Ping-pong: each reply arrives before the next message is sent, which
    // only works if both directions stream concurrently.
    for i in 1..5 {
        tx.send(format!("ping-{i}").into_bytes()).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
            .await
            .expect("an echo before the request ended")
            .unwrap()
            .unwrap();
        assert_eq!(reply, format!("ping-{i}").into_bytes());
    }
    let mut trailers = HeaderMap::new();
    trailers.set("x-client", "done");
    tx.send_trailers(trailers).await.unwrap();
    assert_eq!(resp.chunk().await.unwrap(), None);
    let trailers = resp.trailers().unwrap();
    assert_eq!(trailers.get("x-chunks"), Some("5"));
    // The request's trailers were delivered to the handler.
    assert_eq!(trailers.get("x-client"), Some("done"));
}

#[tokio::test]
async fn body_stream_over_http1_uses_chunked_trailers() {
    let addr = spawn(Arc::default(), h2_config()).await;
    let (mut tx, rx) = body_channel(4);
    tokio::spawn(async move {
        tx.send(b"a".to_vec()).await.unwrap();
        tx.send(b"b".to_vec()).await.unwrap();
        let mut t = HeaderMap::new();
        t.set("x-client", "h1");
        tx.send_trailers(t).await.unwrap();
    });
    let req = Request::builder(Method::Post, &format!("http://{addr}/stream/echo"))
        .unwrap()
        .build();
    let resp = Client::new().send_body_stream(req, rx).await.unwrap();
    assert_eq!(resp.protocol(), Protocol::Http11);
    let (body, trailers) = resp.bytes_with_trailers().await.unwrap();
    // HTTP/1.1 streams the request body too: one piece per chunk sent, and
    // the chunked trailer section reaches the handler.
    assert_eq!(body, b"ab");
    let trailers = trailers.unwrap();
    assert_eq!(trailers.get("x-chunks"), Some("2"));
    // H2-14: the request's chunked trailers reach a streaming handler over
    // HTTP/1.1 too, as over HTTP/2.
    assert_eq!(trailers.get("x-client"), Some("h1"));
}

/// H2-5: values with surrounding whitespace (which HTTP/1.1 ignores) are
/// trimmed on send, so ferroly's own HTTP/2 client accepts the response.
#[tokio::test]
async fn header_values_with_surrounding_whitespace_work_over_both_protocols() {
    struct Padded;
    impl HttpHandler for Padded {
        fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
            Box::pin(async {
                HttpResponse::text(StatusCode::OK, "padded")
                    .header("x-pad", "  value\t")
                    .trailer("x-trail", " t ")
            })
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            Arc::new(Padded),
            std::future::pending::<()>(),
            h2_config(),
        )
        .await;
    });
    for client in [h2c_client(), Client::new()] {
        let resp = client.send(get(&format!("http://{addr}/"))).await.unwrap();
        assert_eq!(resp.headers().get("x-pad"), Some("value"));
        let h2 = resp.protocol() == Protocol::Http2;
        let (body, trailers) = resp.bytes_with_trailers().await.unwrap();
        assert_eq!(body, b"padded");
        if h2 {
            assert_eq!(trailers.unwrap().get("x-trail"), Some("t"));
        }
    }
}

#[tokio::test]
async fn aborted_response_stream_is_an_error_not_a_short_body() {
    let addr = spawn(Arc::default(), h2_config()).await;
    let client = h2c_client();
    let resp = client
        .send(get(&format!("http://{addr}/stream/abort")))
        .await
        .unwrap();
    assert!(resp.bytes().await.is_err());
    // The connection survives.
    let resp = client
        .send(get(&format!("http://{addr}/proto")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn handler_panic_is_a_500_and_reported() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook_seen = seen.clone();
    let config = ServerConfig {
        on_error: Some(ferroly::http::ErrorHook::new(move |_, e| {
            hook_seen.lock().unwrap().push(e.to_string());
        })),
        ..h2_config()
    };
    let addr = spawn(Arc::default(), config).await;
    let client = h2c_client();
    let resp = client
        .send(get(&format!("http://{addr}/panic")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = client
        .send(get(&format!("http://{addr}/proto")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.contains("handler exploded")));
}

#[tokio::test]
async fn handler_timeout_is_a_503() {
    let config = ServerConfig {
        handler_timeout: Some(Duration::from_millis(50)),
        ..h2_config()
    };
    let addr = spawn(Arc::default(), config).await;
    let resp = h2c_client()
        .send(get(&format!("http://{addr}/slow")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn graceful_shutdown_finishes_in_flight_streams() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_with_config(
            listener,
            Arc::new(App::default()),
            async {
                let _ = stop_rx.await;
            },
            h2_config(),
        )
        .await
    });
    let client = h2c_client();
    // Warm the connection, then start a slow request and shut down under it.
    client
        .send(get(&format!("http://{addr}/proto")))
        .await
        .unwrap();
    let slow = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .send(get(&format!("http://{addr}/slow")))
                .await?
                .text()
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    stop_tx.send(()).unwrap();
    assert_eq!(slow.await.unwrap().unwrap(), "slow done");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server drains promptly")
        .unwrap()
        .unwrap();
    // After GOAWAY the connection is not reused; nothing listens any more.
    let err = client.send(get(&format!("http://{addr}/proto"))).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn request_with_hand_built_body_stream() {
    // The setter lets streaming handlers be unit-tested without a server.
    let app = App::default();
    let mut req = Request::builder(Method::Post, "http://x/stream/echo")
        .unwrap()
        .build();
    req.set_body_stream(BodyStream::from_bytes(b"unit".to_vec()));
    let resp = app.handle(req).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.trailers().is_empty());
}

#[tokio::test]
async fn client_errors_are_reported_not_panics() {
    // Prior knowledge against an HTTP/1.1-only server fails cleanly.
    let config = ServerConfig {
        http2: None,
        ..h2_config()
    };
    let addr = spawn(Arc::default(), config).await;
    let err = h2c_client()
        .send(get(&format!("http://{addr}/proto")))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            HttpError::Protocol(_) | HttpError::Io(_) | HttpError::Http2(_)
        ),
        "{err:?}"
    );
}

#[cfg(feature = "turbo")]
mod turbo_over_h2 {
    use super::*;
    use ferroly::turbo::Router;

    #[tokio::test]
    async fn turbo_routes_work_unchanged() {
        let router = Router::new().get("/users/:id", |ctx: ferroly::turbo::Ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                format!("user {}", ctx.param("id").unwrap_or("")),
            )
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = serve_with_config(
                listener,
                router.into_handler(),
                std::future::pending::<()>(),
                h2_config(),
            )
            .await;
        });
        let resp = h2c_client()
            .send(get(&format!("http://{addr}/users/42")))
            .await
            .unwrap();
        assert_eq!(resp.protocol(), Protocol::Http2);
        assert_eq!(resp.text().await.unwrap(), "user 42");
        let resp = h2c_client()
            .send(get(&format!("http://{addr}/nowhere")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
