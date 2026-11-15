#![cfg(feature = "http2")]
//! The HTTP/2 client's connections per origin and its retries: a burst and
//! the retries of its refused requests stay within
//! `max_connections_per_origin`; a refused request is retried on another
//! connection, a new one only below that cap; a request
//! body streamed in the background stops as soon as its stream is refused
//! or reset; and HTTP/2 connection timers count as timeouts.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{
    body_channel, serve_with_config, BoxFuture, Client, Http2Cause, Http2Config, HttpError,
    HttpHandler, HttpResponse, Method, Request, ServerConfig, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const RST_STREAM: u8 = 0x3;
const SETTINGS: u8 = 0x4;
const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const ACK: u8 = 0x1;
const REFUSED_STREAM: u32 = 0x7;
const CANCEL: u32 = 0x8;
/// `:status: 200` (static table index 8).
const STATUS_200: &[u8] = &[0x88];

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

/// The server end of one scripted connection.
struct Peer {
    sock: TcpStream,
}

impl Peer {
    async fn send(&mut self, bytes: &[u8]) {
        let _ = self.sock.write_all(bytes).await;
    }

    async fn read_frame(&mut self) -> Option<Frame> {
        let mut h = [0u8; 9];
        self.sock.read_exact(&mut h).await.ok()?;
        let len = (h[0] as usize) << 16 | (h[1] as usize) << 8 | h[2] as usize;
        let mut payload = vec![0; len];
        self.sock.read_exact(&mut payload).await.ok()?;
        Some(Frame {
            kind: h[3],
            flags: h[4],
            stream: u32::from_be_bytes([h[5], h[6], h[7], h[8]]) & 0x7fff_ffff,
            payload,
        })
    }

    /// Reads frames until one matches, or the connection ends (`None`).
    async fn until(&mut self, mut want: impl FnMut(&Frame) -> bool) -> Option<Frame> {
        let read = async {
            while let Some(f) = self.read_frame().await {
                if want(&f) {
                    return Some(f);
                }
            }
            None
        };
        timeout(Duration::from_secs(10), read).await.ok().flatten()
    }
}

/// A server that runs `script` on every accepted connection, numbered from
/// one, after the preface and the SETTINGS exchange (`settings` is the
/// payload of its own SETTINGS frame).
async fn scripted<F, Fut>(settings: &'static [u8], script: F) -> SocketAddr
where
    F: Fn(usize, Peer) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok((mut sock, _)) = listener.accept().await {
            n += 1;
            let mut preface = [0u8; 24];
            if sock.read_exact(&mut preface).await.is_err() {
                continue;
            }
            let _ = sock.write_all(&frame(SETTINGS, 0, 0, settings)).await;
            let mut peer = Peer { sock };
            let theirs = peer
                .until(|f| f.kind == SETTINGS && f.flags & ACK == 0)
                .await;
            if theirs.is_none() {
                continue;
            }
            peer.send(&frame(SETTINGS, ACK, 0, &[])).await;
            tokio::spawn(script(n, peer));
        }
    });
    addr
}

fn h2_client(f: impl FnOnce(&mut Http2Config)) -> Client {
    let mut h2 = Http2Config::default();
    f(&mut h2);
    Client::builder()
        .http2_prior_knowledge(true)
        .http2_config(h2)
        .build()
        .unwrap()
}

fn get_req(addr: SocketAddr) -> Request {
    Request::builder(Method::Get, &format!("http://{addr}/"))
        .unwrap()
        .build()
}

fn post_req(addr: SocketAddr) -> Request {
    Request::builder(Method::Post, &format!("http://{addr}/"))
        .unwrap()
        .build()
}

/// Records the client port of every connection that reached it.
struct Ports(Mutex<HashSet<u16>>);

impl HttpHandler for Ports {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        if let Some(peer) = req.peer_addr() {
            self.0.lock().unwrap().insert(peer.port());
        }
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            HttpResponse::text(StatusCode::OK, "ok")
        })
    }
}

