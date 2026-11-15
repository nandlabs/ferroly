//! The OTLP exporter's transports: TLS with a private CA, a caller-supplied
//! HTTP client, OTLP protobuf over HTTP and OTLP/gRPC.
//!
//! The exporter is driven directly (`Exporter::export`), so these tests touch
//! no global state.

#![cfg(all(feature = "obs", feature = "http"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpListener;

use ferroly::http::{
    serve, serve_tls, BoxFuture, Client, ClientTlsConfig, HttpError, HttpHandler, HttpResponse,
    Interceptor, Request, StatusCode, TlsConfig,
};
use ferroly::obs::{Exporter, FinishedSpan, OtlpHttpExporter, Resource};

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");

/// What a collector received: `(content-type, body)` per request.
type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

struct Collector(Seen);

impl HttpHandler for Collector {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        let content_type = req.headers.get("content-type").unwrap_or("").to_string();
        self.0.lock().unwrap().push((content_type, req.body));
        Box::pin(async { HttpResponse::new(StatusCode::OK) })
    }
}

async fn collector(tls: bool) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let handler = Arc::new(Collector(seen.clone()));
    let scheme = if tls { "https" } else { "http" };
    tokio::spawn(async move {
        let stop = std::future::pending::<()>();
        if tls {
            let config = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap();
            let _ = serve_tls(listener, handler, stop, config).await;
        } else {
            let _ = serve(listener, handler, stop).await;
        }
    });
    (
        format!("{scheme}://127.0.0.1:{}/v1/traces", addr.port()),
        seen,
    )
}

fn span(name: &str) -> FinishedSpan {
    FinishedSpan {
        name: name.into(),
        trace_id: "0123456789abcdef0123456789abcdef".into(),
        span_id: "0123456789abcdef".into(),
        start_unix_nanos: 1_000,
        duration_nanos: 10,
        ..FinishedSpan::default()
    }
}

fn trust_test_ca() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .without_web_pki_roots()
        .add_root_pem(CA)
        .unwrap()
}

fn fast(builder: ferroly::obs::OtlpBuilder) -> ferroly::obs::OtlpBuilder {
    builder
        .resource(Resource::new("transport-test"))
        .max_retries(0)
        .timeout(Duration::from_secs(5))
        .flush_timeout(Duration::from_secs(10))
}

