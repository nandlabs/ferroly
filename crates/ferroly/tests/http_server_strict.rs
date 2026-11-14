#![cfg(feature = "http")]
//! Regression tests for the HTTP server's strict parsing, limits, timeouts and
//! panic isolation, driven over raw TCP so malformed input reaches the parser
//! exactly as written.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    serve_with_config, BoxFuture, ErrorHook, HttpError, HttpHandler, HttpResponse, Request,
    ServerConfig, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// Generous bound for "this must finish"; real waits are far shorter.
const PATIENCE: Duration = Duration::from_secs(10);

/// Echoes `METHOD PATH?QUERY|host|body` and counts the requests it handled.
#[derive(Default)]
struct Echo {
    hits: AtomicUsize,
}

impl HttpHandler for Echo {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let text = format!(
                "{} {}|{}|{}",
                req.method.as_str(),
                req.uri.request_target(),
                req.uri.host,
                String::from_utf8_lossy(&req.body)
            );
            HttpResponse::text(StatusCode::OK, text)
        })
    }
}

fn test_config() -> ServerConfig {
    ServerConfig {
        max_body_bytes: 1024,
        max_connections: 8,
        head_timeout: Duration::from_secs(5),
        body_timeout: Duration::from_secs(5),
        drain_timeout: Duration::from_secs(1),
        ..ServerConfig::default()
    }
}

async fn spawn(handler: Arc<dyn HttpHandler>, config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(listener, handler, std::future::pending::<()>(), config).await;
    });
    addr
}

async fn spawn_echo() -> (SocketAddr, Arc<Echo>) {
    let echo = Arc::new(Echo::default());
    (spawn(echo.clone(), test_config()).await, echo)
}

/// Sends `raw` and returns everything the server writes until it closes.
async fn send_raw(addr: SocketAddr, raw: &[u8]) -> String {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(raw).await.unwrap();
    read_to_close(&mut sock).await
}

async fn read_to_close(sock: &mut TcpStream) -> String {
    let mut out = Vec::new();
    timeout(PATIENCE, sock.read_to_end(&mut out))
        .await
        .expect("server did not close the connection")
        .unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads exactly one response (head + `Content-Length` body) from `sock`.
async fn read_one(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            let end = pos + 4 + len;
            while buf.len() < end {
                let mut b = [0u8; 1024];
                let n = timeout(PATIENCE, sock.read(&mut b)).await.unwrap().unwrap();
                assert!(n > 0, "connection closed mid-response");
                buf.extend_from_slice(&b[..n]);
            }
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        let mut b = [0u8; 1024];
        let n = timeout(PATIENCE, sock.read(&mut b)).await.unwrap().unwrap();
        assert!(n > 0, "connection closed before a response: {buf:?}");
        buf.extend_from_slice(&b[..n]);
    }
}

fn status_of(resp: &str) -> &str {
    resp.get(..12).unwrap_or(resp)
}

fn status_lines(resp: &str) -> usize {
    resp.matches("HTTP/1.1 ").count()
}

// --- P0-5: Transfer-Encoding and request-line strictness -------------------

#[tokio::test]
async fn smuggling_via_second_transfer_encoding_header_is_refused() {
    let (addr, echo) = spawn_echo().await;
    // The classic TE.TE desync: a lenient parser reads the first header
    // ("gzip", not chunked), treats the body as empty, and then parses the
    // chunk stream as a second request.
    let raw = "POST /upload HTTP/1.1\r\nHost: x\r\n\
               Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n\
               0\r\n\r\nGET /admin HTTP/1.1\r\nHost: x\r\n\r\n";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 501", "{resp:?}");
    assert_eq!(
        status_lines(&resp),
        1,
        "a smuggled request was answered: {resp:?}"
    );
    assert!(resp.to_ascii_lowercase().contains("connection: close"));
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0, "handler ran");
}

