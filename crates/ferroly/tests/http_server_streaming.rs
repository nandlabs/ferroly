#![cfg(feature = "http")]
//! HTTP/1.1 request-body streaming: a handler that asks for a body stream
//! runs as soon as the head arrives and receives `Content-Length` and chunked
//! bodies piece by piece as the client sends them (with their trailers), under
//! the same size cap and body timeout as a buffered body, and can answer while
//! the body is still arriving.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    body_channel, serve_with_config, BoxFuture, HttpError, HttpHandler, HttpResponse, Request,
    ServerConfig, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Reports each body piece a handler receives, as it receives it.
struct App {
    seen: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl App {
    fn note(&self, what: String) {
        if let Some(tx) = self.seen.lock().unwrap().as_ref() {
            let _ = tx.send(what);
        }
    }
}

impl HttpHandler for App {
    fn wants_body_stream(&self, req: &Request) -> bool {
        req.uri.path != "/buffered"
    }

    fn handle(&self, mut req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            if req.uri.path == "/buffered" {
                assert!(req.take_body_stream().is_none());
                return HttpResponse::text(StatusCode::OK, format!("{}", req.body.len()));
            }
            assert!(
                req.body.is_empty(),
                "a streamed request has no buffered body"
            );
            let mut body = req.take_body_stream().expect("a body stream");
            match req.uri.path.as_str() {
                // Report every piece, then the total and the trailers.
                "/pieces" => {
                    let mut total = 0;
                    loop {
                        match body.chunk().await {
                            Ok(Some(piece)) => {
                                total += piece.len();
                                self.note(String::from_utf8_lossy(&piece).into_owned());
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let kind = match e {
                                    HttpError::BodyTooLarge(_) => "too large".to_string(),
                                    HttpError::Timeout => "timeout".to_string(),
                                    other => format!("error: {other}"),
                                };
                                self.note(kind.clone());
                                return HttpResponse::text(StatusCode::BAD_REQUEST, kind);
                            }
                        }
                    }
                    let checksum = body
                        .trailers()
                        .and_then(|t| t.get("x-checksum"))
                        .unwrap_or("none")
                        .to_string();
                    HttpResponse::text(StatusCode::OK, format!("{total} {checksum}"))
                }
                // Echo the body back as it arrives: full duplex.
                "/echo" => {
                    let (mut tx, rx) = body_channel(4);
                    tokio::spawn(async move {
                        while let Ok(Some(piece)) = body.chunk().await {
                            if tx.send(piece).await.is_err() {
                                return;
                            }
                        }
                        let _ = tx.finish().await;
                    });
                    HttpResponse::body_stream(StatusCode::OK, rx)
                }
                // Answer without reading the body.
                _ => {
                    drop(body);
                    HttpResponse::text(StatusCode(202), "not interested")
                }
            }
        })
    }
}

async fn start(config: ServerConfig) -> (std::net::SocketAddr, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Arc::new(App {
        seen: Mutex::new(Some(tx)),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(listener, app, std::future::pending::<()>(), config).await;
    });
    (addr, rx)
}

fn config() -> ServerConfig {
    ServerConfig {
        head_timeout: Duration::from_secs(5),
        body_timeout: Duration::from_secs(5),
        ..ServerConfig::default()
    }
}

async fn next_seen(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the handler saw the piece while the request was still open")
        .expect("handler alive")
}

/// Reads until `needle` has arrived (or the peer closes), returning all of it.
async fn read_until(sock: &mut TcpStream, needle: &str) -> String {
    let mut buf = Vec::new();
    let read = async {
        loop {
            if String::from_utf8_lossy(&buf).contains(needle) {
                break;
            }
            let mut b = [0u8; 1024];
            let n = sock.read(&mut b).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&b[..n]);
        }
    };
    let _ = timeout(Duration::from_secs(5), read).await;
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn chunked_body_pieces_reach_the_handler_as_they_arrive() {
    let (addr, mut seen) = start(config()).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"POST /pieces HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
    )
    .await
    .unwrap();
    // The first chunk is delivered while the rest of the body is unsent.
    assert_eq!(next_seen(&mut seen).await, "hello");
    sock.write_all(b"6\r\n world\r\n").await.unwrap();
    assert_eq!(next_seen(&mut seen).await, " world");
    // The chunked trailer section ends the stream and reaches the handler.
    sock.write_all(b"0\r\nx-checksum: abc\r\n\r\n")
        .await
        .unwrap();
    let resp = read_until(&mut sock, "11 abc").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.ends_with("11 abc"), "{resp}");
}

