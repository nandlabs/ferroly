#![cfg(feature = "grpc")]
//! The gRPC client against real HTTP/2 transport events, from a scripted
//! HTTP/2 server built from raw frames: the status code of every RST_STREAM
//! code, GOAWAY, EOF and TCP reset, each sent once (GRPC-3, GRPC-12, RPC-6); transparent retry of
//! refused streams, including a replayed streamed upload and calls that
//! straddle a GOAWAY (GRPC-5); a peer that resets at its own deadline
//! (GRPC-4); benign RST_STREAM(NO_ERROR) after early trailers keeping the
//! connection (GRPC-2); gzip responses; and, against ferroly's own server,
//! many cancellations and deadline expiries keeping one connection alive
//! (GRPC-1), compressed requests, interceptors, deadline propagation, the
//! streaming-pump panic (GRPC-14) and health on shutdown.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroly::grpc::health::{
    HealthCheckRequest, HealthCheckResponse, HealthReporter, ServingStatus, WATCH_PATH,
};
use ferroly::grpc::{
    encode_frame, Channel, Code, FrameDecoder, GrpcServer, Request, Response, Status, Streaming,
};
use ferroly::http::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};

#[derive(ferroly::proto::Message, Clone, PartialEq, Debug, Default)]
struct Msg {
    #[proto(tag = 1)]
    text: String,
    #[proto(tag = 2)]
    n: u32,
}

fn msg(text: &str, n: u32) -> Msg {
    Msg {
        text: text.into(),
        n,
    }
}

// ---------------------------------------------------------------------------
// A scripted HTTP/2 server.

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const RST_STREAM: u8 = 0x3;
const SETTINGS: u8 = 0x4;
const PING: u8 = 0x6;
const GOAWAY: u8 = 0x7;
const WINDOW_UPDATE: u8 = 0x8;
const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const ACK: u8 = 0x1;

const NO_ERROR: u32 = 0x0;
const PROTOCOL_ERROR: u32 = 0x1;
const INTERNAL_ERROR: u32 = 0x2;
const REFUSED_STREAM: u32 = 0x7;
const CANCEL: u32 = 0x8;
const ENHANCE_YOUR_CALM: u32 = 0xb;
const INADEQUATE_SECURITY: u32 = 0xc;

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// An HPACK literal field without indexing, new name, no Huffman (names and
/// values here are shorter than 127 bytes).
fn lit(out: &mut Vec<u8>, name: &str, value: &str) {
    out.push(0x00);
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
}

/// `:status: 200` (static table index 8) and the gRPC content type.
fn response_head(extra: &[(&str, &str)]) -> Vec<u8> {
    let mut b = vec![0x88];
    lit(&mut b, "content-type", "application/grpc");
    for (k, v) in extra {
        lit(&mut b, k, v);
    }
    b
}

fn trailers(status: u32) -> Vec<u8> {
    let mut b = Vec::new();
    lit(&mut b, "grpc-status", &status.to_string());
    b
}

fn rst(stream: u32, code: u32) -> Vec<u8> {
    frame(RST_STREAM, 0, stream, &code.to_be_bytes())
}

/// Credits `len` bytes back to the connection and `stream` flow-control
/// windows.
fn window_update(stream: u32, len: usize) -> Vec<u8> {
    let inc = (len as u32).to_be_bytes();
    let mut out = frame(WINDOW_UPDATE, 0, 0, &inc);
    out.extend(frame(WINDOW_UPDATE, 0, stream, &inc));
    out
}

fn goaway(last: u32, code: u32) -> Vec<u8> {
    let mut p = last.to_be_bytes().to_vec();
    p.extend_from_slice(&code.to_be_bytes());
    frame(GOAWAY, 0, 0, &p)
}

/// A full gRPC response on `stream`: headers, one message, OK trailers.
fn ok_response(stream: u32, reply: &Msg, extra: &[(&str, &str)], body: Option<Vec<u8>>) -> Vec<u8> {
    let mut out = frame(HEADERS, END_HEADERS, stream, &response_head(extra));
    let data = body.unwrap_or_else(|| encode_frame(reply, usize::MAX).unwrap());
    out.extend(frame(DATA, 0, stream, &data));
    out.extend(frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        stream,
        &trailers(0),
    ));
    out
}

/// What the scripted server does once a request has fully arrived.
enum Reply {
    /// Answer with one message echoing the request's message count.
    Ok,
    /// Answer with `bytes` as the DATA payload and `grpc-encoding: gzip`.
    Gzip(Vec<u8>),
    /// Answer like `Ok`, but with this content type.
    ContentType(&'static str),
    /// RST_STREAM with this code.
    Rst(u32),
    /// The response headers, then RST_STREAM with this code.
    HeadThenRst(u32),
    /// RST_STREAM with this code just after `Instant` (busy-waited on a
    /// blocking thread, so it is not rounded up to a timer tick).
    RstAt(Instant, u32),
    /// Trailers-only `grpc-status`, then RST_STREAM(NO_ERROR), as grpc-go
    /// does when it answers before the request ended.
    TrailersOnlyThenNoError(u32),
    /// GOAWAY with this last stream id and code, then close.
    GoAway(u32, u32),
    /// Close the TCP connection (EOF).
    Close,
    /// Reset the TCP connection (RST, via SO_LINGER 0).
    Reset,
}

/// What a request looked like when it arrived.
struct Arrived {
    conn: usize,
    stream: u32,
    /// The decoded request messages.
    messages: Vec<Msg>,
}

/// Every request: (connection, stream, its message texts).
type Log = Arc<Mutex<Vec<(usize, u32, Vec<String>)>>>;

type Script = Arc<dyn Fn(&Arrived) -> Reply + Send + Sync>;

struct Fake {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    /// Every request: (connection, stream, its message texts).
    log: Log,
}

async fn fake(script: impl Fn(&Arrived) -> Reply + Send + Sync + 'static) -> Fake {
    let script: Script = Arc::new(script);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (c, l) = (connections.clone(), log.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let conn = c.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(serve_fake(tcp, conn, script.clone(), l.clone()));
        }
    });
    Fake {
        addr,
        connections,
        log,
    }
}