#[tokio::test]
async fn transfer_encoding_must_be_exactly_chunked() {
    let (addr, echo) = spawn_echo().await;
    for te in [
        "Transfer-Encoding: xchunked",
        "Transfer-Encoding: chunkedx",
        "Transfer-Encoding: gzip, chunked",
        "Transfer-Encoding: chunked, gzip",
        "Transfer-Encoding: chunked, chunked",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked",
        "Transfer-Encoding: identity",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: identity",
        "Transfer-Encoding:",
    ] {
        let raw = format!(
            "POST / HTTP/1.1\r\nHost: x\r\n{te}\r\n\r\n5\r\nhello\r\n0\r\n\r\n\
             GET /admin HTTP/1.1\r\nHost: x\r\n\r\n"
        );
        let resp = send_raw(addr, raw.as_bytes()).await;
        assert_eq!(status_of(&resp), "HTTP/1.1 501", "{te}: {resp:?}");
        assert_eq!(status_lines(&resp), 1, "{te}: {resp:?}");
    }
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);

    // The one accepted spelling still works (any case, optional whitespace).
    let raw = "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
               Transfer-Encoding:  Chunked \r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 200", "{resp:?}");
    assert!(resp.ends_with("POST /|x|hello"), "{resp:?}");
}

#[tokio::test]
async fn content_length_with_transfer_encoding_stays_rejected() {
    let (addr, echo) = spawn_echo().await;
    for raw in [
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 0\r\n\r\n0\r\n\r\n",
    ] {
        let resp = send_raw(addr, raw.as_bytes()).await;
        assert_eq!(status_of(&resp), "HTTP/1.1 400", "{resp:?}");
        assert_eq!(status_lines(&resp), 1);
    }
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_request_lines_are_rejected() {
    let (addr, echo) = spawn_echo().await;
    for (line, expect) in [
        ("G(ET / HTTP/1.1", "400"),
        ("GE\tT / HTTP/1.1", "400"),
        ("GET /", "400"),
        ("GET / ", "400"),
        ("GET  / HTTP/1.1", "400"),
        ("GET / HTTP/1.1 extra", "400"),
        ("GET / http/1.1", "400"),
        ("GET / HTTP/1", "400"),
        ("GET / HTTP/1.1.1", "400"),
        ("GET / FTP/1.1", "400"),
        ("GET nopath HTTP/1.1", "400"),
        ("GET /a#frag HTTP/1.1", "400"),
        ("GET /a\x01b HTTP/1.1", "400"),
        ("GET * HTTP/1.1", "400"),
        ("GET / HTTP/2.0", "505"),
        ("GET / HTTP/0.9", "505"),
    ] {
        let raw = format!("{line}\r\nHost: x\r\n\r\n");
        let resp = send_raw(addr, raw.as_bytes()).await;
        assert_eq!(
            status_of(&resp),
            format!("HTTP/1.1 {expect}"),
            "{line:?}: {resp:?}"
        );
    }
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http10_is_not_kept_alive_by_default() {
    let (addr, echo) = spawn_echo().await;
    // No Connection header: an HTTP/1.0 client expects the server to close.
    // (`send_raw` only returns once the server has closed.) No Host needed.
    let resp = send_raw(addr, b"GET /one HTTP/1.0\r\n\r\n").await;
    assert_eq!(status_of(&resp), "HTTP/1.1 200", "{resp:?}");
    assert!(
        resp.to_ascii_lowercase().contains("connection: close"),
        "{resp:?}"
    );

    // With an explicit keep-alive the connection persists.
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"GET /two HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")
        .await
        .unwrap();
    let r1 = read_one(&mut sock).await;
    assert!(
        r1.to_ascii_lowercase().contains("connection: keep-alive"),
        "{r1:?}"
    );
    sock.write_all(b"GET /three HTTP/1.0\r\n\r\n")
        .await
        .unwrap();
    let r2 = read_to_close(&mut sock).await;
    assert!(r2.contains("GET /three"), "{r2:?}");
    assert_eq!(echo.hits.load(Ordering::SeqCst), 3);

    // Chunked framing does not exist in HTTP/1.0.
    let raw = "POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 400", "{resp:?}");
}

// --- H1: strict header and chunk parsing -----------------------------------