#[tokio::test]
async fn a_collector_behind_a_private_ca_is_reached_with_tls_options() {
    let (url, seen) = collector(true).await;

    // Trusting the private CA: delivered.
    let exporter = fast(OtlpHttpExporter::builder(&url))
        .tls(trust_test_ca())
        .build();
    exporter.export(&span("secure"));
    assert!(exporter.flush_async().await, "{:?}", exporter.stats());
    assert_eq!(exporter.stats().exported_spans, 1);
    assert_eq!(seen.lock().unwrap().len(), 1);

    // The default (web PKI) roots reject the certificate, and a TLS failure
    // is not retried.
    let exporter = fast(OtlpHttpExporter::builder(&url)).max_retries(3).build();
    exporter.export(&span("untrusted"));
    assert!(!exporter.flush_async().await);
    let stats = exporter.stats();
    assert_eq!(stats.failed_spans, 1);
    assert_eq!(stats.retries, 0, "{stats:?}");
    assert!(stats.last_error.unwrap().contains("tls"));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_tls_options_fail_batches_with_the_reason() {
    let (url, seen) = collector(true).await;
    // No trusted roots at all: the client cannot be built. `build` does not
    // panic; every batch fails with the reason, without a retry.
    let broken = ClientTlsConfig::new().without_web_pki_roots();
    let exporter = fast(OtlpHttpExporter::builder(&url))
        .tls(broken)
        .max_retries(3)
        .build();
    exporter.export(&span("x"));
    assert!(!exporter.flush_async().await);
    let stats = exporter.stats();
    assert_eq!((stats.failed_spans, stats.retries), (1, 0), "{stats:?}");
    let error = stats.last_error.unwrap();
    assert!(
        error.contains("HTTP client") && error.contains("root"),
        "{error}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// A CONNECT proxy: records each request head's first line and tunnels to
/// the requested authority.
async fn connect_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = lines.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let line = head.lines().next().unwrap_or("").to_string();
                seen.lock().unwrap().push(line.clone());
                let authority = line.split(' ').nth(1).unwrap_or("").to_string();
                let Ok(mut upstream) = tokio::net::TcpStream::connect(authority).await else {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                let _ = client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), lines)
}

#[tokio::test]
async fn the_proxy_option_tunnels_through_connect() {
    use ferroly::http::Proxy;
    let (url, seen) = collector(true).await;
    let (proxy_url, proxied) = connect_proxy().await;
    let exporter = fast(OtlpHttpExporter::builder(&url))
        .tls(trust_test_ca())
        .proxy(Proxy::https(&proxy_url).unwrap())
        .build();
    exporter.export(&span("tunnelled"));
    assert!(exporter.flush_async().await, "{:?}", exporter.stats());
    assert_eq!(seen.lock().unwrap().len(), 1);
    let authority = url
        .trim_start_matches("https://")
        .trim_end_matches("/v1/traces");
    assert_eq!(
        proxied.lock().unwrap().as_slice(),
        [format!("CONNECT {authority} HTTP/1.1")]
    );
}

/// A `Retry-After` given as an HTTP date is honoured, like one in seconds.
#[tokio::test]
async fn a_retry_after_date_is_honoured() {
    use std::time::{Instant, SystemTime};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let count = count.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                let mut got = Vec::new();
                // Read the head and the (Content-Length) body.
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got).to_ascii_lowercase();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if got.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let answer = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    let at = SystemTime::now() + Duration::from_secs(2);
                    format!(
                        "HTTP/1.1 503 Service Unavailable\r\nretry-after: {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        ferroly::http::format_http_date(at)
                    )
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                };
                let _ = sock.write_all(answer.as_bytes()).await;
            });
        }
    });
    let exporter = fast(OtlpHttpExporter::builder(format!(
        "http://127.0.0.1:{}/v1/traces",
        addr.port()
    )))
    .max_retries(1)
    .retry_backoff(Duration::from_secs(3600)) // would never finish
    .build();
    exporter.export(&span("paced"));
    let started = Instant::now();
    assert!(exporter.flush_async().await, "{:?}", exporter.stats());
    let waited = started.elapsed();
    // The date has whole-second precision: between 1 and 2 seconds.
    assert!(waited >= Duration::from_millis(900), "waited {waited:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

struct Counting(Arc<AtomicUsize>);

impl Interceptor for Counting {
    fn on_request(&self, _req: &mut Request) -> Result<(), HttpError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn a_supplied_http_client_is_used_and_wins_over_tls_options() {
    let (url, seen) = collector(true).await;
    let sent = Arc::new(AtomicUsize::new(0));
    let client = Client::builder()
        .tls(trust_test_ca())
        .interceptor(Counting(sent.clone()))
        .build()
        .unwrap();
    // The `tls` option (web PKI only) would fail; the supplied client trusts
    // the CA and is the one used.
    let exporter = fast(OtlpHttpExporter::builder(&url))
        .tls(ClientTlsConfig::new())
        .http_client(client)
        .build();
    exporter.export(&span("via-client"));
    exporter.export(&span("via-client-2"));
    assert!(exporter.flush_async().await, "{:?}", exporter.stats());
    assert_eq!(sent.load(Ordering::SeqCst), 1, "one batch, one request");
    assert_eq!(exporter.stats().exported_spans, 2);
    assert_eq!(seen.lock().unwrap()[0].0, "application/json");
}

#[cfg(feature = "protobuf")]
mod protobuf {
    use super::*;
    use ferroly::obs::OtlpProtocol;
    use ferroly::proto::wire::{Reader, WireType};

    /// The length-delimited fields with `tag` in `bytes`.
    fn submessages(bytes: &[u8], tag: u32) -> Vec<Vec<u8>> {
        let mut r = Reader::new(bytes);
        let mut out = Vec::new();
        while let Some((t, wt)) = r.read_key().unwrap() {
            if t == tag && wt == WireType::Len {
                out.push(r.read_bytes().unwrap().to_vec());
            } else {
                r.skip_field(t, wt).unwrap();
            }
        }
        out
    }

    /// The span names in an encoded `ExportTraceServiceRequest`.
    pub(super) fn span_names(request: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        for resource_spans in submessages(request, 1) {
            for scope_spans in submessages(&resource_spans, 2) {
                for span in submessages(&scope_spans, 2) {
                    for name in submessages(&span, 5) {
                        names.push(String::from_utf8(name).unwrap());
                    }
                }
            }
        }
        names
    }

    #[tokio::test]
    async fn http_protobuf_sends_an_encoded_export_request() {
        let (url, seen) = collector(false).await;
        let exporter = fast(OtlpHttpExporter::builder(&url))
            .protocol(OtlpProtocol::HttpProtobuf)
            .build();
        exporter.export(&span("first"));
        exporter.export(&span("second"));
        assert!(exporter.flush_async().await, "{:?}", exporter.stats());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "application/x-protobuf");
        assert_eq!(span_names(&seen[0].1), ["first", "second"]);
    }

    #[test]
    fn json_stays_the_default_protocol() {
        assert_eq!(OtlpProtocol::default(), OtlpProtocol::HttpJson);
    }
}

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use ferroly::grpc::{GrpcServer, Request as GrpcRequest, Response, Status};
    use ferroly::http::{serve_with_config, Http2Config, ServerConfig};
    use ferroly::obs::OtlpProtocol;
    use ferroly::proto::Message;

    #[derive(ferroly::proto::Message, Default, Debug)]
    struct ExportTraceServiceRequest {
        #[proto(tag = 1)]
        resource_spans: Vec<Vec<u8>>,
    }

    #[derive(ferroly::proto::Message, Default, Debug)]
    struct ExportTraceServiceResponse {}

    type Calls = Arc<Mutex<Vec<(Vec<u8>, Option<String>)>>>;

    async fn grpc_collector(fail_first: bool) -> (String, Calls) {
        let calls: Calls = Arc::default();
        let sink = calls.clone();
        let failed = Arc::new(AtomicUsize::new(0));
        let server = GrpcServer::new().unary(
            "/opentelemetry.proto.collector.trace.v1.TraceService/Export",
            move |req: GrpcRequest<ExportTraceServiceRequest>| {
                let sink = sink.clone();
                let failed = failed.clone();
                async move {
                    let auth = req.metadata().get("authorization").map(str::to_string);
                    if fail_first && failed.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(Status::unavailable("warming up"));
                    }
                    let message = req.into_inner();
                    let mut request = Vec::new();
                    for rs in &message.resource_spans {
                        ferroly::proto::wire::encode_key(
                            1,
                            ferroly::proto::wire::WireType::Len,
                            &mut request,
                        );
                        ferroly::proto::wire::encode_varint(rs.len() as u64, &mut request);
                        request.extend_from_slice(rs);
                    }
                    sink.lock().unwrap().push((request, auth));
                    Ok::<_, Status>(Response::new(ExportTraceServiceResponse {}))
                }
            },
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = ServerConfig {
            http2: Some(Http2Config::default()),
            ..ServerConfig::default()
        };
        tokio::spawn(serve_with_config(
            listener,
            Arc::new(server),
            std::future::pending(),
            config,
        ));
        (format!("http://127.0.0.1:{}", addr.port()), calls)
    }

    #[tokio::test]
    async fn grpc_export_reaches_a_grpc_collector() {
        let (url, calls) = grpc_collector(false).await;
        let exporter = fast(OtlpHttpExporter::builder(&url))
            .protocol(OtlpProtocol::Grpc)
            .header("Authorization", "Bearer t0ken")
            .build();
        exporter.export(&span("over-grpc"));
        assert!(exporter.flush_async().await, "{:?}", exporter.stats());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(super::protobuf::span_names(&calls[0].0), ["over-grpc"]);
        assert_eq!(calls[0].1.as_deref(), Some("Bearer t0ken"));
        // The response decodes as the (empty) OTLP response.
        let _ = ExportTraceServiceResponse::decode(&[]).unwrap();
    }

    #[tokio::test]
    async fn grpc_unavailable_is_retried() {
        let (url, calls) = grpc_collector(true).await;
        let exporter = fast(OtlpHttpExporter::builder(&url))
            .protocol(OtlpProtocol::Grpc)
            .max_retries(2)
            .retry_backoff(Duration::from_millis(10))
            .build();
        exporter.export(&span("retried"));
        assert!(exporter.flush_async().await, "{:?}", exporter.stats());
        let stats = exporter.stats();
        assert_eq!(stats.retries, 1, "{stats:?}");
        assert_eq!(stats.exported_spans, 1);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    /// An encoded `google.rpc.Status` whose details hold a `RetryInfo` with
    /// a 10 ms delay.
    fn status_with_retry_info() -> Vec<u8> {
        use ferroly::proto::wire::{encode_key, encode_varint, WireType};
        fn field(buf: &mut Vec<u8>, tag: u32, bytes: &[u8]) {
            encode_key(tag, WireType::Len, buf);
            encode_varint(bytes.len() as u64, buf);
            buf.extend_from_slice(bytes);
        }
        let mut duration = Vec::new();
        encode_key(2, WireType::Varint, &mut duration);
        encode_varint(10_000_000, &mut duration);
        let mut retry_info = Vec::new();
        field(&mut retry_info, 1, &duration);
        let mut any = Vec::new();
        field(&mut any, 1, b"type.googleapis.com/google.rpc.RetryInfo");
        field(&mut any, 2, &retry_info);
        let mut status = Vec::new();
        field(&mut status, 3, &any);
        status
    }

    /// A collector that answers the first call with `RESOURCE_EXHAUSTED`
    /// (with a `RetryInfo` when `retry_info`), then accepts. Returns the URL
    /// and the number of calls.
    async fn exhausted_collector(retry_info: bool) -> (String, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let server = GrpcServer::new().unary(
            "/opentelemetry.proto.collector.trace.v1.TraceService/Export",
            move |_req: GrpcRequest<ExportTraceServiceRequest>| {
                let count = count.clone();
                async move {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        let status = Status::resource_exhausted("over quota");
                        return Err(if retry_info {
                            status.with_details(status_with_retry_info())
                        } else {
                            status
                        });
                    }
                    Ok::<_, Status>(Response::new(ExportTraceServiceResponse {}))
                }
            },
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = ServerConfig {
            http2: Some(Http2Config::default()),
            ..ServerConfig::default()
        };
        tokio::spawn(serve_with_config(
            listener,
            Arc::new(server),
            std::future::pending(),
            config,
        ));
        (format!("http://127.0.0.1:{}", addr.port()), calls)
    }

    #[tokio::test]
    async fn resource_exhausted_is_retried_only_with_retry_info() {
        // Without RetryInfo: permanent.
        let (url, calls) = exhausted_collector(false).await;
        let exporter = fast(OtlpHttpExporter::builder(&url))
            .protocol(OtlpProtocol::Grpc)
            .max_retries(3)
            .retry_backoff(Duration::from_millis(1))
            .build();
        exporter.export(&span("quota"));
        assert!(!exporter.flush_async().await);
        let stats = exporter.stats();
        assert_eq!((stats.retries, stats.failed_spans), (0, 1), "{stats:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // With RetryInfo: retried after its delay.
        let (url, calls) = exhausted_collector(true).await;
        let exporter = fast(OtlpHttpExporter::builder(&url))
            .protocol(OtlpProtocol::Grpc)
            .max_retries(3)
            .retry_backoff(Duration::from_secs(3600)) // the RetryInfo delay wins
            .build();
        exporter.export(&span("quota"));
        assert!(exporter.flush_async().await, "{:?}", exporter.stats());
        assert_eq!(exporter.stats().retries, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A TLS failure over gRPC is permanent, as it is over HTTP.
    #[tokio::test]
    async fn a_grpc_tls_failure_is_not_retried() {
        // A TLS endpoint whose certificate the default (web PKI) roots reject.
        let (url, _seen) = super::collector(true).await;
        let base = url.trim_end_matches("/v1/traces").to_string();
        let exporter = fast(OtlpHttpExporter::builder(&base))
            .protocol(OtlpProtocol::Grpc)
            .max_retries(3)
            .retry_backoff(Duration::from_millis(1))
            .build();
        exporter.export(&span("untrusted"));
        assert!(!exporter.flush_async().await);
        let stats = exporter.stats();
        assert_eq!((stats.retries, stats.failed_spans), (0, 1), "{stats:?}");
        assert!(stats.last_error.unwrap().contains("TLS"));
    }
}