async fn write(w: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>, bytes: &[u8]) {
    let _ = w.lock().await.write_all(bytes).await;
}

async fn serve_fake(tcp: TcpStream, conn: usize, script: Script, log: Log) {
    #[allow(deprecated)] // SO_LINGER 0 never blocks: it resets at once.
    let linger = tcp.set_linger(Some(Duration::ZERO));
    let (mut r, w) = tcp.into_split();
    let w = Arc::new(tokio::sync::Mutex::new(w));
    let mut preface = [0u8; 24];
    if r.read_exact(&mut preface).await.is_err() || preface != PREFACE {
        return;
    }
    write(&w, &frame(SETTINGS, 0, 0, &[])).await;
    let mut bodies: HashMap<u32, FrameDecoder> = HashMap::new();
    loop {
        let mut head = [0u8; 9];
        if r.read_exact(&mut head).await.is_err() {
            return;
        }
        let len = (usize::from(head[0]) << 16) | (usize::from(head[1]) << 8) | usize::from(head[2]);
        let (kind, flags) = (head[3], head[4]);
        let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
        let mut payload = vec![0u8; len];
        if r.read_exact(&mut payload).await.is_err() {
            return;
        }
        let ended = match kind {
            SETTINGS if flags & ACK == 0 => {
                write(&w, &frame(SETTINGS, ACK, 0, &[])).await;
                false
            }
            PING if flags & ACK == 0 => {
                write(&w, &frame(PING, ACK, 0, &payload)).await;
                false
            }
            HEADERS => {
                bodies.insert(stream, FrameDecoder::new(1 << 20));
                flags & END_STREAM != 0
            }
            DATA => {
                if let Some(d) = bodies.get_mut(&stream) {
                    d.push(&payload);
                }
                // Keep large uploads flowing.
                if len > 0 {
                    write(&w, &window_update(stream, len)).await;
                }
                flags & END_STREAM != 0
            }
            _ => false,
        };
        if !ended {
            continue;
        }
        let mut messages = Vec::new();
        if let Some(mut d) = bodies.remove(&stream) {
            while let Ok(Some(bytes)) = d.next_frame() {
                messages.push(<Msg as ferroly::proto::Message>::decode(&bytes).unwrap());
            }
        }
        log.lock().unwrap().push((
            conn,
            stream,
            messages.iter().map(|m| m.text.clone()).collect(),
        ));
        let arrived = Arrived {
            conn,
            stream,
            messages,
        };
        let reply = script(&arrived);
        let n = arrived.messages.len() as u32;
        match reply {
            Reply::Ok => write(&w, &ok_response(stream, &msg("ok", n), &[], None)).await,
            Reply::Gzip(body) => {
                write(
                    &w,
                    &ok_response(
                        stream,
                        &Msg::default(),
                        &[("grpc-encoding", "gzip")],
                        Some(body),
                    ),
                )
                .await
            }
            Reply::ContentType(ct) => {
                let mut head = vec![0x88];
                lit(&mut head, "content-type", ct);
                let mut out = frame(HEADERS, END_HEADERS, stream, &head);
                out.extend(frame(
                    DATA,
                    0,
                    stream,
                    &encode_frame(&msg("ok", n), usize::MAX).unwrap(),
                ));
                out.extend(frame(
                    HEADERS,
                    END_HEADERS | END_STREAM,
                    stream,
                    &trailers(0),
                ));
                write(&w, &out).await
            }
            Reply::Rst(code) => write(&w, &rst(stream, code)).await,
            Reply::HeadThenRst(code) => {
                let mut out = frame(HEADERS, END_HEADERS, stream, &response_head(&[]));
                out.extend(rst(stream, code));
                write(&w, &out).await;
            }
            Reply::RstAt(at, code) => {
                let w = w.clone();
                let runtime = tokio::runtime::Handle::current();
                tokio::task::spawn_blocking(move || {
                    while Instant::now() < at {
                        std::hint::spin_loop();
                    }
                    runtime.block_on(write(&w, &rst(stream, code)));
                });
            }
            Reply::TrailersOnlyThenNoError(status) => {
                let mut b = response_head(&[]);
                b.extend(trailers(status));
                let mut out = frame(HEADERS, END_HEADERS | END_STREAM, stream, &b);
                out.extend(rst(stream, NO_ERROR));
                write(&w, &out).await;
            }
            Reply::GoAway(last, code) => {
                write(&w, &goaway(last, code)).await;
                // Let the client read the GOAWAY before the close.
                tokio::time::sleep(Duration::from_millis(50)).await;
                return;
            }
            Reply::Close => {
                let _ = w.lock().await.shutdown().await;
                return;
            }
            Reply::Reset => {
                assert!(linger.is_ok());
                // Dropping both halves with SO_LINGER 0 sends a TCP RST.
                return;
            }
        }
    }
}

fn channel(addr: SocketAddr) -> Channel {
    Channel::new(&format!("http://{addr}")).unwrap()
}

async fn unary(ch: &Channel, req: Request<Msg>) -> Result<Response<Msg>, Status> {
    ch.unary::<Msg, Msg>("/t.S/M", req).await
}