#[tokio::test]
async fn malformed_headers_are_rejected() {
    let (addr, echo) = spawn_echo().await;
    for headers in [
        "Host : x",                                          // whitespace before the colon
        "Host: x\r\nX-A : 1",                                //
        "Host: x\r\nX-A\t: 1",                               //
        "Host: x\r\nContent-Length: +5",                     // signed
        "Host: x\r\nContent-Length: -5",                     //
        "Host: x\r\nContent-Length: 5, 5",                   // list
        "Host: x\r\nContent-Length: 0x5",                    // not decimal
        "Host: x\r\nContent-Length: five",                   //
        "Host: x\r\nContent-Length: 5.0",                    //
        "Host: x\r\nContent-Length:",                        // empty
        "Host: x\r\nContent-Length: 5\r\nContent-Length: 6", // conflicting duplicates
        "Host: x\r\nno-colon-here",                          // no colon
        "Host: x\r\nX-A: 1\r\n folded",                      // obs-fold (space)
        "Host: x\r\nX-A: 1\r\n\tfolded",                     // obs-fold (tab)
        "Host: x\r\nX-A: a\rb",                              // bare CR in a value
        "Host: x\r\nX-A: a\0b",                              // NUL in a value
        "Host: x\r\n: no-name",                              // empty field name
        "Host: x\r\nX A: 1",                                 // space in a field name
        "X-A: 1",                                            // HTTP/1.1 without Host
        "Host: x\r\nHost: y",                                // duplicate Host
    ] {
        let raw = format!("POST / HTTP/1.1\r\n{headers}\r\n\r\nhello");
        let resp = send_raw(addr, raw.as_bytes()).await;
        assert_eq!(status_of(&resp), "HTTP/1.1 400", "{headers:?}: {resp:?}");
        assert_eq!(status_lines(&resp), 1, "{headers:?}");
    }
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);

    // Identical duplicate Content-Length values are one value.
    let raw = "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
               Content-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert!(resp.ends_with("POST /|x|hello"), "{resp:?}");
}

#[tokio::test]
async fn bare_lf_line_endings_are_rejected() {
    let (addr, echo) = spawn_echo().await;
    for raw in [
        "GET / HTTP/1.1\nHost: x\n\n",
        "GET / HTTP/1.1\r\nHost: x\n\r\n",
        "GET / HTTP/1.1\r\nHost: x\r\n\n",
        "GET / HTTP/1.1\nHost: x\r\n\r\n",
    ] {
        let resp = send_raw(addr, raw.as_bytes()).await;
        assert_eq!(status_of(&resp), "HTTP/1.1 400", "{raw:?}: {resp:?}");
    }
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_chunked_bodies_are_rejected() {
    let (addr, echo) = spawn_echo().await;
    for body in [
        "+5\r\nhello\r\n0\r\n\r\n",               // signed chunk size
        "-5\r\nhello\r\n0\r\n\r\n",               //
        "0x5\r\nhello\r\n0\r\n\r\n",              // prefixed
        "5g\r\nhello\r\n0\r\n\r\n",               // non-hex
        " 5\r\nhello\r\n0\r\n\r\n",               // leading whitespace
        "5 \r\nhello\r\n0\r\n\r\n",               // trailing whitespace
        "\r\nhello\r\n0\r\n\r\n",                 // empty size
        "5\r\nhelloXX0\r\n\r\n",                  // data not followed by CRLF
        "5\r\nhello\n\n0\r\n\r\n",                // ... but by two LFs
        "5\r\nhello\r\r0\r\n\r\n",                // ... or two CRs
        "5\nhello\r\n0\r\n\r\n",                  // bare LF after the size
        "5\r\nhello\r\n0\r\nbad trailer\r\n\r\n", // trailer without a colon
        "5\r\nhello\r\n0\r\nT : v\r\n\r\n",       // space before the trailer colon
        "11111111111111111\r\n",                  // size overflows 64 bits
    ] {
        let raw = format!(
            "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n{body}\
             GET /admin HTTP/1.1\r\nHost: x\r\n\r\n"
        );
        let resp = send_raw(addr, raw.as_bytes()).await;
        assert_eq!(status_of(&resp), "HTTP/1.1 400", "{body:?}: {resp:?}");
        assert_eq!(status_lines(&resp), 1, "{body:?}: {resp:?}");
    }
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);

    // Chunk extensions and well-formed trailers are accepted (and ignored).
    let raw = "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
               Transfer-Encoding: chunked\r\n\r\n\
               5;name=value\r\nhello\r\n1 ;x\r\n!\r\n0\r\nX-Trailer: v\r\n\r\n";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert!(resp.ends_with("POST /|x|hello!"), "{resp:?}");
}

// --- H2: chunk-size lines and trailers are bounded --------------------------

