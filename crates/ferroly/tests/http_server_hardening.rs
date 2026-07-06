#![cfg(feature = "http")]
//! Exercises the HTTP server's resource limits: oversized bodies are rejected
//! with 413 before allocation, and normal requests still succeed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferroly::http::{
    serve_with_config, BoxFuture, Client, HttpHandler, HttpResponse, Method, Request, ServerConfig,
    StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Reads exactly one HTTP/1.1 response (headers + `Content-Length` body) from a
/// raw socket, so a second request can follow on a kept-alive connection.
async fn read_one_response(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]);
            let cl = head
                .lines()
                .find_map(|l| {
                    let ll = l.to_ascii_lowercase();
                    ll.strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            let end = pos + 4 + cl;
            while buf.len() < end {
                let mut b = [0u8; 512];
                let n = sock.read(&mut b).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&b[..n]);
            }
            return String::from_utf8_lossy(&buf[..end.min(buf.len())]).into_owned();
        }
        let mut b = [0u8; 512];
        let n = sock.read(&mut b).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&b[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

struct Ok200;
impl HttpHandler for Ok200 {
    fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move { HttpResponse::text(StatusCode::OK, "ok") })
    }
}

struct EchoBody;
impl HttpHandler for EchoBody {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            HttpResponse::text(
                StatusCode::OK,
                String::from_utf8_lossy(&req.body).into_owned(),
            )
        })
    }
}

fn config(max_body_bytes: usize) -> ServerConfig {
    ServerConfig {
        max_body_bytes,
        max_connections: 8,
        head_timeout: Duration::from_secs(5),
        body_timeout: Duration::from_secs(5),
        drain_timeout: Duration::from_secs(5),
        ..ServerConfig::default()
    }
}

/// Spawns the server on an ephemeral port with a small body cap and returns its
/// address.
async fn spawn(max_body_bytes: usize) -> String {
    spawn_with(std::sync::Arc::new(Ok200), max_body_bytes).await
}

async fn spawn_with(handler: std::sync::Arc<dyn HttpHandler>, max_body_bytes: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            handler,
            std::future::pending::<()>(),
            config(max_body_bytes),
        )
        .await;
    });
    addr.to_string()
}

