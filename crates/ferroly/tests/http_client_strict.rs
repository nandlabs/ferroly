#![cfg(feature = "http")]
//! Regression tests for the HTTP client's hardening and features, driven
//! against scripted raw-TCP servers so every byte the "server" sends (or
//! withholds) is under the test's control.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    Client, HeaderMap, HttpError, Interceptor, Method, Multipart, Proxy, Request, StatusCode, Uri,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const PATIENCE: Duration = Duration::from_secs(10);
const OK_HELLO: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";

/// Reads one request (head, plus a `Content-Length` or chunked body) and
/// returns it verbatim; `None` when the client closed the connection.
async fn read_request(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
            let chunked = head.contains("transfer-encoding: chunked");
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            loop {
                let done = if chunked {
                    buf[pos..].ends_with(b"0\r\n\r\n")
                } else {
                    buf.len() >= pos + 4 + len
                };
                if done {
                    return Some(String::from_utf8_lossy(&buf).into_owned());
                }
                let mut b = [0u8; 4096];
                let n = sock.read(&mut b).await.ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&b[..n]);
            }
        }
        let mut b = [0u8; 4096];
        let n = sock.read(&mut b).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&b[..n]);
    }
}

/// What a scripted server observed.
#[derive(Default)]
struct Seen {
    connections: AtomicUsize,
    requests: Mutex<Vec<String>>,
}

impl Seen {
    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
    fn record(&self, req: &str) {
        self.requests.lock().unwrap().push(req.to_string());
    }
}

/// Starts a server that runs `script(connection_index, socket, seen)` for each
/// accepted connection.
async fn scripted<F, Fut>(script: F) -> (SocketAddr, Arc<Seen>)
where
    F: Fn(usize, TcpStream, Arc<Seen>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Seen::default());
    let shared = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let index = shared.connections.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(script(index, sock, shared.clone()));
        }
    });
    (addr, seen)
}

/// A keep-alive server answering every request with `response`.
async fn canned(response: &'static [u8]) -> (SocketAddr, Arc<Seen>) {
    scripted(move |_, mut sock, seen| async move {
        while let Some(req) = read_request(&mut sock).await {
            seen.record(&req);
            if sock.write_all(response).await.is_err() {
                return;
            }
        }
    })
    .await
}

fn get(addr: SocketAddr, path: &str) -> Request {
    Request::builder(Method::Get, &format!("http://{addr}{path}"))
        .unwrap()
        .build()
}

fn fast_client() -> ferroly::http::ClientBuilder {
    Client::builder()
        .timeout(Some(Duration::from_secs(5)))
        .connect_timeout(Some(Duration::from_secs(5)))
}

// --- P0-6 / H9: nothing in a request can split it ---------------------------

#[tokio::test]
async fn urls_with_crlf_spaces_or_control_characters_are_rejected() {
    for url in [
        "http://h/ HTTP/1.1\r\nHost: evil\r\n\r\nGET /admin",
        "http://h/a b",
        "http://h/a\rb",
        "http://h/a\nb",
        "http://h/a\tb",
        "http://h/a\0b",
        "http://h/?q=a b",
        "http://h /",
        "http://h\r\nX: y/",
        "http://user:pw@h/",
    ] {
        assert!(Uri::parse(url).is_err(), "Uri::parse accepted {url:?}");
        assert!(
            Request::builder(Method::Get, url).is_err(),
            "Request::builder accepted {url:?}"
        );
    }
}

#[tokio::test]
async fn hand_built_requests_cannot_split_the_request() {
    let (addr, seen) = canned(OK_HELLO).await;
    let client = fast_client().build().unwrap();
    let injected = " HTTP/1.1\r\nHost: evil\r\n\r\nGET /admin";

    // `Uri` has public fields, so a path can be set without going through the
    // parser. The client must refuse to put it on the wire.
    let mut req = get(addr, "/ok");
    req.uri.path = format!("/x{injected}");
    assert!(matches!(
        client.send(req).await,
        Err(HttpError::InvalidUrl(_))
    ));

    let mut req = get(addr, "/ok");
    req.uri.query = Some(format!("q=1{injected}"));
    assert!(matches!(
        client.send(req).await,
        Err(HttpError::InvalidUrl(_))
    ));

    let mut req = get(addr, "/ok");
    req.uri.host = "h\r\nX-Injected: 1".into();
    assert!(client.send(req).await.is_err());

    let mut req = get(addr, "/ok");
    req.method = Method::Other("GET /admin HTTP/1.1\r\nHost: evil\r\n\r\nGET".into());
    assert!(matches!(
        client.send(req).await,
        Err(HttpError::Protocol(_))
    ));

    let mut req = get(addr, "/ok");
    req.method = Method::Other("BAD METHOD".into());
    assert!(client.send(req).await.is_err());

    // None of those attempts reached the network.
    assert_eq!(seen.connections(), 0);

    // Header names and values are sanitised rather than refused, and what goes
    // out is exactly one request with no injected line.
    let mut req = get(addr, "/ok");
    req.headers.set(
        "X-A\r\nX-Injected: 1",
        "v\r\nX-Injected: 2\r\n\r\nGET /admin HTTP/1.1",
    );
    req.headers.append("X-B: oops\r\n", "w\0\x01");
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello");
    let requests = seen.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.matches("\r\n\r\n").count(), 1, "{sent:?}");
    assert!(sent.starts_with("GET /ok HTTP/1.1\r\n"), "{sent:?}");
    assert!(!sent.contains("\r\nX-Injected"), "{sent:?}");
    assert!(
        sent.contains("\r\nX-AX-Injected1: vX-Injected: 2GET /admin HTTP/1.1\r\n"),
        "{sent:?}"
    );
    assert!(sent.contains("\r\nX-Boops: w\r\n"), "{sent:?}");
}