#[tokio::test]
async fn chunk_size_lines_and_trailers_are_capped() {
    let echo = Arc::new(Echo::default());
    let config = ServerConfig {
        max_body_bytes: 16,
        max_headers: 8,
        max_header_bytes: 512,
        ..test_config()
    };
    let addr = spawn(echo.clone(), config).await;
    let head = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n";

    // A chunk-size line padded with a huge extension: megabytes on the wire
    // for a body capped at 16 bytes. Refused at the line limit.
    let mut raw = head.as_bytes().to_vec();
    raw.extend_from_slice(b"5;");
    raw.extend(std::iter::repeat(b'a').take(64 * 1024));
    raw.extend_from_slice(b"\r\nhello\r\n0\r\n\r\n");
    let mut sock = TcpStream::connect(addr).await.unwrap();
    // The server may close while we are still writing; ignore that.
    let _ = sock.write_all(&raw).await;
    let resp = read_one(&mut sock).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 400", "{resp:?}");

    // Too many trailer lines.
    let mut raw = format!("{head}5\r\nhello\r\n0\r\n");
    for i in 0..9 {
        raw.push_str(&format!("T{i}: v\r\n"));
    }
    raw.push_str("\r\n");
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 400", "{resp:?}");

    // Too many trailer bytes.
    let raw = format!("{head}5\r\nhello\r\n0\r\nT: {}\r\n\r\n", "v".repeat(600));
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 400", "{resp:?}");

    // A declared chunk beyond the body cap is refused before it is read.
    let raw = format!("{head}11\r\n");
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 413", "{resp:?}");
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn header_limits_are_configurable() {
    let config = ServerConfig {
        max_headers: 4,
        max_header_bytes: 256,
        max_line_bytes: 96,
        ..test_config()
    };
    let addr = spawn(Arc::new(Echo::default()), config).await;

    let ok = "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nA: 1\r\nB: 2\r\n\r\n";
    assert_eq!(
        status_of(&send_raw(addr, ok.as_bytes()).await),
        "HTTP/1.1 200"
    );

    let many = "GET / HTTP/1.1\r\nHost: x\r\nA: 1\r\nB: 2\r\nC: 3\r\nD: 4\r\n\r\n";
    assert_eq!(
        status_of(&send_raw(addr, many.as_bytes()).await),
        "HTTP/1.1 431"
    );

    let long_line = format!(
        "GET / HTTP/1.1\r\nHost: x\r\nA: {}\r\n\r\n",
        "a".repeat(100)
    );
    assert_eq!(
        status_of(&send_raw(addr, long_line.as_bytes()).await),
        "HTTP/1.1 431"
    );

    let long_target = format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(100));
    assert_eq!(
        status_of(&send_raw(addr, long_target.as_bytes()).await),
        "HTTP/1.1 414"
    );

    let big_block = format!(
        "GET / HTTP/1.1\r\nHost: x\r\nA: {a}\r\nB: {a}\r\nC: {a}\r\n\r\n",
        a = "a".repeat(80)
    );
    assert_eq!(
        status_of(&send_raw(addr, big_block.as_bytes()).await),
        "HTTP/1.1 431"
    );
}

// --- H3: timeouts -------------------------------------------------------------

struct BigBody;
impl HttpHandler for BigBody {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            if req.uri.path == "/big" {
                HttpResponse::new(StatusCode::OK).body(vec![b'x'; 32 * 1024 * 1024])
            } else {
                HttpResponse::text(StatusCode::OK, "small")
            }
        })
    }
}