/// Sends `raw` bytes to `addr` and returns the response as a string.
async fn send_raw(addr: &str, raw: &[u8]) -> String {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(raw).await.unwrap();
    sock.flush().await.unwrap();
    let mut out = Vec::new();
    sock.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn rejects_oversized_content_length_with_413() {
    let addr = spawn(16).await;
    // Advertise a huge body but send none — the server must reject on the
    // header alone, never allocating the claimed length.
    let req = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1000000000\r\n\r\n";
    let resp = send_raw(&addr, req.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 413"), "got: {resp:?}");
}

#[tokio::test]
async fn accepts_body_within_limit() {
    let addr = spawn(1024).await;
    let req = "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi";
    let resp = send_raw(&addr, req.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");
    assert!(resp.trim_end().ends_with("ok"), "got: {resp:?}");
}

#[tokio::test]
async fn dechunks_chunked_request_body() {
    let addr = spawn_with(std::sync::Arc::new(EchoBody), 1024).await;
    // "hello" as a single chunk, then the terminating zero chunk.
    let req =
        "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n\
               5\r\nhello\r\n0\r\n\r\n";
    let resp = send_raw(&addr, req.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");
    assert!(resp.ends_with("hello"), "got: {resp:?}");
}

#[tokio::test]
async fn rejects_conflicting_content_length_and_transfer_encoding() {
    let addr = spawn(1024).await;
    let req = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
               Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    let resp = send_raw(&addr, req.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp:?}");
}

#[tokio::test]
async fn rejects_conflicting_duplicate_content_length() {
    let addr = spawn(1024).await;
    let req = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello";
    let resp = send_raw(&addr, req.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp:?}");
}

#[tokio::test]
async fn rejects_too_many_headers() {
    let addr = spawn(1024).await;
    let mut req = String::from("GET / HTTP/1.1\r\nHost: x\r\n");
    for i in 0..500 {
        req.push_str(&format!("X-H{i}: v\r\n"));
    }
    req.push_str("\r\n");
    // The connection is dropped (protocol error) rather than served a 200.
    let resp = send_raw(&addr, req.as_bytes()).await;
    assert!(!resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");
}

#[tokio::test]
async fn drains_in_flight_request_on_shutdown() {
    use tokio::sync::oneshot;

    struct Slow;
    impl HttpHandler for Slow {
        fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                HttpResponse::text(StatusCode::OK, "done")
            })
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            std::sync::Arc::new(Slow),
            async move {
                let _ = rx.await;
            },
            config(1024),
        )
        .await;
    });

    let mut sock = TcpStream::connect(&addr).await.unwrap();
    sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    sock.flush().await.unwrap();

    // Let the handler start, then trigger shutdown while it is in flight.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = tx.send(());

    // The in-flight request must still complete despite the shutdown signal.
    let mut out = Vec::new();
    sock.read_to_end(&mut out).await.unwrap();
    let resp = String::from_utf8_lossy(&out);
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");
    assert!(resp.trim_end().ends_with("done"), "got: {resp:?}");
    let _ = server.await;
}

#[tokio::test]
async fn server_keep_alive_serves_multiple_requests_on_one_connection() {
    let addr = spawn_with(Arc::new(Ok200), 1024).await;
    let mut sock = TcpStream::connect(&addr).await.unwrap();

    // First request: HTTP/1.1 defaults to keep-alive.
    sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    sock.flush().await.unwrap();
    let r1 = read_one_response(&mut sock).await;
    assert!(r1.starts_with("HTTP/1.1 200"), "r1={r1:?}");
    assert!(
        r1.to_ascii_lowercase().contains("connection: keep-alive"),
        "r1={r1:?}"
    );
    assert!(r1.ends_with("ok"), "r1={r1:?}");

    // Second request on the SAME connection, asking to close afterwards.
    sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    sock.flush().await.unwrap();
    let r2 = read_one_response(&mut sock).await;
    assert!(r2.starts_with("HTTP/1.1 200"), "r2={r2:?}");
    assert!(
        r2.to_ascii_lowercase().contains("connection: close"),
        "r2={r2:?}"
    );
}

/// A raw keep-alive HTTP server that counts how many TCP connections it accepts.
async fn counting_keepalive_server(counter: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    // Read one request (GET, no body) up to the blank line.
                    let has_head = buf.windows(4).any(|w| w == b"\r\n\r\n");
                    if !has_head {
                        let mut b = [0u8; 512];
                        let n = match sock.read(&mut b).await {
                            Ok(0) | Err(_) => break, // client closed
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&b[..n]);
                        continue;
                    }
                    buf.clear();
                    let resp =
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok";
                    if sock.write_all(resp).await.is_err() {
                        break;
                    }
                    let _ = sock.flush().await;
                }
            });
        }
    });
    format!("http://{addr}/")
}

#[tokio::test]
async fn client_reuses_pooled_connection() {
    let counter = Arc::new(AtomicUsize::new(0));
    let url = counting_keepalive_server(counter.clone()).await;

    let client = Client::new();
    for _ in 0..3 {
        let resp = client
            .send(Request::builder(Method::Get, &url).unwrap().build())
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        // Draining the body returns the connection to the pool for reuse.
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    // All three requests should have reused a single pooled connection.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sends_100_continue_when_expected() {
    let addr = spawn_with(Arc::new(EchoBody), 1024).await;
    let mut sock = TcpStream::connect(&addr).await.unwrap();
    // Send the head with Expect: 100-continue, withholding the body.
    sock.write_all(
        b"POST / HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n",
    )
    .await
    .unwrap();
    sock.flush().await.unwrap();

    // The server must send an interim 100 Continue before the body arrives.
    let mut buf = [0u8; 64];
    let n = sock.read(&mut buf).await.unwrap();
    let interim = String::from_utf8_lossy(&buf[..n]);
    assert!(interim.starts_with("HTTP/1.1 100"), "interim={interim:?}");

    // Now send the body and read the final echoed response.
    sock.write_all(b"hi").await.unwrap();
    sock.flush().await.unwrap();
    let resp = read_one_response(&mut sock).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "resp={resp:?}");
    assert!(resp.ends_with("hi"), "resp={resp:?}");
}