// ---------------------------------------------------------------------------
// GRPC-3 / GRPC-12: one code per transport event, never "malformed http".

fn assert_clean_message(status: &Status) {
    let text = status.message().to_ascii_lowercase();
    assert!(
        !text.contains("malformed http"),
        "status message has the HTTP/1 prefix: {status}"
    );
}

#[tokio::test]
async fn rst_stream_codes_map_to_grpc_codes() {
    let cases = [
        (CANCEL, Code::Cancelled),
        (ENHANCE_YOUR_CALM, Code::ResourceExhausted),
        (INADEQUATE_SECURITY, Code::PermissionDenied),
        (PROTOCOL_ERROR, Code::Internal),
        (INTERNAL_ERROR, Code::Internal),
        (NO_ERROR, Code::Internal),
        (REFUSED_STREAM, Code::Unavailable),
    ];
    for (rst_code, want) in cases {
        let server = fake(move |_| Reply::Rst(rst_code)).await;
        let err = unary(&channel(server.addr), Request::new(msg("x", 0)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), want, "RST_STREAM({rst_code:#x}): {err}");
        assert_clean_message(&err);
        if rst_code != REFUSED_STREAM {
            // The server may have processed it: never sent twice.
            let log = server.log.lock().unwrap().clone();
            assert_eq!(log.len(), 1, "RST_STREAM({rst_code:#x}): {log:?}");
        }
    }
}