#[tokio::test]
async fn fragment_is_never_sent() {
    let (addr, seen) = canned(OK_HELLO).await;
    let client = fast_client().build().unwrap();
    let req = Request::builder(Method::Get, &format!("http://{addr}/p?q=1#secret-fragment"))
        .unwrap()
        .build();
    client.send(req).await.unwrap().bytes().await.unwrap();
    let sent = &seen.requests()[0];
    assert!(sent.starts_with("GET /p?q=1 HTTP/1.1\r\n"), "{sent:?}");
    assert!(!sent.contains("secret-fragment"), "{sent:?}");
}

#[tokio::test]
async fn ipv6_literal_urls_connect_and_keep_brackets_in_host() {
    // Skip quietly where the host has no IPv6 loopback.
    let Ok(listener) = TcpListener::bind("[::1]:0").await else {
        eprintln!("skipping: no IPv6 loopback");
        return;
    };
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let req = read_request(&mut sock).await.unwrap();
        sock.write_all(OK_HELLO).await.unwrap();
        req
    });
    let url = format!("http://[::1]:{port}/v6");
    let uri = Uri::parse(&url).unwrap();
    assert_eq!(uri.host, "::1");
    assert_eq!(uri.authority(), format!("[::1]:{port}"));

    let client = fast_client().build().unwrap();
    let resp = client
        .send(Request::builder(Method::Get, &url).unwrap().build())
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello");
    let sent = server.await.unwrap();
    assert!(
        sent.contains(&format!("\r\nHost: [::1]:{port}\r\n")),
        "{sent:?}"
    );
}

// --- P0-7: interim responses --------------------------------------------------

#[tokio::test]
async fn interim_responses_are_skipped_and_do_not_poison_the_connection() {
    let (addr, seen) = scripted(|_, mut sock, seen| async move {
        let mut n = 0;
        while let Some(req) = read_request(&mut sock).await {
            seen.record(&req);
            n += 1;
            let reply = if n == 1 {
                // 103 Early Hints, then 100 Continue, then the real response.
                "HTTP/1.1 103 Early Hints\r\nLink: </style.css>; rel=preload\r\n\r\n\
                 HTTP/1.1 100 Continue\r\n\r\n\
                 HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-N: 1\r\n\r\nfirst"
                    .to_string()
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nX-N: 2\r\n\r\nsecond".to_string()
            };
            sock.write_all(reply.as_bytes()).await.unwrap();
        }
    })
    .await;

    let client = fast_client().build().unwrap();
    let r1 = client.send(get(addr, "/1")).await.unwrap();
    assert_eq!(r1.status().as_u16(), 200);
    assert_eq!(r1.headers().get("x-n"), Some("1"));
    assert!(r1.headers().get("link").is_none());
    assert_eq!(r1.text().await.unwrap(), "first");

    // The second request reuses the connection and gets its own response,
    // not a leftover of the first exchange.
    let r2 = client.send(get(addr, "/2")).await.unwrap();
    assert_eq!(r2.headers().get("x-n"), Some("2"));
    assert_eq!(r2.text().await.unwrap(), "second");
    assert_eq!(seen.connections(), 1);
    assert_eq!(seen.requests().len(), 2);
}