#[tokio::test]
async fn non_reading_client_is_disconnected_and_releases_its_slot() {
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = errors.clone();
    let config = ServerConfig {
        max_connections: 1,
        write_timeout: Duration::from_millis(200),
        on_error: Some(ErrorHook::new(move |_peer, err| {
            seen.lock().unwrap().push(err.to_string());
        })),
        ..test_config()
    };
    let addr = spawn(Arc::new(BigBody), config).await;

    // Ask for 32 MiB and never read a byte of it.
    let mut stalled = TcpStream::connect(addr).await.unwrap();
    stalled
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();

    // With a single connection slot, this request is only served once the
    // stalled connection has been dropped by the write timeout.
    let resp = timeout(
        PATIENCE,
        send_raw(
            addr,
            b"GET /small HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        ),
    )
    .await
    .expect("the stalled client still holds the only connection slot");
    assert!(resp.ends_with("small"), "{resp:?}");
    assert!(
        errors.lock().unwrap().iter().any(|e| e == "timed out"),
        "errors: {:?}",
        errors.lock().unwrap()
    );
    drop(stalled);
}

#[tokio::test]
async fn idle_keep_alive_connection_is_closed() {
    let config = ServerConfig {
        idle_timeout: Duration::from_millis(150),
        ..test_config()
    };
    let addr = spawn(Arc::new(Echo::default()), config).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let resp = read_one(&mut sock).await;
    assert!(resp.to_ascii_lowercase().contains("connection: keep-alive"));
    // No second request: the server closes the idle connection by itself,
    // silently (an idle close is not an error and gets no response).
    let rest = timeout(Duration::from_secs(5), read_to_close(&mut sock))
        .await
        .expect("idle connection was not closed");
    assert_eq!(rest, "");
}

#[tokio::test]
async fn slow_head_and_slow_body_get_408() {
    let config = ServerConfig {
        head_timeout: Duration::from_millis(150),
        body_timeout: Duration::from_millis(150),
        ..test_config()
    };
    let echo = Arc::new(Echo::default());
    let addr = spawn(echo.clone(), config).await;

    // Half a request head, then silence.
    let resp = send_raw(addr, b"GET / HTTP/1.1\r\nHost: x\r\n").await;
    assert_eq!(status_of(&resp), "HTTP/1.1 408", "{resp:?}");

    // A complete head announcing ten body bytes, of which two arrive.
    let resp = send_raw(
        addr,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab",
    )
    .await;
    assert_eq!(status_of(&resp), "HTTP/1.1 408", "{resp:?}");

    // A connection that never sends anything is closed without a response.
    let mut sock = TcpStream::connect(addr).await.unwrap();
    assert_eq!(read_to_close(&mut sock).await, "");
    assert_eq!(echo.hits.load(Ordering::SeqCst), 0);
}

struct Sleepy;
impl HttpHandler for Sleepy {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            if req.uri.path == "/slow" {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            HttpResponse::text(StatusCode::OK, "done")
        })
    }
}

#[tokio::test]
async fn handler_timeout_answers_503_and_keeps_serving() {
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = errors.clone();
    let config = ServerConfig {
        handler_timeout: Some(Duration::from_millis(100)),
        on_error: Some(ErrorHook::new(move |_peer, err| {
            seen.lock().unwrap().push(err.to_string());
        })),
        ..test_config()
    };
    let addr = spawn(Arc::new(Sleepy), config).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let resp = read_one(&mut sock).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 503", "{resp:?}");
    // The same connection serves the next request.
    sock.write_all(b"GET /fast HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let resp = read_to_close(&mut sock).await;
    assert!(
        resp.starts_with("HTTP/1.1 200") && resp.ends_with("done"),
        "{resp:?}"
    );
    assert_eq!(*errors.lock().unwrap(), ["timed out"]);
}

// --- A1: panic isolation and the error hook ------------------------------------

struct Panicky;
impl HttpHandler for Panicky {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        if req.uri.path == "/panic-sync" {
            panic!("sync boom");
        }
        Box::pin(async move {
            if req.uri.path == "/panic" {
                tokio::task::yield_now().await;
                panic!("async boom");
            }
            HttpResponse::text(StatusCode::OK, "alive")
        })
    }
}

