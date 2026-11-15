#![cfg(feature = "http2")]
//! Interoperability with a mainstream HTTP/2 client: the system `curl`
//! (libcurl + nghttp2), over cleartext h2c with prior knowledge and over TLS
//! with ALPN. Each test skips itself (passing) when `curl` is missing or was
//! built without HTTP/2 support.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    serve_tls_with_config, serve_with_config, BoxFuture, Http2Config, HttpHandler, HttpResponse,
    Request, ServerConfig, StatusCode, TlsConfig,
};
use tokio::net::TcpListener;

const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");

fn ca_path() -> String {
    format!(
        "{}/src/http/testdata/test_ca_cert.pem",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Whether a `curl` with HTTP/2 support is on the PATH.
fn curl_with_http2() -> bool {
    match Command::new("curl").arg("-V").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let ok = text
                .lines()
                .any(|l| l.starts_with("Features:") && l.split_whitespace().any(|f| f == "HTTP2"));
            if !ok {
                eprintln!("skipping: curl has no HTTP2 feature");
            }
            ok
        }
        _ => {
            eprintln!("skipping: curl is not available");
            false
        }
    }
}

/// Runs curl with `args` off the async runtime; returns (exit ok, stdout,
/// stderr).
async fn curl(args: Vec<String>) -> (bool, Vec<u8>, String) {
    tokio::task::spawn_blocking(move || {
        let out = Command::new("curl")
            .args(["-sS", "--max-time", "20"])
            .args(&args)
            .output()
            .expect("run curl");
        (
            out.status.success(),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    })
    .await
    .unwrap()
}

#[derive(Default)]
struct App {
    peers: Mutex<HashSet<u16>>,
}

impl HttpHandler for App {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        if let Some(peer) = req.peer_addr() {
            self.peers.lock().unwrap().insert(peer.port());
        }
        Box::pin(async move {
            match req.uri.path.as_str() {
                "/proto" => HttpResponse::text(StatusCode::OK, req.protocol().to_string()),
                "/echo" => HttpResponse::new(StatusCode::OK).body(req.body),
                "/big" => {
                    let n: usize = req.uri.query.as_deref().unwrap_or("0").parse().unwrap_or(0);
                    HttpResponse::new(StatusCode::OK).body(vec![b'z'; n])
                }
                "/slow" => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    HttpResponse::text(StatusCode::OK, "slow")
                }
                "/trailers" => HttpResponse::text(StatusCode::OK, "t").trailer("x-done", "yes"),
                _ => HttpResponse::text(StatusCode::NOT_FOUND, "nope"),
            }
        })
    }
}

fn config() -> ServerConfig {
    ServerConfig {
        http2: Some(Http2Config::default()),
        ..ServerConfig::default()
    }
}

async fn spawn(app: Arc<App>, tls: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let shutdown = std::future::pending::<()>();
        if tls {
            let tls = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap();
            let _ = serve_tls_with_config(listener, app, shutdown, config(), tls).await;
        } else {
            let _ = serve_with_config(listener, app, shutdown, config()).await;
        }
    });
    addr
}

fn s(v: &str) -> String {
    v.to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn curl_prior_knowledge_h2c() {
    if !curl_with_http2() {
        return;
    }
    let addr = spawn(Arc::default(), false).await;
    let base = format!("http://{addr}");
    let (ok, out, err) = curl(vec![
        s("--http2-prior-knowledge"),
        s("-i"),
        format!("{base}/proto"),
    ])
    .await;
    assert!(ok, "{err}");
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/2 200"), "{text}");
    assert!(text.ends_with("HTTP/2"), "{text}");

    // A POST body.
    let (ok, out, err) = curl(vec![
        s("--http2-prior-knowledge"),
        s("--data-binary"),
        s("hello from curl"),
        format!("{base}/echo"),
    ])
    .await;
    assert!(ok, "{err}");
    assert_eq!(out, b"hello from curl");

    // A body larger than the default 64 KiB windows.
    let n = 3 * 1024 * 1024;
    let (ok, out, err) = curl(vec![
        s("--http2-prior-knowledge"),
        format!("{base}/big?{n}"),
    ])
    .await;
    assert!(ok, "{err}");
    assert_eq!(out.len(), n);

    // Trailers do not upset curl.
    let (ok, out, err) = curl(vec![
        s("--http2-prior-knowledge"),
        format!("{base}/trailers"),
    ])
    .await;
    assert!(ok, "{err}");
    assert_eq!(out, b"t");
}

#[tokio::test(flavor = "multi_thread")]
async fn curl_concurrent_streams_on_one_connection() {
    if !curl_with_http2() {
        return;
    }
    let app = Arc::new(App::default());
    let addr = spawn(app.clone(), false).await;
    let mut args = vec![
        s("--http2-prior-knowledge"),
        s("--parallel"),
        s("--parallel-max"),
        s("16"),
    ];
    for _ in 0..16 {
        args.push(format!("http://{addr}/slow"));
    }
    let (ok, out, err) = curl(args).await;
    assert!(ok, "{err}");
    assert_eq!(String::from_utf8_lossy(&out), "slow".repeat(16));
    assert_eq!(app.peers.lock().unwrap().len(), 1, "curl multiplexed");
}

#[tokio::test(flavor = "multi_thread")]
async fn curl_h2_over_tls_with_alpn() {
    if !curl_with_http2() {
        return;
    }
    let addr = spawn(Arc::default(), true).await;
    let url = format!("https://127.0.0.1:{}/proto", addr.port());
    let (ok, out, err) = curl(vec![
        s("--http2"),
        s("--cacert"),
        ca_path(),
        s("-i"),
        url.clone(),
    ])
    .await;
    if !ok && tls_backend_refused(&err) {
        // Some TLS backends (e.g. Windows Schannel) cannot take a private CA
        // file; that says nothing about HTTP/2.
        eprintln!("skipping: curl's TLS backend rejected the test CA: {err}");
        return;
    }
    assert!(ok, "{err}");
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/2 200"), "{text}");
    assert!(text.ends_with("HTTP/2"), "{text}");

    // `--http1.1` still gets HTTP/1.1 from the same server.
    let (ok, out, err) = curl(vec![s("--http1.1"), s("--cacert"), ca_path(), url]).await;
    assert!(ok, "{err}");
    assert_eq!(out, b"HTTP/1.1");
}

/// Whether a curl error is about its TLS backend (certificates, CA files)
/// rather than about the HTTP exchange.
fn tls_backend_refused(stderr: &str) -> bool {
    let e = stderr.to_ascii_lowercase();
    ["ssl", "certificate", "schannel", "cacert", "tls"]
        .iter()
        .any(|w| e.contains(w))
}