#[tokio::test]
async fn endless_interim_responses_are_an_error() {
    let (addr, _seen) = scripted(|_, mut sock, _| async move {
        let _ = read_request(&mut sock).await;
        for _ in 0..64 {
            if sock
                .write_all(b"HTTP/1.1 102 Processing\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    })
    .await;
    let client = fast_client().build().unwrap();
    let err = client.send(get(addr, "/")).await.unwrap_err();
    assert!(matches!(err, HttpError::Protocol(_)), "{err:?}");
}

// --- H4: method-aware framing and timeouts ------------------------------------

#[tokio::test]
async fn head_response_with_content_length_has_no_body() {
    let (addr, seen) = scripted(|_, mut sock, seen| async move {
        while let Some(req) = read_request(&mut sock).await {
            seen.record(&req);
            let reply: &[u8] = if req.starts_with("HEAD") {
                // Declares a body that, for HEAD, is never sent.
                b"HTTP/1.1 200 OK\r\nContent-Length: 12345\r\n\r\n"
            } else {
                OK_HELLO
            };
            sock.write_all(reply).await.unwrap();
        }
    })
    .await;
    let client = fast_client().build().unwrap();
    let req = Request::builder(Method::Head, &format!("http://{addr}/h"))
        .unwrap()
        .build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.headers().content_length(), Some(12345));
    // Used to wait forever for 12345 bytes.
    let body = timeout(PATIENCE, resp.bytes())
        .await
        .expect("HEAD body hung");
    assert_eq!(body.unwrap(), b"");
    // The connection is still in sync and reusable.
    let resp = client.send(get(addr, "/g")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello");
    assert_eq!(seen.connections(), 1);
}

#[tokio::test]
async fn bodyless_responses_free_the_connection_without_being_read() {
    let (addr, seen) = canned(b"HTTP/1.1 204 No Content\r\n\r\n").await;
    let client = fast_client().build().unwrap();
    for _ in 0..3 {
        // The response is dropped without its (empty) body ever being read.
        let resp = client.send(get(addr, "/")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 204);
    }
    assert_eq!(seen.connections(), 1);
}

#[tokio::test]
async fn body_read_idle_timeout() {
    let (addr, _seen) = scripted(|_, mut sock, _| async move {
        let _ = read_request(&mut sock).await;
        // The head and half the body arrive promptly; the rest never does.
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello")
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    })
    .await;
    let client = fast_client()
        .read_timeout(Some(Duration::from_millis(150)))
        .build()
        .unwrap();
    let mut resp = client.send(get(addr, "/")).await.unwrap();
    assert_eq!(resp.chunk().await.unwrap().unwrap(), b"hello");
    let err = timeout(PATIENCE, resp.chunk())
        .await
        .expect("body read was not bounded")
        .unwrap_err();
    assert!(matches!(err, HttpError::Timeout), "{err:?}");
    assert!(err.is_timeout());
    // The body stays failed rather than pretending to have ended.
    assert!(resp.chunk().await.is_err());
}

#[tokio::test]
async fn total_deadline_covers_a_trickling_body() {
    let (addr, _seen) = scripted(|_, mut sock, _| async move {
        let _ = read_request(&mut sock).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await;
        // A byte every 40 ms keeps any idle timeout happy indefinitely.
        loop {
            if sock.write_all(b"1\r\nx\r\n").await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await;
    let client = fast_client()
        .read_timeout(Some(Duration::from_secs(5)))
        .total_timeout(Some(Duration::from_millis(400)))
        .build()
        .unwrap();
    // Wherever the deadline lands (normally while the body is being read),
    // the exchange as a whole ends in a timeout.
    let exchange = async { client.send(get(addr, "/")).await?.bytes().await };
    let err = timeout(PATIENCE, exchange)
        .await
        .expect("total deadline did not fire")
        .unwrap_err();
    assert!(matches!(err, HttpError::Timeout), "{err:?}");
}

#[tokio::test]
async fn response_head_timeout_and_connect_timeout() {
    // A server that accepts and then says nothing.
    let (addr, _seen) = scripted(|_, mut sock, _| async move {
        let mut sink = [0u8; 1024];
        while matches!(sock.read(&mut sink).await, Ok(n) if n > 0) {}
    })
    .await;

    // Plain HTTP: the connection opens, the response head never comes.
    let client = Client::new().with_timeout(Some(Duration::from_millis(150)));
    let err = timeout(PATIENCE, client.send(get(addr, "/")))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, HttpError::Timeout), "{err:?}");

    // HTTPS to the same mute server: the TLS handshake never completes, which
    // is a *connect* failure with its own timeout and error.
    let client = Client::builder()
        .connect_timeout(Some(Duration::from_millis(150)))
        .timeout(Some(Duration::from_secs(5)))
        .build()
        .unwrap();
    let req = Request::builder(Method::Get, &format!("https://{addr}/"))
        .unwrap()
        .build();
    let err = timeout(PATIENCE, client.send(req))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, HttpError::ConnectTimeout), "{err:?}");
    assert!(err.is_timeout());
}

// --- H5: truncated bodies -------------------------------------------------------

#[tokio::test]
async fn truncated_bodies_are_errors() {
    for reply in [
        // Content-Length promises 10 bytes, 4 arrive.
        &b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhell"[..],
        // Chunked body cut inside a chunk.
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel",
        // Chunked body with no terminating zero chunk.
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
        // Terminating chunk without the final blank line.
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n",
    ] {
        let (addr, _seen) = scripted(move |_, mut sock, _| async move {
            let _ = read_request(&mut sock).await;
            let _ = sock.write_all(reply).await;
            // Dropping the socket closes the connection mid-body.
        })
        .await;
        let client = fast_client().build().unwrap();
        let resp = client.send(get(addr, "/")).await.unwrap();
        let err = resp.bytes().await.unwrap_err();
        assert!(
            matches!(err, HttpError::Protocol(_)),
            "{:?} -> {err:?}",
            String::from_utf8_lossy(reply)
        );
    }

    // Read-until-close framing has no declared length, so EOF is the end.
    let (addr, _seen) = scripted(|_, mut sock, _| async move {
        let _ = read_request(&mut sock).await;
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\n\r\nall of it").await;
    })
    .await;
    let resp = fast_client()
        .build()
        .unwrap()
        .send(get(addr, "/"))
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "all of it");
}

#[tokio::test]
async fn malformed_response_framing_is_rejected() {
    for reply in [
        &b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\nhello"[..],
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\nhello",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n+5\r\nhello\r\n0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhelloXX0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nBad Header: x\r\nContent-Length: 5\r\n\r\nhello",
        b"HTTP/1.1 200 OK\r\nX: a\r\n folded\r\nContent-Length: 5\r\n\r\nhello",
        b"HTTP/9.9 200 OK\r\nContent-Length: 5\r\n\r\nhello",
        b"garbage\r\n\r\n",
    ] {
        let (addr, _seen) = scripted(move |_, mut sock, _| async move {
            let _ = read_request(&mut sock).await;
            let _ = sock.write_all(reply).await;
        })
        .await;
        let client = fast_client().build().unwrap();
        let outcome = match client.send(get(addr, "/")).await {
            Ok(resp) => resp.bytes().await.map(|_| ()),
            Err(e) => Err(e),
        };
        assert!(
            matches!(outcome, Err(HttpError::Protocol(_))),
            "{:?} -> {outcome:?}",
            String::from_utf8_lossy(reply)
        );
    }
}

// --- H6: retry on a reused connection ------------------------------------------

/// Connection 0 answers its first request and then, for the second request it
/// receives, behaves per `mode`; later connections answer normally.
async fn flaky_keepalive(mode: &'static str) -> (SocketAddr, Arc<Seen>) {
    scripted(move |index, mut sock, seen| async move {
        let mut n = 0;
        while let Some(req) = read_request(&mut sock).await {
            seen.record(&req);
            n += 1;
            if index == 0 && n == 2 {
                match mode {
                    // Read the whole request, then close without a byte.
                    "close" => return,
                    // Start a response, then close.
                    "partial" => {
                        let _ = sock.write_all(b"HTTP/1.1 200").await;
                        return;
                    }
                    _ => unreachable!(),
                }
            }
            if sock.write_all(OK_HELLO).await.is_err() {
                return;
            }
        }
    })
    .await
}

fn post(addr: SocketAddr, path: &str) -> Request {
    Request::builder(Method::Post, &format!("http://{addr}{path}"))
        .unwrap()
        .body(b"payload".to_vec())
        .build()
}

#[tokio::test]
async fn post_is_not_resent_when_a_reused_connection_dies() {
    let (addr, seen) = flaky_keepalive("close").await;
    let client = fast_client().build().unwrap();
    client
        .send(get(addr, "/warm"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    // The server received this POST in full and then dropped the connection.
    // It may well have acted on it, so the client must not send it again.
    let err = client.send(post(addr, "/charge")).await.unwrap_err();
    assert!(
        matches!(err, HttpError::Protocol(_) | HttpError::Io(_)),
        "{err:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let posts = seen
        .requests()
        .iter()
        .filter(|r| r.starts_with("POST"))
        .count();
    assert_eq!(posts, 1, "the POST was re-sent: {:?}", seen.requests());
    assert_eq!(seen.connections(), 1);
}

#[tokio::test]
async fn idempotent_request_is_resent_once_on_a_fresh_connection() {
    let (addr, seen) = flaky_keepalive("close").await;
    let client = fast_client().build().unwrap();
    client
        .send(get(addr, "/warm"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    let resp = client.send(get(addr, "/retry-me")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello");
    let attempts = seen
        .requests()
        .iter()
        .filter(|r| r.starts_with("GET /retry-me"))
        .count();
    assert_eq!(attempts, 2);
    assert_eq!(seen.connections(), 2);
}

#[tokio::test]
async fn nothing_is_resent_once_response_bytes_arrived() {
    let (addr, seen) = flaky_keepalive("partial").await;
    let client = fast_client().build().unwrap();
    client
        .send(get(addr, "/warm"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    // Even an idempotent GET: the server demonstrably processed it.
    let err = client.send(get(addr, "/once")).await.unwrap_err();
    assert!(
        matches!(err, HttpError::Protocol(_) | HttpError::Io(_)),
        "{err:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let attempts = seen
        .requests()
        .iter()
        .filter(|r| r.starts_with("GET /once"))
        .count();
    assert_eq!(attempts, 1);
    assert_eq!(seen.connections(), 1);
}

#[tokio::test]
async fn connection_closed_while_idle_is_discarded_before_use() {
    // Each connection serves exactly one request, then closes — without
    // saying `Connection: close`, so the client pools it.
    let (addr, seen) = scripted(|_, mut sock, seen| async move {
        if let Some(req) = read_request(&mut sock).await {
            seen.record(&req);
            let _ = sock.write_all(OK_HELLO).await;
        }
        drop(sock);
        seen.record("closed");
    })
    .await;
    let client = fast_client().build().unwrap();
    client
        .send(get(addr, "/warm"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    // Wait until the server has closed its end, then let the FIN arrive.
    timeout(PATIENCE, async {
        while !seen.requests().iter().any(|r| r == "closed") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The pooled connection is noticed to be closed before anything is
    // written to it, so even a POST goes out exactly once, on a new one.
    let resp = client.send(post(addr, "/charge")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello");
    let posts = seen
        .requests()
        .iter()
        .filter(|r| r.starts_with("POST"))
        .count();
    assert_eq!(posts, 1);
    assert_eq!(seen.connections(), 2);
}

// --- A2: response-body cap --------------------------------------------------------

#[tokio::test]
async fn response_body_cap_applies_to_every_framing() {
    for reply in [
        // Declared length over the cap: refused before a byte is buffered.
        &b"HTTP/1.1 200 OK\r\nContent-Length: 1000000000000\r\n\r\n"[..],
        // Chunked: 4 + 4 + 4 bytes against a cap of 10.
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\naaaa\r\n4\r\nbbbb\r\n4\r\ncccc\r\n0\r\n\r\n",
        // One huge declared chunk.
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFF\r\n",
        // Read-until-close.
        b"HTTP/1.1 200 OK\r\n\r\naaaabbbbcccc",
    ] {
        let (addr, _seen) = scripted(move |_, mut sock, _| async move {
            let _ = read_request(&mut sock).await;
            let _ = sock.write_all(reply).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        })
        .await;
        let client = fast_client().max_response_body_bytes(Some(10)).build().unwrap();
        let resp = client.send(get(addr, "/")).await.unwrap();
        let err = timeout(PATIENCE, resp.bytes()).await.unwrap().unwrap_err();
        assert!(
            matches!(err, HttpError::BodyTooLarge(10)),
            "{:?} -> {err:?}",
            String::from_utf8_lossy(reply)
        );
    }

    // At the cap exactly is fine, and one response can lift the cap.
    let (addr, _seen) = canned(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\naaaabbbbcccc").await;
    let client = fast_client()
        .max_response_body_bytes(Some(12))
        .build()
        .unwrap();
    assert_eq!(
        client
            .send(get(addr, "/"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .len(),
        12
    );
    let client = fast_client()
        .max_response_body_bytes(Some(3))
        .build()
        .unwrap();
    let mut resp = client.send(get(addr, "/")).await.unwrap();
    resp.set_max_body_bytes(None);
    assert_eq!(resp.text().await.unwrap(), "aaaabbbbcccc");
}

#[tokio::test]
async fn overlong_chunk_size_line_is_rejected() {
    let (addr, _seen) = scripted(|_, mut sock, _| async move {
        let _ = read_request(&mut sock).await;
        let mut reply = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;".to_vec();
        reply.extend(std::iter::repeat(b'x').take(64 * 1024));
        reply.extend_from_slice(b"\r\nhello\r\n0\r\n\r\n");
        let _ = sock.write_all(&reply).await;
    })
    .await;
    let resp = fast_client()
        .build()
        .unwrap()
        .send(get(addr, "/"))
        .await
        .unwrap();
    let err = resp.bytes().await.unwrap_err();
    assert!(matches!(err, HttpError::Protocol(_)), "{err:?}");
}

// --- Capabilities ---------------------------------------------------------------

#[tokio::test]
async fn default_user_agent_is_sent_and_overridable() {
    let (addr, seen) = canned(OK_HELLO).await;
    let expected = format!("\r\nUser-Agent: ferroly/{}\r\n", env!("CARGO_PKG_VERSION"));

    Client::new()
        .send(get(addr, "/"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        seen.requests()[0].contains(&expected),
        "{:?}",
        seen.requests()[0]
    );

    let client = fast_client()
        .user_agent("my-app/1.0\r\nX-Evil: 1")
        .build()
        .unwrap();
    client
        .send(get(addr, "/"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(seen.requests()[1].contains("\r\nUser-Agent: my-app/1.0X-Evil: 1\r\n"));

    // A per-request header wins over the client default.
    let mut req = get(addr, "/");
    req.headers.set("user-agent", "per-request");
    client.send(req).await.unwrap().bytes().await.unwrap();
    assert!(seen.requests()[2].contains("\r\nuser-agent: per-request\r\n"));
    assert!(!seen.requests()[2].contains("my-app"));

    let client = fast_client().no_user_agent().build().unwrap();
    client
        .send(get(addr, "/"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(!seen.requests()[3]
        .to_ascii_lowercase()
        .contains("user-agent"));
}

/// A server whose paths redirect in various ways; `other` is a second origin.
async fn redirecting(other: SocketAddr) -> (SocketAddr, Arc<Seen>) {
    scripted(move |_, mut sock, seen| async move {
        while let Some(req) = read_request(&mut sock).await {
            seen.record(&req);
            let line = req.lines().next().unwrap_or_default().to_string();
            let path = line.split(' ').nth(1).unwrap_or("/").to_string();
            let redirect = |code: u16, location: String| {
                format!("HTTP/1.1 {code} Redirect\r\nLocation: {location}\r\nContent-Length: 4\r\n\r\ngone")
            };
            let reply = match path.as_str() {
                "/a/start" => redirect(302, "../b/next?x=1#frag".into()),
                "/see-other" => redirect(303, "/landed".into()),
                "/temporary" => redirect(307, "/landed".into()),
                "/moved" => redirect(301, "/landed".into()),
                "/cross" => redirect(302, format!("http://{other}/landed")),
                "/loop" => redirect(302, "/loop".into()),
                "/bad-location" => redirect(302, "/a b".into()),
                "/no-location" => "HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n".to_string(),
                _ => "HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nlanded".to_string(),
            };
            if sock.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    })
    .await
}

#[tokio::test]
async fn redirects_are_off_by_default_and_follow_the_rules_when_enabled() {
    let (other, other_seen) = canned(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nother").await;
    let (addr, seen) = redirecting(other).await;

    // Off by default: the 302 itself is returned.
    let resp = fast_client()
        .build()
        .unwrap()
        .send(get(addr, "/a/start"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(resp.headers().get("location"), Some("../b/next?x=1#frag"));
    assert_eq!(resp.text().await.unwrap(), "gone");

    let client = fast_client().max_redirects(3).build().unwrap();

    // A relative Location is resolved; the fragment is dropped.
    let resp = client.send(get(addr, "/a/start")).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.uri().request_target(), "/b/next?x=1");
    assert_eq!(resp.text().await.unwrap(), "landed");
    assert!(seen
        .requests()
        .last()
        .unwrap()
        .starts_with("GET /b/next?x=1 HTTP/1.1\r\n"));

    let with_body = |path: &str| {
        Request::builder(Method::Post, &format!("http://{addr}{path}"))
            .unwrap()
            .header("authorization", "Bearer secret")
            .header("content-type", "text/plain")
            .body(b"payload".to_vec())
            .build()
    };

    // 303 and 301: POST becomes a body-less GET.
    for path in ["/see-other", "/moved"] {
        let resp = client.send(with_body(path)).await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "landed");
        let last = seen.requests().last().unwrap().clone();
        assert!(
            last.starts_with("GET /landed HTTP/1.1\r\n"),
            "{path}: {last:?}"
        );
        assert!(!last.contains("payload") && !last.to_ascii_lowercase().contains("content-"));
        // Same origin: credentials are kept.
        assert!(last.contains("authorization: Bearer secret"), "{last:?}");
    }

    // 307: method and body are preserved.
    let resp = client.send(with_body("/temporary")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "landed");
    let last = seen.requests().last().unwrap().clone();
    assert!(
        last.starts_with("POST /landed HTTP/1.1\r\n") && last.ends_with("payload"),
        "{last:?}"
    );

    // Cross-origin: followed, but Authorization does not travel.
    let mut req = get(addr, "/cross");
    req.headers.set("authorization", "Bearer secret");
    req.headers.set("cookie", "sid=1");
    req.headers.set("x-other", "kept");
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.uri().port, other.port());
    assert_eq!(resp.text().await.unwrap(), "other");
    let arrived = other_seen.requests().pop().unwrap();
    assert!(
        arrived.starts_with("GET /landed HTTP/1.1\r\n"),
        "{arrived:?}"
    );
    let lower = arrived.to_ascii_lowercase();
    assert!(
        !lower.contains("authorization") && !lower.contains("cookie"),
        "{arrived:?}"
    );
    assert!(arrived.contains("x-other: kept"));
    assert!(
        arrived.contains(&format!("Host: {other}\r\n")),
        "{arrived:?}"
    );

    // A loop ends at the limit instead of spinning.
    let err = client.send(get(addr, "/loop")).await.unwrap_err();
    assert!(matches!(err, HttpError::TooManyRedirects), "{err:?}");

    // A Location that would split the request is refused, not followed.
    let err = client.send(get(addr, "/bad-location")).await.unwrap_err();
    assert!(matches!(err, HttpError::InvalidUrl(_)), "{err:?}");

    // No Location: nothing to follow.
    let resp = client.send(get(addr, "/no-location")).await.unwrap();
    assert_eq!(resp.status().as_u16(), 302);
}

#[tokio::test]
async fn http_proxy_receives_absolute_form_with_credentials() {
    let (proxy_addr, proxy_seen) =
        canned(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nproxied").await;
    let (direct_addr, direct_seen) = canned(OK_HELLO).await;

    let proxy = Proxy::http(&format!("http://{proxy_addr}"))
        .unwrap()
        .basic_auth("user", "pass")
        .no_proxy(&format!("127.0.0.1:{}", direct_addr.port()));
    let client = fast_client().proxy(proxy).build().unwrap();

    // The target host is never resolved or contacted: the proxy gets the
    // request with an absolute-form target.
    let req = Request::builder(Method::Get, "http://target.invalid:8080/path?q=1")
        .unwrap()
        .build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "proxied");
    let sent = &proxy_seen.requests()[0];
    assert!(
        sent.starts_with("GET http://target.invalid:8080/path?q=1 HTTP/1.1\r\n"),
        "{sent:?}"
    );
    assert!(
        sent.contains("\r\nHost: target.invalid:8080\r\n"),
        "{sent:?}"
    );
    assert!(
        sent.contains("\r\nProxy-Authorization: Basic dXNlcjpwYXNz\r\n"),
        "{sent:?}"
    );

    // A NO_PROXY match goes direct, in origin form, without proxy credentials.
    let resp = client.send(get(direct_addr, "/direct")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello");
    let sent = &direct_seen.requests()[0];
    assert!(sent.starts_with("GET /direct HTTP/1.1\r\n"), "{sent:?}");
    assert!(!sent.contains("Proxy-Authorization"), "{sent:?}");
    assert_eq!(proxy_seen.requests().len(), 1);

    // Credentials embedded in the proxy URL work the same way.
    let proxy = Proxy::all(&format!("http://us%40er:p%3Aw@{proxy_addr}")).unwrap();
    let client = fast_client().proxy(proxy).build().unwrap();
    let req = Request::builder(Method::Get, "http://target.invalid/")
        .unwrap()
        .build();
    client.send(req).await.unwrap().bytes().await.unwrap();
    // base64("us@er:p:w")
    assert!(proxy_seen.requests()[1].contains("Proxy-Authorization: Basic dXNAZXI6cDp3\r\n"));
}

#[tokio::test]
async fn refused_connect_tunnel_is_a_proxy_error() {
    let (proxy_addr, proxy_seen) = scripted(|_, mut sock, seen| async move {
        if let Some(req) = read_request(&mut sock).await {
            seen.record(&req);
            let _ = sock
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
        }
    })
    .await;
    let proxy = Proxy::https(&format!("http://{proxy_addr}")).unwrap();
    let client = fast_client().proxy(proxy).build().unwrap();
    let req = Request::builder(Method::Get, "https://secure.invalid/x")
        .unwrap()
        .build();
    let err = client.send(req).await.unwrap_err();
    assert!(
        matches!(&err, HttpError::Proxy(m) if m.contains("407")),
        "{err:?}"
    );
    let sent = &proxy_seen.requests()[0];
    assert!(
        sent.starts_with("CONNECT secure.invalid:443 HTTP/1.1\r\nHost: secure.invalid:443\r\n"),
        "{sent:?}"
    );
    // The request itself (path, headers) is never shown to the proxy.
    assert!(!sent.contains("/x"), "{sent:?}");
}

#[tokio::test]
async fn form_and_multipart_bodies() {
    let (addr, seen) = canned(OK_HELLO).await;
    let client = fast_client().build().unwrap();

    let req = Request::builder(Method::Post, &format!("http://{addr}/form"))
        .unwrap()
        .form([("name", "J\u{f6}rg & co"), ("q", "a=b+c")])
        .build();
    client.send(req).await.unwrap().bytes().await.unwrap();
    let sent = &seen.requests()[0];
    assert!(
        sent.contains("content-type: application/x-www-form-urlencoded\r\n"),
        "{sent:?}"
    );
    assert!(
        sent.ends_with("\r\n\r\nname=J%C3%B6rg+%26+co&q=a%3Db%2Bc"),
        "{sent:?}"
    );

    let form = Multipart::new().text("title", "hello").file(
        "upload",
        "a.bin",
        "application/octet-stream",
        vec![0u8, 1, 2, 255],
    );
    let req = Request::builder(Method::Post, &format!("http://{addr}/multipart"))
        .unwrap()
        .multipart(&form)
        .build();
    let expected_body = form.encode();
    assert_eq!(req.body, expected_body);
    client.send(req).await.unwrap().bytes().await.unwrap();
    let sent = &seen.requests()[1];
    let boundary = form.boundary();
    assert!(
        sent.contains(&format!(
            "content-type: multipart/form-data; boundary={boundary}\r\n"
        )),
        "{sent:?}"
    );
    assert!(sent.contains(&format!("Content-Length: {}\r\n", expected_body.len())));
    assert!(sent.contains(&format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n"
    )));
    assert!(sent.contains(
        "Content-Disposition: form-data; name=\"upload\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    ));
}

#[tokio::test]
async fn streaming_upload_is_chunked_or_exact_length() {
    let (addr, seen) = canned(OK_HELLO).await;
    let client = fast_client().build().unwrap();
    let payload: Vec<u8> = (0..40_000u32).map(|i| b'a' + (i % 26) as u8).collect();

    // No Content-Length: chunked.
    let req = Request::builder(Method::Put, &format!("http://{addr}/up"))
        .unwrap()
        .build();
    let resp = client.send_streaming(req, &payload[..]).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello");
    let sent = seen.requests()[0].clone();
    let (head, body) = sent.split_once("\r\n\r\n").unwrap();
    assert!(head.contains("Transfer-Encoding: chunked") && !head.contains("Content-Length"));
    // Decode the chunks the server saw and compare with what was streamed.
    let mut decoded = Vec::new();
    let mut rest = body.as_bytes();
    loop {
        let eol = rest.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap(), 16).unwrap();
        rest = &rest[eol + 2..];
        if size == 0 {
            break;
        }
        decoded.extend_from_slice(&rest[..size]);
        assert_eq!(&rest[size..size + 2], b"\r\n");
        rest = &rest[size + 2..];
    }
    assert_eq!(decoded, payload);

    // With a declared Content-Length: exactly that many raw bytes.
    let req = Request::builder(Method::Put, &format!("http://{addr}/up"))
        .unwrap()
        .header("content-length", payload.len().to_string())
        .build();
    client
        .send_streaming(req, &payload[..])
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let sent = seen.requests()[1].clone();
    let (head, body) = sent.split_once("\r\n\r\n").unwrap();
    assert!(!head.contains("Transfer-Encoding"), "{head:?}");
    assert_eq!(body.as_bytes(), &payload[..]);

    // A stream shorter than its declared length is an error.
    let req = Request::builder(Method::Put, &format!("http://{addr}/up"))
        .unwrap()
        .header("content-length", "100")
        .build();
    let err = client.send_streaming(req, &b"short"[..]).await.unwrap_err();
    assert!(matches!(err, HttpError::Protocol(_)), "{err:?}");
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
}

impl Interceptor for Recorder {
    fn on_request(&self, req: &mut Request) -> Result<(), HttpError> {
        if req.uri.path == "/forbidden" {
            return Err(HttpError::Protocol("blocked by policy".into()));
        }
        req.headers.set("x-trace-id", "trace-1");
        self.events
            .lock()
            .unwrap()
            .push(format!("request {}", req.uri.path));
        Ok(())
    }
    fn on_response(&self, req: &Request, status: StatusCode, headers: &HeaderMap) {
        self.events.lock().unwrap().push(format!(
            "response {} {} len={}",
            req.uri.path,
            status.as_u16(),
            headers.get("content-length").unwrap_or("-")
        ));
    }
    fn on_error(&self, req: &Request, error: &HttpError) {
        self.events
            .lock()
            .unwrap()
            .push(format!("error {} {error}", req.uri.path));
    }
}

#[tokio::test]
async fn interceptors_inject_headers_and_observe_outcomes() {
    let (addr, seen) = canned(OK_HELLO).await;
    let recorder = Arc::new(Recorder::default());
    struct Shared(Arc<Recorder>);
    impl Interceptor for Shared {
        fn on_request(&self, req: &mut Request) -> Result<(), HttpError> {
            self.0.on_request(req)
        }
        fn on_response(&self, req: &Request, status: StatusCode, headers: &HeaderMap) {
            self.0.on_response(req, status, headers)
        }
        fn on_error(&self, req: &Request, error: &HttpError) {
            self.0.on_error(req, error)
        }
    }
    let client = fast_client()
        .interceptor(Shared(recorder.clone()))
        .build()
        .unwrap();

    client
        .send(get(addr, "/ok"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(seen.requests()[0].contains("\r\nx-trace-id: trace-1\r\n"));

    // An interceptor can veto a request before it is sent.
    let err = client.send(get(addr, "/forbidden")).await.unwrap_err();
    assert!(err.to_string().contains("blocked by policy"));
    assert_eq!(seen.requests().len(), 1);

    // A transport failure is reported to `on_error`.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    assert!(client.send(get(dead, "/down")).await.is_err());

    let events = recorder.events.lock().unwrap().clone();
    assert_eq!(events[0], "request /ok");
    assert_eq!(events[1], "response /ok 200 len=5");
    assert_eq!(events[2], "request /down");
    assert!(events[3].starts_with("error /down io error"), "{events:?}");
    assert_eq!(events.len(), 4);
}

#[tokio::test]
async fn pool_size_and_idle_expiry_are_configurable() {
    // Pooling off: one connection per request.
    let (addr, seen) = canned(OK_HELLO).await;
    let client = fast_client().pool_max_idle_per_host(0).build().unwrap();
    for _ in 0..3 {
        client
            .send(get(addr, "/"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
    }
    assert_eq!(seen.connections(), 3);

    // Default: reused.
    let (addr, seen) = canned(OK_HELLO).await;
    let client = fast_client().build().unwrap();
    for _ in 0..3 {
        client
            .send(get(addr, "/"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
    }
    assert_eq!(seen.connections(), 1);

    // An idle connection past its expiry is not reused.
    let (addr, seen) = canned(OK_HELLO).await;
    let client = fast_client()
        .pool_idle_timeout(Some(Duration::from_millis(50)))
        .build()
        .unwrap();
    client
        .send(get(addr, "/"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    client
        .send(get(addr, "/"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(seen.connections(), 2);

    // `Connection: close` and HTTP/1.0 responses are never pooled.
    for reply in [
        &b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 5\r\n\r\nhello"[..],
        b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello",
    ] {
        let (addr, seen) = canned(reply).await;
        let client = fast_client().build().unwrap();
        for _ in 0..2 {
            client
                .send(get(addr, "/"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
        }
        assert_eq!(
            seen.connections(),
            2,
            "{:?}",
            String::from_utf8_lossy(reply)
        );
    }
}

// --- Decompression ---------------------------------------------------------------

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// `gzip.compress(b"hello gzip world", mtime=0)`
const GZIP_HELLO: &str = "1f8b08000000000002ffcb48cdc9c95748afca2c5028cf2fca4901006b7de8b710000000";
/// zlib-wrapped DEFLATE of b"hello hello hello hello"
const ZLIB_HELLO: &str = "7801cb48cdc9c957c8402701680308b1";

/// A server answering with `body` under the given `Content-Encoding`, chunked
/// in small pieces so the decoder sees the stream split at awkward places.
async fn encoded(encoding: &'static str, body: Vec<u8>) -> (SocketAddr, Arc<Seen>) {
    let body = Arc::new(body);
    scripted(move |_, mut sock, seen| {
        let body = body.clone();
        async move {
            while let Some(req) = read_request(&mut sock).await {
                seen.record(&req);
                let mut reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Encoding: {encoding}\r\nTransfer-Encoding: chunked\r\n\r\n"
                )
                .into_bytes();
                for piece in body.chunks(7) {
                    reply.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
                    reply.extend_from_slice(piece);
                    reply.extend_from_slice(b"\r\n");
                }
                reply.extend_from_slice(b"0\r\n\r\n");
                if sock.write_all(&reply).await.is_err() {
                    return;
                }
            }
        }
    })
    .await
}

#[tokio::test]
async fn decompression_is_opt_in() {
    let (addr, seen) = encoded("gzip", unhex(GZIP_HELLO)).await;

    // Off (the default): no Accept-Encoding is sent and the body is raw.
    let resp = fast_client()
        .build()
        .unwrap()
        .send(get(addr, "/"))
        .await
        .unwrap();
    assert_eq!(resp.headers().get("content-encoding"), Some("gzip"));
    assert_eq!(resp.bytes().await.unwrap(), unhex(GZIP_HELLO));
    assert!(!seen.requests()[0]
        .to_ascii_lowercase()
        .contains("accept-encoding"));

    // On: advertised, decoded, and the encoding header is removed.
    let client = fast_client().decompress(true).build().unwrap();
    let resp = client.send(get(addr, "/")).await.unwrap();
    assert!(resp.headers().get("content-encoding").is_none());
    assert_eq!(resp.text().await.unwrap(), "hello gzip world");
    assert!(seen.requests()[1].contains("\r\nAccept-Encoding: gzip, deflate\r\n"));
    // The connection is reusable afterwards.
    let resp = client.send(get(addr, "/")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello gzip world");
    assert_eq!(seen.connections(), 2);

    let (addr, _seen) = encoded("deflate", unhex(ZLIB_HELLO)).await;
    let resp = client.send(get(addr, "/")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello hello hello hello");

    // An encoding the client does not implement is passed through untouched.
    let (addr, _seen) = encoded("br", b"not really brotli".to_vec()).await;
    let resp = client.send(get(addr, "/")).await.unwrap();
    assert_eq!(resp.headers().get("content-encoding"), Some("br"));
    assert_eq!(resp.text().await.unwrap(), "not really brotli");
}

#[tokio::test]
async fn decompression_is_bounded_and_checks_integrity() {
    // 4 MiB of zeros in ~4 KiB of gzip: stopped at the 1 MiB cap.
    let bomb = unhex(include_str!("../src/http/testdata/inflate_bomb_gzip.hex"));
    assert!(bomb.len() < 8 * 1024);
    let (addr, _seen) = encoded("gzip", bomb.clone()).await;
    let client = fast_client()
        .decompress(true)
        .max_response_body_bytes(Some(1024 * 1024))
        .build()
        .unwrap();
    let mut resp = client.send(get(addr, "/")).await.unwrap();
    let mut received = 0usize;
    let err = loop {
        match resp.chunk().await {
            Ok(Some(piece)) => received += piece.len(),
            Ok(None) => panic!("decompression bomb was fully inflated"),
            Err(e) => break e,
        }
    };
    assert!(matches!(err, HttpError::BodyTooLarge(_)), "{err:?}");
    assert!(received <= 1024 * 1024, "{received}");

    // With room for it, the same stream decodes completely.
    let client = fast_client()
        .decompress(true)
        .max_response_body_bytes(Some(8 * 1024 * 1024))
        .build()
        .unwrap();
    let body = client
        .send(get(addr, "/"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.len(), 4 * 1024 * 1024);
    assert!(body.iter().all(|&b| b == 0));

    // Corrupt data, a bad checksum and a truncated stream are all errors.
    let good = unhex(GZIP_HELLO);
    let mut bad_crc = good.clone();
    let n = bad_crc.len();
    bad_crc[n - 8] ^= 0xff;
    let truncated = good[..good.len() - 5].to_vec();
    for body in [bad_crc, truncated, b"this is not gzip at all".to_vec()] {
        let (addr, _seen) = encoded("gzip", body).await;
        let client = fast_client().decompress(true).build().unwrap();
        let err = client
            .send(get(addr, "/"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Protocol(_)), "{err:?}");
    }
}