#[tokio::test]
async fn content_length_body_is_streamed_and_the_connection_is_reused() {
    let (addr, mut seen) = start(config()).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"POST /pieces HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabcd")
        .await
        .unwrap();
    assert_eq!(next_seen(&mut seen).await, "abcd");
    sock.write_all(b"efghij").await.unwrap();
    assert_eq!(next_seen(&mut seen).await, "efghij");
    let resp = read_until(&mut sock, "10 none").await;
    assert!(resp.contains("Connection: keep-alive"), "{resp}");

    // The fully read body leaves the connection usable for the next request,
    // here a buffered one.
    sock.write_all(b"POST /buffered HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nxyz")
        .await
        .unwrap();
    let resp = read_until(&mut sock, "\r\n\r\n3").await;
    assert!(
        resp.contains("HTTP/1.1 200") && resp.ends_with("\r\n\r\n3"),
        "{resp}"
    );
}

#[tokio::test]
async fn the_response_streams_while_the_request_body_is_still_arriving() {
    let (addr, _seen) = start(config()).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nping\r\n",
    )
    .await
    .unwrap();
    // The echo of the first chunk comes back before the second is sent.
    let first = read_until(&mut sock, "4\r\nping\r\n").await;
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");
    assert!(first.contains("Transfer-Encoding: chunked"), "{first}");
    assert!(first.ends_with("4\r\nping\r\n"), "{first}");
    sock.write_all(b"4\r\npong\r\n0\r\n\r\n").await.unwrap();
    let rest = read_until(&mut sock, "0\r\n\r\n").await;
    assert_eq!(rest, "4\r\npong\r\n0\r\n\r\n");
}

#[tokio::test]
async fn an_over_cap_chunked_body_fails_the_stream_with_body_too_large() {
    let (addr, mut seen) = start(ServerConfig {
        max_body_bytes: 8,
        ..config()
    })
    .await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"POST /pieces HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n10\r\n",
    )
    .await
    .unwrap();
    assert_eq!(next_seen(&mut seen).await, "abcd");
    assert_eq!(next_seen(&mut seen).await, "too large");
    let resp = read_until(&mut sock, "too large").await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    // The framing is no longer trusted, so the connection is not kept.
    assert!(resp.contains("Connection: close"), "{resp}");

    // A declared Content-Length over the cap is still refused up front.
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"POST /pieces HTTP/1.1\r\nHost: x\r\nContent-Length: 9\r\n\r\n")
        .await
        .unwrap();
    let resp = read_until(&mut sock, "payload too large").await;
    assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
}

#[tokio::test]
async fn a_stalled_body_times_out_into_the_stream() {
    let (addr, mut seen) = start(ServerConfig {
        body_timeout: Duration::from_millis(200),
        ..config()
    })
    .await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"POST /pieces HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab")
        .await
        .unwrap();
    assert_eq!(next_seen(&mut seen).await, "ab");
    assert_eq!(next_seen(&mut seen).await, "timeout");
    let resp = read_until(&mut sock, "timeout").await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
}

#[tokio::test]
async fn a_handler_that_ignores_the_body_answers_and_closes() {
    let (addr, _seen) = start(config()).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"POST /ignore HTTP/1.1\r\nHost: x\r\nContent-Length: 100000\r\n\r\npartial")
        .await
        .unwrap();
    // The handler answers without waiting for the body; the rest of the
    // body is never read, so the connection is not reused.
    let resp = read_until(&mut sock, "not interested").await;
    assert!(resp.starts_with("HTTP/1.1 202"), "{resp}");
    assert!(resp.contains("Connection: close"), "{resp}");
}

/// HTTP-14: the bytes of a body the handler abandoned are never parsed as
/// a request, even when they arrive in full with the head and look like
/// one, and a request pipelined after them is not served on that
/// connection either (the connection closes after the one answer).
#[tokio::test]
async fn an_abandoned_body_is_never_parsed_as_a_pipelined_request() {
    let (addr, _seen) = start(config()).await;
    let smuggled = "POST /buffered HTTP/1.1\r\nHost: x\r\nContent-Length: 8\r\n\r\nSMUGGLED";
    let mut wire = format!(
        "POST /ignore HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{smuggled}",
        smuggled.len()
    );
    wire.push_str("GET /buffered HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(wire.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = timeout(Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
    let resp = String::from_utf8_lossy(&buf);
    assert!(resp.starts_with("HTTP/1.1 202"), "{resp}");
    assert_eq!(resp.matches("HTTP/1.1 ").count(), 1, "{resp}");
    assert!(!resp.contains("HTTP/1.1 200"), "{resp}");
}

#[tokio::test]
async fn a_truncated_body_is_an_error_not_a_short_success() {
    let (addr, mut seen) = start(config()).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"POST /pieces HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc")
        .await
        .unwrap();
    assert_eq!(next_seen(&mut seen).await, "abc");
    sock.shutdown().await.unwrap();
    let outcome = next_seen(&mut seen).await;
    assert!(outcome.starts_with("error: "), "{outcome}");
}