/// HTTP-6: a cold client's burst of concurrent requests (more than the
/// server's stream limit, so many are refused before its SETTINGS arrive)
/// opens at most `max_connections_per_origin` connections, and every request
/// is answered.
#[tokio::test]
async fn a_cold_burst_stays_within_max_connections_per_origin() {
    for (server_streams, requests) in [(100u32, 150usize), (10, 150)] {
        let ports = Arc::new(Ports(Mutex::new(HashSet::new())));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut h2 = Http2Config::default();
        h2.max_concurrent_streams = server_streams;
        let config = ServerConfig {
            http2: Some(h2),
            ..ServerConfig::default()
        };
        let handler = ports.clone();
        tokio::spawn(async move {
            let _ =
                serve_with_config(listener, handler, std::future::pending::<()>(), config).await;
        });
        let client = h2_client(|_| {});
        let mut tasks = Vec::new();
        for _ in 0..requests {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                client.send(get_req(addr)).await?.text().await
            }));
        }
        for task in tasks {
            let answer = timeout(Duration::from_secs(20), task)
                .await
                .expect("answered in time")
                .unwrap();
            assert_eq!(answer.unwrap(), "ok", "server limit {server_streams}");
        }
        let opened = ports.0.lock().unwrap().len();
        assert!(
            (1..=4).contains(&opened),
            "{opened} connections for a cap of 4 (server limit {server_streams})"
        );
    }
}