#[tokio::test]
async fn connection_failures_are_unavailable() {
    let goaway_error = fake(|_| Reply::GoAway(1, INTERNAL_ERROR)).await;
    let goaway_graceful = fake(|_| Reply::GoAway(1, NO_ERROR)).await;
    let eof = fake(|_| Reply::Close).await;
    let reset = fake(|_| Reply::Reset).await;
    for (what, server) in [
        ("GOAWAY(INTERNAL_ERROR) covering the stream", goaway_error),
        ("GOAWAY(NO_ERROR) covering the stream", goaway_graceful),
        ("EOF", eof),
        ("TCP reset", reset),
    ] {
        let err = unary(&channel(server.addr), Request::new(msg("x", 0)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::Unavailable, "{what}: {err}");
        assert_clean_message(&err);
        // The server may have processed it: never sent twice.
        let log = server.log.lock().unwrap().clone();
        assert_eq!(log.len(), 1, "{what}: {log:?}");
    }

    // Nothing listening: UNAVAILABLE too.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let err = unary(&channel(addr), Request::new(msg("x", 0)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err}");
    assert_clean_message(&err);
}

// ---------------------------------------------------------------------------
// GRPC-5: transparent retry.

#[tokio::test]
async fn refused_unary_calls_are_retried_transparently() {
    // The first stream of every connection is refused. The HTTP client's
    // at-once retry goes to a second connection (below the cap, it never goes
    // back to the one that refused it), whose first stream is refused too.
    // The paused gRPC-layer retry then takes the first connection, which has
    // a free slot, where the next stream is accepted: exactly three sends.
    let server = fake(|a| {
        if a.stream == 1 {
            Reply::Rst(REFUSED_STREAM)
        } else {
            Reply::Ok
        }
    })
    .await;
    let ch = channel(server.addr);
    let reply = unary(&ch, Request::new(msg("x", 0))).await.unwrap();
    assert_eq!(reply.into_inner(), msg("ok", 1));
    let sends: Vec<(usize, u32)> = server
        .log
        .lock()
        .unwrap()
        .iter()
        .map(|(conn, stream, _)| (*conn, *stream))
        .collect();
    assert_eq!(sends.len(), 3, "{sends:?}");
    let (first, second) = (sends[0].0, sends[1].0);
    assert_ne!(first, second, "{sends:?}");
    assert_eq!(sends, [(first, 1), (second, 1), (first, 3)]);

    // Server streaming is buffered and retried the same way.
    let mut stream = ch
        .server_streaming::<Msg, Msg>("/t.S/M", Request::new(msg("y", 0)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.message().await.unwrap(), Some(msg("ok", 1)));
    assert_eq!(stream.message().await.unwrap(), None);
}

#[tokio::test]
async fn refused_streamed_uploads_are_replayed() {
    // Every connection refuses its first stream once the whole upload has
    // arrived, so the client has sent all of it: only a replay recovers.
    let server = fake(|a| {
        if a.stream == 1 {
            Reply::Rst(REFUSED_STREAM)
        } else {
            Reply::Ok
        }
    })
    .await;
    let ch = channel(server.addr);
    let messages: Vec<Msg> = (0..5).map(|i| msg(&format!("m{i}"), i)).collect();
    let reply = ch
        .client_streaming::<Msg, Msg>("/t.S/M", Request::new(Streaming::from_messages(messages)))
        .await
        .unwrap();
    assert_eq!(reply.into_inner(), msg("ok", 5));
    let log = server.log.lock().unwrap().clone();
    let want: Vec<String> = (0..5).map(|i| format!("m{i}")).collect();
    assert!(log.len() >= 2, "{log:?}");
    // The refused attempt and the replay each carried every message.
    for (_, _, texts) in &log {
        assert_eq!(texts, &want, "{log:?}");
    }

    // Bidirectional: the same.
    let mut responses = ch
        .bidi_streaming::<Msg, Msg>(
            "/t.S/M",
            Request::new(Streaming::from_messages(vec![msg("a", 1), msg("b", 2)])),
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(responses.message().await.unwrap(), Some(msg("ok", 2)));
}

#[tokio::test]
async fn calls_beyond_a_goaway_last_stream_id_move_to_a_new_connection() {
    // The first connection answers its first call with GOAWAY(last = 0):
    // that stream was never processed, so it is retried on a second one.
    let server = fake(|a| {
        if a.conn == 0 {
            Reply::GoAway(0, NO_ERROR)
        } else {
            Reply::Ok
        }
    })
    .await;
    let ch = channel(server.addr);
    assert_eq!(
        unary(&ch, Request::new(msg("x", 0)))
            .await
            .unwrap()
            .into_inner(),
        msg("ok", 1)
    );
    assert!(server.connections.load(Ordering::SeqCst) >= 2);

    // A streamed upload straddling the GOAWAY is replayed as well.
    let server = fake(|a| {
        if a.conn == 0 {
            Reply::GoAway(0, NO_ERROR)
        } else {
            Reply::Ok
        }
    })
    .await;
    let ch = channel(server.addr);
    let reply = ch
        .client_streaming::<Msg, Msg>(
            "/t.S/M",
            Request::new(Streaming::from_messages(vec![msg("a", 1), msg("b", 2)])),
        )
        .await
        .unwrap();
    assert_eq!(reply.into_inner(), msg("ok", 2));
}

#[tokio::test]
async fn a_server_that_refuses_everything_fails_unavailable_after_bounded_retries() {
    // A unary call: the HTTP client's at-once retry on a second connection,
    // then the paused retry (and its own at-once retry, on the other of the
    // two connections): four sends.
    let server = fake(|_| Reply::Rst(REFUSED_STREAM)).await;
    let started = Instant::now();
    let err = unary(&channel(server.addr), Request::new(msg("x", 0)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err}");
    assert_eq!(server.log.lock().unwrap().len(), 4);
    assert!(started.elapsed() >= Duration::from_millis(25));

    // A streamed upload refused once it has arrived: at once, then paused.
    let server = fake(|_| Reply::Rst(REFUSED_STREAM)).await;
    let err = channel(server.addr)
        .client_streaming::<Msg, Msg>(
            "/t.S/M",
            Request::new(Streaming::from_messages(vec![msg("a", 1)])),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err}");
    assert_eq!(server.log.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_refusal_after_the_response_head_is_not_retried() {
    // The server answered (it may have processed the call): a later
    // REFUSED_STREAM is an error, never a second send.
    let server = fake(|_| Reply::HeadThenRst(REFUSED_STREAM)).await;
    let err = unary(&channel(server.addr), Request::new(msg("x", 0)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err}");
    assert_eq!(server.log.lock().unwrap().len(), 1);

    let server = fake(|_| Reply::HeadThenRst(REFUSED_STREAM)).await;
    let result = channel(server.addr)
        .client_streaming::<Msg, Msg>(
            "/t.S/M",
            Request::new(Streaming::from_messages(vec![msg("a", 1)])),
        )
        .await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(server.log.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_refused_upload_larger_than_the_replay_buffer_fails_unavailable() {
    // 320 KiB sent before the refusal: more than the 256 KiB kept for a
    // replay, so the call fails instead of being sent again.
    let server = fake(|a| {
        if a.stream == 1 {
            Reply::Rst(REFUSED_STREAM)
        } else {
            Reply::Ok
        }
    })
    .await;
    let big = "x".repeat(64 * 1024);
    let messages: Vec<Msg> = (0..5).map(|i| msg(&big, i)).collect();
    let err = channel(server.addr)
        .client_streaming::<Msg, Msg>("/t.S/M", Request::new(Streaming::from_messages(messages)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{err}");
    let log = server.log.lock().unwrap().clone();
    assert_eq!(log.len(), 1, "{} sends", log.len());
    assert_eq!(log[0].2.len(), 5);
}

/// A server that refuses the first stream it sees as soon as its HEADERS
/// arrive (after `delay`), and answers each later stream's first message at
/// once with headers and one message, leaving the stream open (like a bidi
/// ping-pong server).
async fn refuse_first_headers(delay: Duration) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let streams = Arc::new(AtomicUsize::new(0));
    let seen = streams.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else {
                return;
            };
            let streams = seen.clone();
            tokio::spawn(async move {
                let mut preface = [0u8; 24];
                if tcp.read_exact(&mut preface).await.is_err() {
                    return;
                }
                let _ = tcp.write_all(&frame(SETTINGS, 0, 0, &[])).await;
                let mut refused = None;
                loop {
                    let mut head = [0u8; 9];
                    if tcp.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let len = (usize::from(head[0]) << 16)
                        | (usize::from(head[1]) << 8)
                        | usize::from(head[2]);
                    let (kind, flags) = (head[3], head[4]);
                    let stream =
                        u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
                    let mut payload = vec![0u8; len];
                    if tcp.read_exact(&mut payload).await.is_err() {
                        return;
                    }
                    let out = match kind {
                        SETTINGS if flags & ACK == 0 => frame(SETTINGS, ACK, 0, &[]),
                        HEADERS => {
                            if streams.fetch_add(1, Ordering::SeqCst) == 0 {
                                refused = Some(stream);
                                tokio::time::sleep(delay).await;
                                rst(stream, REFUSED_STREAM)
                            } else {
                                Vec::new()
                            }
                        }
                        DATA if len > 0 && Some(stream) != refused => {
                            let mut out = frame(HEADERS, END_HEADERS, stream, &response_head(&[]));
                            out.extend(frame(
                                DATA,
                                0,
                                stream,
                                &encode_frame(&msg("pong", 1), usize::MAX).unwrap(),
                            ));
                            out.extend(window_update(stream, len));
                            out
                        }
                        _ => Vec::new(),
                    };
                    if !out.is_empty() && tcp.write_all(&out).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, streams)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_call_with_an_idle_request_stream_is_replayed() {
    // Ping-pong: one request is queued, the next would be sent only after
    // the answer. The refused attempt must hand the request stream to the
    // retry even though nothing more arrives on it.
    for delay in [Duration::ZERO, Duration::from_millis(20)] {
        let (addr, streams) = refuse_first_headers(delay).await;
        let ch = channel(addr);
        let (tx, requests) = Streaming::<Msg>::channel(4);
        tx.send(msg("ping", 1)).await.unwrap();
        let call = async {
            let mut responses = ch
                .bidi_streaming::<Msg, Msg>("/t.S/Chat", Request::new(requests))
                .await?
                .into_inner();
            responses.message().await
        };
        let got = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap_or_else(|_| panic!("refused after {delay:?}: the call hung"));
        assert_eq!(
            got.unwrap(),
            Some(msg("pong", 1)),
            "refused after {delay:?}"
        );
        assert!(streams.load(Ordering::SeqCst) >= 2);
        drop(tx);
    }
}

// ---------------------------------------------------------------------------
// GRPC-4: a peer that resets at its own deadline.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_reset_at_the_deadline_is_deadline_exceeded() {
    // Like grpc-go: the server cancels the stream (RST_STREAM(CANCEL)) as
    // soon as the deadline has passed, which beats the client's own timer
    // (rounded up to the next millisecond tick): still DEADLINE_EXCEEDED.
    let deadline = Instant::now() + Duration::from_millis(200);
    let server = fake(move |_| Reply::RstAt(deadline + Duration::from_micros(100), CANCEL)).await;
    let ch = channel(server.addr);
    let calls = (0..24).map(|_| {
        let ch = ch.clone();
        async move { unary(&ch, Request::new(msg("x", 0)).with_deadline(deadline)).await }
    });
    let mut handles = Vec::new();
    for call in calls {
        handles.push(tokio::spawn(call));
    }
    for h in handles {
        let err = h.await.unwrap().unwrap_err();
        assert_eq!(err.code(), Code::DeadlineExceeded, "{err}");
    }
}

// ---------------------------------------------------------------------------
// GRPC-2: RST_STREAM(NO_ERROR) after a complete response is benign.

#[tokio::test]
async fn no_error_resets_after_early_trailers_keep_the_connection() {
    let server = fake(|_| Reply::TrailersOnlyThenNoError(5)).await;
    let ch = channel(server.addr);
    for _ in 0..8 {
        let calls: Vec<_> = (0..50)
            .map(|_| {
                let ch = ch.clone();
                tokio::spawn(async move { unary(&ch, Request::new(msg("x", 0))).await })
            })
            .collect();
        for c in calls {
            let err = c.await.unwrap().unwrap_err();
            assert_eq!(err.code(), Code::NotFound, "{err}");
        }
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Compression.

/// CRC-32 (IEEE), bitwise.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// `data` as a gzip member of one stored block.
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1];
    let len = data.len() as u16;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// A compressed gRPC frame (flag 1) around `compressed`.
fn compressed_frame(compressed: &[u8]) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    out.extend_from_slice(compressed);
    out
}

fn bomb() -> Vec<u8> {
    // 4 MiB of zeros in a few KiB of gzip.
    let hex = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/http/testdata/inflate_bomb_gzip.hex"
    ));
    let digits: Vec<u8> = hex.bytes().filter(u8::is_ascii_hexdigit).collect();
    digits
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

#[tokio::test]
async fn gzip_responses_are_decoded_and_bounded() {
    let body = <Msg as ferroly::proto::Message>::encode_to_vec(&msg("zipped", 7));
    let good = compressed_frame(&gzip_stored(&body));
    let server = fake(move |_| Reply::Gzip(good.clone())).await;
    let reply = unary(&channel(server.addr), Request::new(msg("x", 0)))
        .await
        .unwrap();
    assert_eq!(reply.into_inner(), msg("zipped", 7));

    // A decompression bomb stops at the decoding limit.
    let bomb = compressed_frame(&bomb());
    let server = fake(move |_| Reply::Gzip(bomb.clone())).await;
    let ch = Channel::builder(&format!("http://{}", server.addr))
        .unwrap()
        .max_decoding_message_size(64 * 1024)
        .build()
        .unwrap();
    let err = unary(&ch, Request::new(msg("x", 0))).await.unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{err}");
}

#[tokio::test]
async fn only_grpc_content_types_are_accepted_in_responses() {
    // GRPC-11: `application/grpc-web+proto` starts with `application/grpc`
    // but is not gRPC.
    for (ct, ok) in [
        ("application/grpc", true),
        ("application/grpc+proto", true),
        ("application/grpc; charset=utf-8", true),
        ("application/grpc-web+proto", false),
        ("application/grpc-web", false),
        ("application/grpc+json", false),
        ("text/plain", false),
    ] {
        let server = fake(move |_| Reply::ContentType(ct)).await;
        let result = unary(&channel(server.addr), Request::new(msg("x", 0))).await;
        match result {
            Ok(_) => assert!(ok, "{ct} accepted"),
            Err(e) => {
                assert!(!ok, "{ct}: {e}");
                assert_eq!(e.code(), Code::Unknown, "{ct}: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Against ferroly's own server.

async fn serve(server: GrpcServer) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(server.serve(listener, async {
        let _ = stopped.await;
    }));
    (addr, stop)
}

/// A server whose `/t.S/Sleep` waits `n` milliseconds and records the
/// client port of every call.
fn sleepy(ports: Arc<Mutex<HashSet<u16>>>) -> GrpcServer {
    GrpcServer::new().unary("/t.S/Sleep", move |req: Request<Msg>| {
        let ports = ports.clone();
        async move {
            if let Some(addr) = req.peer_addr() {
                ports.lock().unwrap().insert(addr.port());
            }
            let n = req.into_inner().n;
            tokio::time::sleep(Duration::from_millis(u64::from(n))).await;
            Ok(Response::new(msg("slept", n)))
        }
    })
}

#[tokio::test]
async fn many_cancellations_and_deadline_expiries_keep_the_connection() {
    // GRPC-1: 400 calls ended by the client (deadline expiry, or the future
    // dropped) on one connection within seconds must not be taken for a
    // rapid-reset attack, and must not fail an unrelated call.
    let ports = Arc::new(Mutex::new(HashSet::new()));
    let (addr, _stop) = serve(sleepy(ports.clone())).await;
    let ch = channel(addr);
    let long = {
        let ch = ch.clone();
        tokio::spawn(async move {
            ch.unary::<Msg, Msg>("/t.S/Sleep", Request::new(msg("long", 3000)))
                .await
        })
    };
    let started = Instant::now();
    for round in 0..8 {
        let calls: Vec<_> = (0..50)
            .map(|i| {
                let ch = ch.clone();
                tokio::spawn(async move {
                    let req = Request::new(msg("x", 10_000));
                    if (round + i) % 2 == 0 {
                        // A deadline expiry, as short as grpc-go clients
                        // commonly use.
                        let err = ch
                            .unary::<Msg, Msg>(
                                "/t.S/Sleep",
                                req.with_timeout(Duration::from_millis(20)),
                            )
                            .await
                            .unwrap_err();
                        assert_eq!(err.code(), Code::DeadlineExceeded, "{err}");
                    } else {
                        // A cancellation: the call's future is dropped.
                        let call = ch.unary::<Msg, Msg>("/t.S/Sleep", req);
                        let _ = tokio::time::timeout(Duration::from_millis(20), call).await;
                    }
                })
            })
            .collect();
        for c in calls {
            c.await.unwrap();
        }
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    let reply = long.await.unwrap().unwrap();
    assert_eq!(reply.into_inner(), msg("slept", 3000));
    // One more call after the storm, still on the same connection.
    ch.unary::<Msg, Msg>("/t.S/Sleep", Request::new(msg("after", 1)))
        .await
        .unwrap();
    assert_eq!(
        ports.lock().unwrap().len(),
        1,
        "{:?}",
        ports.lock().unwrap()
    );
}

#[test]
fn serve_uses_grpc_http2_defaults() {
    // GRPC-6 / GRPC-7: no per-connection stream cap, no stall reset.
    let config = GrpcServer::server_config();
    let h2 = config.http2.expect("HTTP/2 is on");
    assert_eq!(h2.stream_stall_timeout, None);
    assert_eq!(h2.max_streams_per_connection, None);
    assert_eq!(h2.rapid_reset_threshold, Duration::from_millis(10));
}

#[tokio::test]
async fn more_than_a_thousand_calls_share_one_connection() {
    // GRPC-6: no GOAWAY after 1000 calls.
    let ports = Arc::new(Mutex::new(HashSet::new()));
    let (addr, _stop) = serve(sleepy(ports.clone())).await;
    let ch = channel(addr);
    for _ in 0..12 {
        let calls: Vec<_> = (0..100)
            .map(|_| {
                let ch = ch.clone();
                tokio::spawn(async move {
                    ch.unary::<Msg, Msg>("/t.S/Sleep", Request::new(msg("x", 0)))
                        .await
                })
            })
            .collect();
        for c in calls {
            c.await.unwrap().unwrap();
        }
    }
    assert!(
        ports.lock().unwrap().len() <= 4,
        "{:?}",
        ports.lock().unwrap()
    );
}

#[tokio::test]
async fn compressed_requests_are_decoded() {
    let server = GrpcServer::new()
        .max_decoding_message_size(64 * 1024)
        .unary("/t.S/Echo", |req: Request<Msg>| async move {
            Ok(Response::new(req.into_inner()))
        });
    let (addr, _stop) = serve(server).await;
    let client = Client::builder()
        .http2_prior_knowledge(true)
        .build()
        .unwrap();
    let send = |encoding: &'static str, body: Vec<u8>| {
        let client = client.clone();
        async move {
            let req = ferroly::http::Request::builder(
                ferroly::http::Method::Post,
                &format!("http://{addr}/t.S/Echo"),
            )
            .unwrap()
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .header("grpc-encoding", encoding)
            .body(body)
            .build();
            let mut resp = client.send(req).await.unwrap();
            let mut data = Vec::new();
            while let Some(chunk) = resp.chunk().await.unwrap() {
                data.extend(chunk);
            }
            let headers = resp.headers().clone();
            let status = resp
                .trailers()
                .and_then(|t| t.get("grpc-status").map(str::to_string))
                .or_else(|| headers.get("grpc-status").map(str::to_string));
            (status, headers, data)
        }
    };

    let body = <Msg as ferroly::proto::Message>::encode_to_vec(&msg("zipped", 3));
    let (status, headers, data) = send("gzip", compressed_frame(&gzip_stored(&body))).await;
    assert_eq!(status.as_deref(), Some("0"));
    assert_eq!(
        headers.get("grpc-accept-encoding"),
        Some("identity,gzip,deflate")
    );
    assert_eq!(headers.get("grpc-encoding"), None, "responses are identity");
    assert_eq!(data, encode_frame(&msg("zipped", 3), usize::MAX).unwrap());

    // A decompression bomb is cut off at the decoding limit.
    let (status, _, _) = send("gzip", compressed_frame(&bomb())).await;
    assert_eq!(status.as_deref(), Some("8"));
    // Corrupt gzip is INTERNAL.
    let mut corrupt = gzip_stored(&body);
    let n = corrupt.len();
    corrupt[n - 5] ^= 0xff;
    let (status, _, _) = send("gzip", compressed_frame(&corrupt)).await;
    assert_eq!(status.as_deref(), Some("13"));
}

#[tokio::test]
async fn te_trailers_can_be_required() {
    // GRPC-13: lenient by default, like grpc-go; strict on request.
    for strict in [false, true] {
        let server = GrpcServer::new()
            .require_te_trailers(strict)
            .unary("/t.S/Echo", |req: Request<Msg>| async move {
                Ok(Response::new(req.into_inner()))
            });
        let (addr, _stop) = serve(server).await;
        let client = Client::builder()
            .http2_prior_knowledge(true)
            .build()
            .unwrap();
        for te in [None, Some("trailers")] {
            let mut req = ferroly::http::Request::builder(
                ferroly::http::Method::Post,
                &format!("http://{addr}/t.S/Echo"),
            )
            .unwrap()
            .header("content-type", "application/grpc")
            .body(encode_frame(&msg("x", 1), usize::MAX).unwrap())
            .build();
            if let Some(te) = te {
                req.headers.set("te", te);
            }
            let mut resp = client.send(req).await.unwrap();
            while resp.chunk().await.unwrap().is_some() {}
            let status = resp
                .trailers()
                .and_then(|t| t.get("grpc-status").map(str::to_string))
                .or_else(|| resp.headers().get("grpc-status").map(str::to_string));
            let want = if strict && te.is_none() { "13" } else { "0" };
            assert_eq!(status.as_deref(), Some(want), "strict {strict}, te {te:?}");
        }
    }
}

#[tokio::test]
async fn interceptors_on_both_sides() {
    let server = GrpcServer::new()
        .interceptor(|path: &str, req: &mut Request<()>| {
            assert_eq!(path, "/t.S/Who");
            match req.metadata().get("authorization") {
                Some("Bearer good") => {
                    req.metadata_mut().insert("x-user", "ada")?;
                    Ok(())
                }
                _ => Err(Status::unauthenticated("no token")),
            }
        })
        .unary("/t.S/Who", |req: Request<Msg>| async move {
            let user = req.metadata().get("x-user").unwrap_or("").to_string();
            Ok(Response::new(msg(&user, 0)))
        });
    let (addr, _stop) = serve(server).await;

    let anonymous = channel(addr);
    let err = anonymous
        .unary::<Msg, Msg>("/t.S/Who", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated, "{err}");

    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let authed = Channel::builder(&format!("http://{addr}"))
        .unwrap()
        .interceptor(move |_: &str, req: &mut Request<()>| {
            counted.fetch_add(1, Ordering::SeqCst);
            req.metadata_mut().insert("authorization", "Bearer good")
        })
        .build()
        .unwrap();
    let reply = authed
        .unary::<Msg, Msg>("/t.S/Who", Request::new(Msg::default()))
        .await
        .unwrap();
    assert_eq!(reply.into_inner().text, "ada");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A client interceptor's error fails the call without sending it.
    let refusing = Channel::builder(&format!("http://{addr}"))
        .unwrap()
        .interceptor(|_: &str, _: &mut Request<()>| Err(Status::aborted("not today")))
        .build()
        .unwrap();
    let err = refusing
        .unary::<Msg, Msg>("/t.S/Who", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Aborted);
}

#[tokio::test]
async fn a_panicking_server_interceptor_ends_the_call_with_internal() {
    let panics = Arc::new(Mutex::new(Vec::new()));
    let seen = panics.clone();
    let server = GrpcServer::new()
        .interceptor(|path: &str, _: &mut Request<()>| {
            if path == "/t.S/Boom" {
                panic!("interceptor exploded");
            }
            Ok(())
        })
        .on_handler_panic(move |path, message| {
            seen.lock().unwrap().push(format!("{path}: {message}"));
        })
        .unary("/t.S/Boom", |_: Request<Msg>| async {
            Ok(Response::new(msg("x", 0)))
        })
        .unary("/t.S/Fine", |_: Request<Msg>| async {
            Ok(Response::new(msg("fine", 0)))
        });
    let (addr, _stop) = serve(server).await;
    let ch = channel(addr);
    let err = ch
        .unary::<Msg, Msg>("/t.S/Boom", Request::new(Msg::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Internal, "{err}");
    assert!(!err.message().contains("exploded"), "{err}");
    assert_eq!(
        panics.lock().unwrap().clone(),
        ["/t.S/Boom: interceptor exploded"]
    );
    // The connection and the server carry on.
    let ok = ch
        .unary::<Msg, Msg>("/t.S/Fine", Request::new(Msg::default()))
        .await
        .unwrap();
    assert_eq!(ok.into_inner().text, "fine");
}

#[tokio::test]
async fn deadlines_propagate_to_outgoing_calls() {
    // A front service calls a slow back service with its own incoming
    // deadline: the downstream call gives up when the caller would.
    let (back, _stop_back) = serve(sleepy(Arc::default())).await;
    let back = channel(back);
    let front = GrpcServer::new().unary("/t.S/Front", move |req: Request<Msg>| {
        let back = back.clone();
        async move {
            let downstream = Request::new(msg("down", 5000)).inherit_deadline(&req);
            assert!(downstream.deadline().is_some());
            let started = Instant::now();
            let err = back
                .unary::<Msg, Msg>("/t.S/Sleep", downstream)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::DeadlineExceeded);
            assert!(started.elapsed() < Duration::from_secs(2));
            Err::<Response<Msg>, _>(err)
        }
    });
    let (addr, _stop) = serve(front).await;
    let err = channel(addr)
        .unary::<Msg, Msg>(
            "/t.S/Front",
            Request::new(Msg::default()).with_timeout(Duration::from_millis(300)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::DeadlineExceeded, "{err}");

    // A deadline already in the past fails before anything is sent.
    let err = channel(addr)
        .unary::<Msg, Msg>(
            "/t.S/Front",
            Request::new(Msg::default()).with_deadline(Instant::now()),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::DeadlineExceeded, "{err}");
}

/// A message whose encoding panics, to exercise the response pump.
#[derive(Debug, Default)]
struct Explosive;

impl ferroly::proto::Message for Explosive {
    fn encode_raw(&self, _: &mut Vec<u8>) {
        panic!("boom in encode");
    }
    fn encoded_len(&self) -> usize {
        1
    }
    fn merge_from(
        &mut self,
        r: &mut ferroly::proto::wire::Reader<'_>,
    ) -> Result<(), ferroly::proto::ProtoError> {
        while let Some((tag, wire_type)) = r.read_key()? {
            r.skip_field(tag, wire_type)?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_panic_in_the_response_stream_ends_the_call_with_internal() {
    // GRPC-14.
    let panics = Arc::new(Mutex::new(Vec::new()));
    let seen = panics.clone();
    let server = GrpcServer::new()
        .on_handler_panic(move |path, message| {
            seen.lock()
                .unwrap()
                .push((path.to_string(), message.to_string()));
        })
        .server_streaming("/t.S/Boom", |_: Request<Msg>| async move {
            Ok(Response::new(Streaming::from_messages(vec![Explosive])))
        });
    let (addr, _stop) = serve(server).await;
    let mut stream = channel(addr)
        .server_streaming::<Msg, Msg>("/t.S/Boom", Request::new(Msg::default()))
        .await
        .unwrap()
        .into_inner();
    let err = stream.message().await.unwrap_err();
    assert_eq!(err.code(), Code::Internal, "{err}");
    let panics = panics.lock().unwrap().clone();
    assert_eq!(panics.len(), 1);
    assert_eq!(panics[0].0, "/t.S/Boom");
    assert!(panics[0].1.contains("boom in encode"));
}

#[tokio::test]
async fn health_watch_streams_end_on_shutdown() {
    let health = HealthReporter::new();
    let server = GrpcServer::new().add_service(health.service());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(listener, async {
        let _ = stopped.await;
    }));
    let mut watch = channel(addr)
        .server_streaming::<HealthCheckRequest, HealthCheckResponse>(
            WATCH_PATH,
            Request::new(HealthCheckRequest::new("")),
        )
        .await
        .unwrap()
        .into_inner();
    let first = watch.message().await.unwrap().unwrap();
    assert_eq!(first.status, ServingStatus::Serving);

    let started = Instant::now();
    stop.send(()).unwrap();
    let next = watch.message().await.unwrap().unwrap();
    assert_eq!(next.status, ServingStatus::NotServing);
    assert_eq!(
        watch.message().await.unwrap(),
        None,
        "the Watch ends cleanly"
    );
    // The drain did not wait for its 30 s timeout.
    serving.await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(health.is_shut_down());
}

#[tokio::test]
async fn streaming_is_lazy_and_pollable() {
    // `from_messages` pulls one message per read: an unbounded iterator is
    // fine, and stops being pulled when the reader stops.
    let pulled = Arc::new(AtomicUsize::new(0));
    let counter = pulled.clone();
    let mut stream = Streaming::from_messages((0u32..).map(move |n| {
        counter.fetch_add(1, Ordering::SeqCst);
        msg("n", n)
    }));
    assert_eq!(stream.next().await.unwrap().unwrap().n, 0);
    assert_eq!(stream.message().await.unwrap().unwrap().n, 1);
    assert_eq!(pulled.load(Ordering::SeqCst), 2);

    // The poll form, driven by hand.
    let polled = std::future::poll_fn(|cx| stream.poll_message(cx)).await;
    assert_eq!(polled.unwrap().unwrap().n, 2);

    // A channel-fed stream that is still pending when polled once, then
    // finished through `message`.
    let (tx, mut stream) = Streaming::<Msg>::channel(1);
    struct Noop;
    impl std::task::Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = std::task::Waker::from(Arc::new(Noop));
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(stream.poll_message(&mut cx).is_pending());
    tx.send(msg("late", 9)).await.unwrap();
    assert_eq!(stream.message().await.unwrap(), Some(msg("late", 9)));
    tx.finish().await.unwrap();
    assert_eq!(Streaming::collect(stream).await.unwrap(), Vec::<Msg>::new());
}

#[tokio::test]
async fn a_cancelled_message_after_a_pending_poll_keeps_the_stream() {
    // `poll_message` returned Pending, so a read owns the source; `message`
    // finishes it, and dropping that `message` (a timeout, a `select!`
    // branch) must leave the read in place, not end the stream.
    let (tx, mut stream) = Streaming::<Msg>::channel(4);
    let pending =
        std::future::poll_fn(|cx| std::task::Poll::Ready(stream.poll_message(cx).is_pending()))
            .await;
    assert!(pending);
    let timed_out = tokio::time::timeout(Duration::from_millis(20), stream.message()).await;
    assert!(timed_out.is_err());
    tx.send(msg("after", 7)).await.unwrap();
    assert_eq!(stream.message().await.unwrap(), Some(msg("after", 7)));
    assert!(!tx.is_closed());
    tx.finish().await.unwrap();
    assert_eq!(stream.message().await.unwrap(), None);
}