#[tokio::test]
async fn handler_panic_yields_500_and_the_server_stays_up() {
    type Reported = Vec<(Option<SocketAddr>, String)>;
    let errors: Arc<Mutex<Reported>> = Arc::default();
    let seen = errors.clone();
    let config = ServerConfig {
        on_error: Some(ErrorHook::new(move |peer, err| {
            assert!(matches!(err, HttpError::HandlerPanic(_)), "{err:?}");
            seen.lock().unwrap().push((peer, err.to_string()));
        })),
        ..test_config()
    };
    let addr = spawn(Arc::new(Panicky), config).await;

    let mut sock = TcpStream::connect(addr).await.unwrap();
    let local = sock.local_addr().unwrap();
    for path in ["/panic", "/panic-sync"] {
        sock.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let resp = read_one(&mut sock).await;
        assert_eq!(status_of(&resp), "HTTP/1.1 500", "{path}: {resp:?}");
        // The panic message is not disclosed to the client.
        assert!(!resp.contains("boom"), "{resp:?}");
    }
    // Same connection, next request: still served.
    sock.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    assert!(read_one(&mut sock).await.ends_with("alive"));
    // And so is a brand-new connection.
    let resp = send_raw(
        addr,
        b"GET /ok HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp.ends_with("alive"), "{resp:?}");

    let errors = errors.lock().unwrap();
    assert_eq!(
        *errors,
        [
            (Some(local), "handler panicked: async boom".to_string()),
            (Some(local), "handler panicked: sync boom".to_string()),
        ]
    );
}

#[tokio::test]
async fn connection_errors_reach_the_error_hook() {
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = errors.clone();
    let config = ServerConfig {
        on_error: Some(ErrorHook::new(move |peer, err| {
            assert!(peer.is_some());
            seen.lock().unwrap().push(err.to_string());
            // A misbehaving hook must not hurt the server either.
            panic!("hook boom");
        })),
        ..test_config()
    };
    let addr = spawn(Arc::new(Echo::default()), config).await;
    let resp = send_raw(addr, b"GET / HTTP/1.1\r\nHost : x\r\n\r\n").await;
    assert_eq!(status_of(&resp), "HTTP/1.1 400");
    let resp = send_raw(
        addr,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status_of(&resp), "HTTP/1.1 200");
    assert_eq!(
        *errors.lock().unwrap(),
        ["malformed http: invalid header name"]
    );

    // Refusals made after the head parsed (smuggling attempts, oversized and
    // malformed bodies) are reported too — once each.
    errors.lock().unwrap().clear();
    for raw in [
        "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 999999\r\n\r\n",
        "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n+1\r\nx\r\n0\r\n\r\n",
    ] {
        send_raw(addr, raw.as_bytes()).await;
    }
    assert_eq!(
        *errors.lock().unwrap(),
        [
            "malformed http: unsupported Transfer-Encoding",
            "malformed http: conflicting Content-Length and Transfer-Encoding",
            "body exceeds the configured limit of 1024 bytes",
            "malformed http: bad chunk size",
        ]
    );
}

// --- Capabilities -------------------------------------------------------------

struct Describe;
impl HttpHandler for Describe {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            let text = format!(
                "peer={} local={} tls={} certs={} path={}",
                req.peer_addr().map(|a| a.to_string()).unwrap_or_default(),
                req.local_addr().map(|a| a.to_string()).unwrap_or_default(),
                req.is_tls(),
                req.peer_certificates().len(),
                req.uri.path,
            );
            let mut resp = HttpResponse::text(StatusCode::OK, text);
            if req.uri.path == "/close" {
                resp = resp.header("connection", "close");
            }
            resp
        })
    }
}