/// HTTP-6: a refused request is retried on a connection other than the one
/// that refused it, dialing one while the origin is below
/// `max_connections_per_origin`; only at the cap does it go back to the
/// refusing connection. With one connection allowed that is the same
/// connection, so no second one is dialed.
#[tokio::test]
async fn a_refused_request_is_retried_on_a_new_connection_only_below_the_cap() {
    for (max, expected) in [(1, 1), (4, 2)] {
        let conns = Arc::new(AtomicUsize::new(0));
        let seen = conns.clone();
        // Each round has its own server, whose first connection refuses its
        // first stream.
        let addr = scripted(&[], move |n, mut peer| {
            seen.fetch_add(1, Ordering::SeqCst);
            async move {
                while let Some(f) = peer.until(|f| f.kind == HEADERS).await {
                    if n == 1 && f.stream == 1 {
                        let refuse = frame(RST_STREAM, 0, f.stream, &REFUSED_STREAM.to_be_bytes());
                        peer.send(&refuse).await;
                        continue;
                    }
                    let reply = frame(HEADERS, END_HEADERS | END_STREAM, f.stream, STATUS_200);
                    peer.send(&reply).await;
                }
            }
        })
        .await;
        let client = h2_client(|h| h.max_connections_per_origin = max);
        let resp = timeout(Duration::from_secs(5), client.send(get_req(addr)))
            .await
            .expect("answered in time")
            .expect("retried after the refusal");
        assert_eq!(resp.status(), StatusCode::OK);
        // max 1: the retry went back to the refusing connection. max 4: it
        // went to a second connection, not back to the one that refused it.
        assert_eq!(conns.load(Ordering::SeqCst), expected, "max {max}");
        // Later requests use the connections already there.
        for _ in 0..3 {
            let resp = client.send(get_req(addr)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(conns.load(Ordering::SeqCst), expected, "max {max}");
    }
}

/// RPC-3 (HTTP/2 side): a streamed request body that has sent its first
/// piece and then goes quiet is dropped as soon as the server refuses the
/// stream, so its producer learns at once (rather than at its next send)
/// that nobody reads it, and the request fails promptly as refused.
#[tokio::test]
async fn an_idle_streamed_body_stops_when_its_stream_is_refused() {
    let addr = scripted(&[], |_, mut peer| async move {
        if let Some(h) = peer.until(|f| f.kind == HEADERS).await {
            let _ = peer.until(|f| f.kind == DATA && f.stream == h.stream).await;
            let refuse = frame(RST_STREAM, 0, h.stream, &REFUSED_STREAM.to_be_bytes());
            peer.send(&refuse).await;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    })
    .await;
    let client = h2_client(|_| {});
    let (mut tx, body) = body_channel(4);
    tx.send("first").await.unwrap();
    let err = timeout(
        Duration::from_secs(5),
        client.send_body_stream(post_req(addr), body),
    )
    .await
    .expect("the refusal ends the request promptly")
    .unwrap_err();
    assert_eq!(err.http2_cause(), Some(Http2Cause::Refused), "{err:?}");
    timeout(Duration::from_secs(2), tx.closed())
        .await
        .expect("the upload noticed its stream was refused");
}

/// RPC-3 (HTTP/2 side): the same after the response head, for a stream the
/// server resets while the request body is idle: once the response body
/// reports the reset, the upload stops even though the response is held.
#[tokio::test]
async fn an_idle_streamed_body_stops_when_its_stream_is_reset() {
    let addr = scripted(&[], |_, mut peer| async move {
        if let Some(h) = peer.until(|f| f.kind == HEADERS).await {
            let _ = peer.until(|f| f.kind == DATA && f.stream == h.stream).await;
            let mut reply = frame(HEADERS, END_HEADERS, h.stream, STATUS_200);
            reply.extend(frame(RST_STREAM, 0, h.stream, &CANCEL.to_be_bytes()));
            peer.send(&reply).await;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    })
    .await;
    let client = h2_client(|_| {});
    let (mut tx, body) = body_channel(4);
    tx.send("first").await.unwrap();
    let mut resp = timeout(
        Duration::from_secs(5),
        client.send_body_stream(post_req(addr), body),
    )
    .await
    .expect("the response head in time")
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let err = resp.chunk().await.unwrap_err();
    assert!(
        matches!(err.http2_cause(), Some(Http2Cause::Reset(_))),
        "{err:?}"
    );
    timeout(Duration::from_secs(2), tx.closed())
        .await
        .expect("the upload noticed its stream was reset");
    drop(resp);
}

/// A server that answers before the request body is complete may still
/// read it: after a clean response the upload carries on to its end.
#[tokio::test]
async fn an_upload_continues_after_a_clean_early_response() {
    let (seen_tx, mut seen) = mpsc::unbounded_channel();
    let addr = scripted(&[], move |_, mut peer| {
        let seen_tx = seen_tx.clone();
        async move {
            if let Some(h) = peer.until(|f| f.kind == HEADERS).await {
                let reply = frame(HEADERS, END_HEADERS | END_STREAM, h.stream, STATUS_200);
                peer.send(&reply).await;
                while let Some(f) = peer.until(|f| f.kind == DATA && f.stream == h.stream).await {
                    let _ = seen_tx.send((f.payload, f.flags & END_STREAM != 0));
                    if f.flags & END_STREAM != 0 {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    })
    .await;
    let client = h2_client(|_| {});
    let (mut tx, body) = body_channel(4);
    let resp = timeout(
        Duration::from_secs(5),
        client.send_body_stream(post_req(addr), body),
    )
    .await
    .expect("the response head in time")
    .unwrap();
    assert!(resp.bytes().await.unwrap().is_empty());
    tx.send("late").await.unwrap();
    tx.finish().await.unwrap();
    let mut got = Vec::new();
    let mut ended = false;
    while !ended {
        let (piece, end) = timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("the upload carried on")
            .unwrap();
        got.extend(piece);
        ended = end;
    }
    assert_eq!(got, b"late");
}

/// HTTP-8: `is_timeout` is true for an HTTP/1.1 timeout and for an HTTP/2
/// connection timer (an unanswered keep-alive PING) alike.
#[tokio::test]
async fn timeouts_are_timeouts_on_both_protocols() {
    // HTTP/1.1: a server that never answers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    let client = Client::builder()
        .timeout(Some(Duration::from_millis(200)))
        .build()
        .unwrap();
    let err = client.send(get_req(addr)).await.unwrap_err();
    assert!(matches!(err, HttpError::Timeout), "{err:?}");
    assert!(err.is_timeout());

    // HTTP/2: a peer that goes silent; the keep-alive PING times out.
    let addr = scripted(&[], |_, mut peer| async move {
        let _ = peer.until(|f| f.kind == HEADERS).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(peer);
    })
    .await;
    let client = h2_client(|h| {
        h.keepalive_interval = Some(Duration::from_millis(200));
        h.keepalive_timeout = Duration::from_millis(300);
    });
    let err = timeout(Duration::from_secs(5), client.send(get_req(addr)))
        .await
        .expect("the dead peer is detected")
        .unwrap_err();
    assert_eq!(err.http2_cause(), Some(Http2Cause::Timeout), "{err:?}");
    assert!(err.is_timeout(), "{err:?}");
}