#[tokio::test]
async fn request_exposes_peer_address_and_response_has_a_date() {
    let addr = spawn(Arc::new(Describe), test_config()).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    let local = sock.local_addr().unwrap();
    sock.write_all(b"GET /who HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let resp = read_one(&mut sock).await;
    assert!(
        resp.ends_with(&format!(
            "peer={local} local={addr} tls=false certs=0 path=/who"
        )),
        "{resp:?}"
    );

    // IMF-fixdate: `Date: Sun, 06 Nov 1994 08:49:37 GMT`.
    let date = resp
        .lines()
        .find_map(|l| l.strip_prefix("Date: "))
        .unwrap_or_else(|| panic!("no Date header in {resp:?}"));
    assert_eq!(date.len(), 29, "{date:?}");
    assert!(
        date.ends_with(" GMT") && date.as_bytes()[3] == b',',
        "{date:?}"
    );

    // A handler's `Connection: close` really closes the connection.
    sock.write_all(b"GET /close HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let resp = read_to_close(&mut sock).await;
    assert!(resp.contains("path=/close"), "{resp:?}");
    assert!(
        !resp.to_ascii_lowercase().contains("keep-alive"),
        "{resp:?}"
    );
}

#[tokio::test]
async fn continue_is_sent_only_after_the_size_check() {
    let (addr, echo) = spawn_echo().await; // max_body_bytes = 1024
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"POST / HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 4096\r\n\r\n",
    )
    .await
    .unwrap();
    // Too large: the final status arrives without an interim 100 Continue.
    let resp = read_to_close(&mut sock).await;
    assert_eq!(status_of(&resp), "HTTP/1.1 413", "{resp:?}");
    assert!(!resp.contains("100 Continue"), "{resp:?}");

    // Within the limit: 100 Continue first, then the real response.
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n",
    )
    .await
    .unwrap();
    let mut interim = [0u8; 25];
    timeout(PATIENCE, sock.read_exact(&mut interim))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    sock.write_all(b"hi").await.unwrap();
    let resp = read_to_close(&mut sock).await;
    assert!(
        resp.starts_with("HTTP/1.1 200") && resp.ends_with("|hi"),
        "{resp:?}"
    );

    // An expectation the server does not know is refused, not ignored.
    let resp = send_raw(
        addr,
        b"POST / HTTP/1.1\r\nHost: x\r\nExpect: 200-ok\r\nContent-Length: 2\r\n\r\nhi",
    )
    .await;
    assert_eq!(status_of(&resp), "HTTP/1.1 417", "{resp:?}");
    assert_eq!(echo.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_targets_are_reduced_to_origin_form() {
    let (addr, _echo) = spawn_echo().await;
    // absolute-form: the path is extracted and the target's authority wins
    // over the Host header.
    let raw =
        "GET http://real.example:8080/a/b?q=1 HTTP/1.1\r\nHost: other\r\nConnection: close\r\n\r\n";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert!(
        resp.ends_with("GET /a/b?q=1|real.example:8080|"),
        "{resp:?}"
    );

    let raw = "OPTIONS * HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert!(resp.ends_with("OPTIONS *|x|"), "{resp:?}");

    // One leading empty line before the request line is tolerated.
    let raw = "\r\nGET /lead HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = send_raw(addr, raw.as_bytes()).await;
    assert!(resp.ends_with("GET /lead|x|"), "{resp:?}");
}

#[tokio::test]
async fn path_normalisation_is_opt_in() {
    let raw = |target: &str| {
        format!("GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").into_bytes()
    };

    // Off (the default): the handler sees the path exactly as sent.
    let (addr, _echo) = spawn_echo().await;
    let resp = send_raw(addr, &raw("/a/../b/%41//c")).await;
    assert!(resp.ends_with("GET /a/../b/%41//c|x|"), "{resp:?}");

    let config = ServerConfig {
        normalize_paths: true,
        ..test_config()
    };
    let echo = Arc::new(Echo::default());
    let addr = spawn(echo.clone(), config).await;
    let resp = send_raw(addr, &raw("/a/./x/../b/%41%2e?k=%41/../")).await;
    // The path is normalised; the query is left alone.
    assert!(resp.ends_with("GET /a/b/A.?k=%41/../|x|"), "{resp:?}");
    let resp = send_raw(addr, &raw("/api/%2e%2e/admin")).await;
    assert!(resp.ends_with("GET /admin|x|"), "{resp:?}");
    assert_eq!(echo.hits.load(Ordering::SeqCst), 2);
    for bad in [
        "//api/secret",
        "/api//secret",
        "/a/%zz",
        "/a/%00",
        "/a/%0d%0a",
    ] {
        let resp = send_raw(addr, &raw(bad)).await;
        assert_eq!(status_of(&resp), "HTTP/1.1 400", "{bad}: {resp:?}");
    }
    assert_eq!(echo.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn keep_alive_can_be_disabled_and_is_capped() {
    let config = ServerConfig {
        keep_alive: false,
        ..test_config()
    };
    let addr = spawn(Arc::new(Echo::default()), config).await;
    let resp = send_raw(addr, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(
        resp.to_ascii_lowercase().contains("connection: close"),
        "{resp:?}"
    );

    let config = ServerConfig {
        max_keep_alive_requests: 2,
        ..test_config()
    };
    let addr = spawn(Arc::new(Echo::default()), config).await;
    let two = b"GET /1 HTTP/1.1\r\nHost: x\r\n\r\nGET /2 HTTP/1.1\r\nHost: x\r\n\r\nGET /3 HTTP/1.1\r\nHost: x\r\n\r\n";
    let resp = send_raw(addr, two).await;
    // Two pipelined requests are answered, then the connection is closed.
    assert_eq!(status_lines(&resp), 2, "{resp:?}");
    assert!(resp.contains("GET /1") && resp.contains("GET /2") && !resp.contains("GET /3"));
}

#[tokio::test]
async fn head_responses_have_no_body_and_keep_framing_in_sync() {
    let (addr, _echo) = spawn_echo().await;
    let raw = b"HEAD /h HTTP/1.1\r\nHost: x\r\n\r\nGET /g HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = send_raw(addr, raw).await;
    assert_eq!(status_lines(&resp), 2, "{resp:?}");
    assert!(
        !resp.contains("HEAD /h|"),
        "HEAD response carried a body: {resp:?}"
    );
    assert!(resp.ends_with("GET /g|x|"), "{resp:?}");
}

struct Mirror;
impl HttpHandler for Mirror {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move { HttpResponse::new(StatusCode::OK).body(req.body) })
    }
}

#[tokio::test]
async fn large_bodies_round_trip_between_client_and_server() {
    use ferroly::http::{Client, Method};

    let config = ServerConfig {
        max_body_bytes: 4 * 1024 * 1024,
        ..test_config()
    };
    let addr = spawn(Arc::new(Mirror), config).await;
    let url = format!("http://{addr}/mirror");
    // Larger than every internal slice and buffer size, and not a multiple
    // of any of them.
    let payload: Vec<u8> = (0..1_000_003u32).map(|i| (i % 251) as u8).collect();
    let client = Client::new();

    // Buffered upload (Content-Length) and a large fixed-length response.
    let req = Request::builder(Method::Post, &url)
        .unwrap()
        .body(payload.clone())
        .build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.headers().content_length(), Some(payload.len() as u64));
    assert!(resp.bytes().await.unwrap() == payload);

    // Streamed upload (chunked), dechunked by the server, on the same client.
    let req = Request::builder(Method::Post, &url).unwrap().build();
    let resp = client.send_streaming(req, &payload[..]).await.unwrap();
    assert!(resp.bytes().await.unwrap() == payload);

    // The same upload against a smaller body cap is refused with 413.
    let small = spawn(Arc::new(Mirror), test_config()).await; // 1024-byte cap
    let req = Request::builder(Method::Post, &format!("http://{small}/mirror"))
        .unwrap()
        .body(payload.clone())
        .build();
    // The server answers and closes while the client is still uploading, so
    // the client sees either the 413 or a broken connection — never a 200.
    match client.send(req).await {
        Ok(resp) => assert_eq!(resp.status().as_u16(), 413),
        Err(e) => assert!(
            matches!(e, HttpError::Io(_) | HttpError::Protocol(_)),
            "{e:?}"
        ),
    }
}

// --- H8: Server-Sent Events cannot be forged through event text ------------------

struct EventSource;
impl HttpHandler for EventSource {
    fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            let (tx, rx) = tokio::sync::mpsc::channel::<String>(8);
            tokio::spawn(async move {
                for text in [
                    "plain",
                    "two\nlines",
                    // Text that tries to end the event and start a forged one.
                    "evil\r\n\r\nevent: admin\rdata: forged\n\nid: 7",
                    "last",
                ] {
                    if tx.send(text.to_string()).await.is_err() {
                        return;
                    }
                }
            });
            HttpResponse::event_stream(rx)
        })
    }
}

#[tokio::test]
async fn event_stream_text_cannot_inject_fields_or_events() {
    use ferroly::http::{Client, Method, SseDecoder};

    let addr = spawn(Arc::new(EventSource), test_config()).await;
    let req = Request::builder(Method::Get, &format!("http://{addr}/events"))
        .unwrap()
        .build();
    let mut resp = Client::new().send(req).await.unwrap();
    assert_eq!(
        resp.headers().get("content-type"),
        Some("text/event-stream")
    );

    let mut decoder = SseDecoder::new();
    let mut events = Vec::new();
    while let Some(chunk) = resp.chunk().await.unwrap() {
        events.extend(decoder.push(&chunk));
    }
    // Four strings in, exactly four events out, none with a forged field.
    let data: Vec<&str> = events.iter().map(|e| e.data.as_str()).collect();
    assert_eq!(
        data,
        [
            "plain",
            "two\nlines",
            "evil\n\nevent: admin\ndata: forged\n\nid: 7",
            "last",
        ]
    );
    assert!(events.iter().all(|e| e.event.is_none() && e.id.is_none()));
}
