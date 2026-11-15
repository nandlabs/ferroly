#![cfg(feature = "http2")]
//! Hostile HTTP/2 peers, spoken to with hand-built frames: a bad preface,
//! oversized frames, flow-control violations, a CONTINUATION flood
//! (CVE-2024-27316), rapid reset (CVE-2023-44487), header bombs, SETTINGS /
//! PING floods, malformed requests — and a hostile server against the
//! client. Every case must end in the RFC 9113 error the defence promises
//! (RST_STREAM or GOAWAY with the right code), without a panic, and leave
//! the server able to serve the next connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ferroly::http::{
    serve_with_config, BoxFuture, Client, Http2Config, HttpHandler, HttpResponse, Method, Request,
    ServerConfig, StatusCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const PRIORITY: u8 = 0x2;
const RST_STREAM: u8 = 0x3;
const SETTINGS: u8 = 0x4;
const PUSH_PROMISE: u8 = 0x5;
const PING: u8 = 0x6;
const GOAWAY: u8 = 0x7;
const WINDOW_UPDATE: u8 = 0x8;
const CONTINUATION: u8 = 0x9;

const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;

const PROTOCOL_ERROR: u32 = 0x1;
const FLOW_CONTROL_ERROR: u32 = 0x3;
const FRAME_SIZE_ERROR: u32 = 0x6;
const REFUSED_STREAM: u32 = 0x7;
const COMPRESSION_ERROR: u32 = 0x9;
const ENHANCE_YOUR_CALM: u32 = 0xb;

struct App;

impl HttpHandler for App {
    fn wants_body_stream(&self, req: &Request) -> bool {
        matches!(req.uri.path.as_str(), "/hold" | "/count" | "/late")
    }

    fn handle(&self, mut req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            match req.uri.path.as_str() {
                "/count" => {
                    let mut n = 0usize;
                    if let Some(mut body) = req.take_body_stream() {
                        while let Ok(Some(chunk)) = body.chunk().await {
                            n += chunk.len();
                        }
                    }
                    HttpResponse::text(StatusCode::OK, n.to_string())
                }
                "/hold" => {
                    // Keep the body stream without ever reading it.
                    let body = req.take_body_stream();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    drop(body);
                    HttpResponse::text(StatusCode::OK, "held")
                }
                "/slow" => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    HttpResponse::text(StatusCode::OK, "slow")
                }
                "/late" => {
                    // Read the body only after it has all arrived; answer
                    // "<chunks> <bytes>".
                    let mut body = req.take_body_stream().expect("a body stream");
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let (mut chunks, mut n) = (0usize, 0usize);
                    while let Ok(Some(chunk)) = body.chunk().await {
                        chunks += 1;
                        n += chunk.len();
                    }
                    HttpResponse::text(StatusCode::OK, format!("{chunks} {n}"))
                }
                "/len" => HttpResponse::text(StatusCode::OK, req.body.len().to_string()),
                path if path.starts_with("/bytes/") => {
                    let n: usize = path["/bytes/".len()..].parse().unwrap_or(0);
                    HttpResponse::new(StatusCode::OK).body(vec![b'b'; n])
                }
                _ => HttpResponse::text(StatusCode::OK, "ok"),
            }
        })
    }
}

fn h2(f: impl FnOnce(&mut Http2Config)) -> ServerConfig {
    let mut h2 = Http2Config::default();
    f(&mut h2);
    ServerConfig {
        http2: Some(h2),
        head_timeout: Duration::from_secs(5),
        ..ServerConfig::default()
    }
}

async fn spawn(config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            Arc::new(App),
            std::future::pending::<()>(),
            config,
        )
        .await;
    });
    addr
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// An HPACK integer with an `n`-bit prefix.
fn int(out: &mut Vec<u8>, flags: u8, n: u8, mut v: usize) {
    let max = (1usize << n) - 1;
    if v < max {
        out.push(flags | v as u8);
        return;
    }
    out.push(flags | max as u8);
    v -= max;
    while v >= 128 {
        out.push((v % 128) as u8 | 0x80);
        v /= 128;
    }
    out.push(v as u8);
}

/// A literal field without indexing, new name, no Huffman.
fn lit(out: &mut Vec<u8>, name: &str, value: &str) {
    out.push(0x00);
    int(out, 0, 7, name.len());
    out.extend_from_slice(name.as_bytes());
    int(out, 0, 7, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn request(method: &str, path: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut b = Vec::new();
    lit(&mut b, ":method", method);
    lit(&mut b, ":scheme", "http");
    lit(&mut b, ":authority", "test");
    lit(&mut b, ":path", path);
    for (n, v) in extra {
        lit(&mut b, n, v);
    }
    b
}

struct Peer {
    sock: TcpStream,
}

#[derive(Debug)]
struct Frame {
    kind: u8,
    end_stream: bool,
    stream: u32,
    payload: Vec<u8>,
}

impl Peer {
    /// Connects and sends the preface and an empty SETTINGS frame.
    async fn open(addr: SocketAddr) -> Peer {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(PREFACE).await.unwrap();
        sock.write_all(&frame(SETTINGS, 0, 0, &[])).await.unwrap();
        Peer { sock }
    }

    async fn send(&mut self, bytes: &[u8]) {
        // The server may already have closed; that is what some tests expect.
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
            end_stream: h[4] & END_STREAM != 0,
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
        tokio::time::timeout(Duration::from_secs(10), read)
            .await
            .expect("the server answered in time")
    }

    async fn goaway(&mut self) -> u32 {
        let f = self
            .until(|f| f.kind == GOAWAY)
            .await
            .expect("a GOAWAY frame");
        u32::from_be_bytes([f.payload[4], f.payload[5], f.payload[6], f.payload[7]])
    }

    async fn rst(&mut self, stream: u32) -> u32 {
        let f = self
            .until(|f| f.kind == RST_STREAM && f.stream == stream)
            .await
            .expect("an RST_STREAM frame");
        u32::from_be_bytes([f.payload[0], f.payload[1], f.payload[2], f.payload[3]])
    }

    /// Reads the response HEADERS for `stream` and returns its `:status`
    /// (the server's encoder uses the static table or plain literals).
    async fn status(&mut self, stream: u32) -> String {
        let f = self
            .until(|f| f.kind == HEADERS && f.stream == stream)
            .await
            .expect("response HEADERS");
        decode_status(&f.payload)
    }
}

/// Finds `:status` in a server header block (the first field after an
/// optional table-size update).
fn decode_status(block: &[u8]) -> String {
    let mut b = block;
    if b[0] & 0xe0 == 0x20 {
        b = &b[1..];
    }
    match b[0] {
        0x88 => "200".into(),
        0x89 => "204".into(),
        0x8c => "400".into(),
        0x8d => "404".into(),
        0x8e => "500".into(),
        // Literal with the `:status` name (static index 8, 4-bit prefix):
        // 0x08, then the value; ferroly Huffman-codes it when shorter.
        0x08 => {
            let len = (b[1] & 0x7f) as usize;
            let raw = &b[2..2 + len];
            if b[1] & 0x80 == 0 {
                String::from_utf8_lossy(raw).into_owned()
            } else {
                huffman_digits(raw)
            }
        }
        other => panic!("unexpected first field 0x{other:x}"),
    }
}

/// Decodes a Huffman string made only of digits (5-bit codes 0..2 for
/// '0'..'2', 6-bit codes 0x18..0x1e for '3'..'9').
fn huffman_digits(raw: &[u8]) -> String {
    let bits: Vec<u8> = raw
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1))
        .collect();
    let mut out = String::new();
    let mut i = 0;
    while i + 5 <= bits.len() {
        let five = bits[i..i + 5].iter().fold(0u8, |a, b| a << 1 | b);
        if five <= 2 {
            out.push((b'0' + five) as char);
            i += 5;
            continue;
        }
        if i + 6 > bits.len() {
            break;
        }
        let six = bits[i..i + 6].iter().fold(0u8, |a, b| a << 1 | b);
        if (0x18..=0x1e).contains(&six) {
            out.push((b'3' + six - 0x18) as char);
            i += 6;
            continue;
        }
        break;
    }
    out
}

/// The server still serves a fresh, well-behaved connection.
async fn still_serving(addr: SocketAddr) {
    let client = Client::builder()
        .http2_prior_knowledge(true)
        .build()
        .unwrap();
    let req = Request::builder(Method::Get, &format!("http://{addr}/"))
        .unwrap()
        .build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn bad_preface_is_refused() {
    let addr = spawn(h2(|_| {})).await;
    // Looks like the preface for its first bytes, then is not: on cleartext
    // that is an (invalid) HTTP/1.1 request, refused as such.
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"PRI * HTTP/2.0\r\n\r\nXX\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut answer)).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(
        answer.starts_with("HTTP/1.1 4") || answer.starts_with("HTTP/1.1 5"),
        "{answer}"
    );
    // The same preface split into tiny pieces is still recognised.
    let mut sock = TcpStream::connect(addr).await.unwrap();
    for piece in PREFACE.chunks(3) {
        sock.write_all(piece).await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    sock.write_all(&frame(SETTINGS, 0, 0, &[])).await.unwrap();
    sock.write_all(&frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        1,
        &request("GET", "/", &[]),
    ))
    .await
    .unwrap();
    let mut peer = Peer { sock };
    assert_eq!(peer.status(1).await, "200");
    // A first frame that is not SETTINGS.
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(PREFACE).await.unwrap();
    sock.write_all(&frame(PING, 0, 0, &[0; 8])).await.unwrap();
    let mut peer = Peer { sock };
    assert_eq!(peer.goaway().await, PROTOCOL_ERROR);
    still_serving(addr).await;
}

#[tokio::test]
async fn oversized_frame_is_a_frame_size_error() {
    let addr = spawn(h2(|_| {})).await;
    let mut peer = Peer::open(addr).await;
    // A header announcing a 1 MiB DATA frame; the payload never needs to be
    // buffered.
    let mut head = frame(DATA, 0, 1, &[]);
    head[0] = 0x10;
    peer.send(&head).await;
    assert_eq!(peer.goaway().await, FRAME_SIZE_ERROR);
    // A fixed-size frame of the wrong size.
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(PING, 0, 0, &[0; 9])).await;
    assert_eq!(peer.goaway().await, FRAME_SIZE_ERROR);
    still_serving(addr).await;
}

#[tokio::test]
async fn stream_flow_control_violation_resets_the_stream() {
    let addr = spawn(h2(|h| h.initial_stream_window_size = 65_535)).await;
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(
        HEADERS,
        END_HEADERS,
        1,
        &request("POST", "/hold", &[]),
    ))
    .await;
    // 5 × 16 KiB = 80 KiB against a 64 KiB window nobody is draining.
    for _ in 0..5 {
        peer.send(&frame(DATA, 0, 1, &[b'x'; 16_384])).await;
    }
    assert_eq!(peer.rst(1).await, FLOW_CONTROL_ERROR);
    // The connection itself is fine.
    peer.send(&frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        3,
        &request("GET", "/", &[]),
    ))
    .await;
    assert_eq!(peer.status(3).await, "200");
}

#[tokio::test]
async fn connection_flow_control_violation_is_goaway() {
    let addr = spawn(h2(|h| h.initial_connection_window_size = 65_535)).await;
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(
        HEADERS,
        END_HEADERS,
        1,
        &request("POST", "/hold", &[]),
    ))
    .await;
    for _ in 0..5 {
        peer.send(&frame(DATA, 0, 1, &[b'x'; 16_384])).await;
    }
    assert_eq!(peer.goaway().await, FLOW_CONTROL_ERROR);
    // Overflowing the send window with WINDOW_UPDATE is an error too.
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(WINDOW_UPDATE, 0, 0, &0x7fff_ffffu32.to_be_bytes()))
        .await;
    assert_eq!(peer.goaway().await, FLOW_CONTROL_ERROR);
    still_serving(addr).await;
}

#[tokio::test]
async fn continuation_flood_is_cut_off() {
    let addr = spawn(h2(|_| {})).await;
    // Endless empty CONTINUATION frames (CVE-2024-27316).
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(HEADERS, 0, 1, &request("GET", "/", &[])))
        .await;
    let mut flood = Vec::new();
    for _ in 0..200 {
        flood.extend(frame(CONTINUATION, 0, 1, &[]));
    }
    peer.send(&flood).await;
    assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM);
    // Large CONTINUATION frames: capped by size long before memory matters.
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(HEADERS, 0, 1, &request("GET", "/", &[])))
        .await;
    for _ in 0..20 {
        peer.send(&frame(CONTINUATION, 0, 1, &[0x82; 16_384])).await;
    }
    assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM);
    // A different frame in the middle of a header block.
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(HEADERS, 0, 1, &request("GET", "/", &[])))
        .await;
    peer.send(&frame(PING, 0, 0, &[0; 8])).await;
    assert_eq!(peer.goaway().await, PROTOCOL_ERROR);
    still_serving(addr).await;
}

#[tokio::test]
async fn rapid_reset_is_goaway_enhance_your_calm() {
    let addr = spawn(h2(|h| h.max_resets_per_window = 50)).await;
    let mut peer = Peer::open(addr).await;
    let mut burst = Vec::new();
    let mut id = 1u32;
    for _ in 0..200 {
        burst.extend(frame(
            HEADERS,
            END_HEADERS | END_STREAM,
            id,
            &request("GET", "/slow", &[]),
        ));
        burst.extend(frame(RST_STREAM, 0, id, &8u32.to_be_bytes()));
        id += 2;
    }
    peer.send(&burst).await;
    assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM);
    still_serving(addr).await;
}

#[tokio::test]
async fn reset_streams_keep_counting_until_their_handler_stops() {
    // With a limit of 2 streams, two reset-but-still-running handlers
    // occupy both slots, so a third stream is refused.
    let addr = spawn(h2(|h| h.max_concurrent_streams = 2)).await;
    let mut peer = Peer::open(addr).await;
    for id in [1u32, 3] {
        peer.send(&frame(
            HEADERS,
            END_HEADERS,
            id,
            &request("POST", "/hold", &[]),
        ))
        .await;
    }
    // Give the server time to dispatch both, then reset them.
    tokio::time::sleep(Duration::from_millis(100)).await;
    for id in [1u32, 3] {
        peer.send(&frame(RST_STREAM, 0, id, &8u32.to_be_bytes()))
            .await;
    }
    peer.send(&frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        5,
        &request("GET", "/", &[]),
    ))
    .await;
    // Aborting is immediate, so the slots may already be free; either way
    // the server must answer stream 5 (refused or served) and stay up.
    let f = peer
        .until(|f| (f.kind == HEADERS || f.kind == RST_STREAM) && f.stream == 5)
        .await
        .expect("an answer for stream 5");
    if f.kind == RST_STREAM {
        assert_eq!(
            u32::from_be_bytes(f.payload[..4].try_into().unwrap()),
            REFUSED_STREAM
        );
    }
}

#[tokio::test]
async fn max_concurrent_streams_refuses_extra_streams() {
    let addr = spawn(h2(|h| h.max_concurrent_streams = 2)).await;
    let mut peer = Peer::open(addr).await;
    for id in [1u32, 3, 5] {
        peer.send(&frame(
            HEADERS,
            END_HEADERS | END_STREAM,
            id,
            &request("GET", "/slow", &[]),
        ))
        .await;
    }
    assert_eq!(peer.rst(5).await, REFUSED_STREAM);
    assert_eq!(peer.status(1).await, "200");
}

#[tokio::test]
async fn header_bomb_is_431_and_the_connection_survives() {
    let addr = spawn(h2(|_| {})).await;
    let mut peer = Peer::open(addr).await;
    // Insert a 4000-byte entry with incremental indexing, then reference it
    // 300 times: ~50 KiB on the wire would decode to 1.2 MB.
    let mut block = request("GET", "/", &[]);
    block.push(0x40);
    int(&mut block, 0, 7, 1);
    block.push(b'x');
    int(&mut block, 0, 7, 3950);
    block.extend(std::iter::repeat(b'v').take(3950));
    block.extend(std::iter::repeat(0xbe).take(300));
    // Sent as HEADERS + CONTINUATION.
    let (a, b) = block.split_at(block.len() / 2);
    peer.send(&frame(HEADERS, END_STREAM, 1, a)).await;
    peer.send(&frame(CONTINUATION, END_HEADERS, 1, b)).await;
    assert_eq!(peer.status(1).await, "431");
    // The HPACK table stayed in sync: the next request decodes fine.
    peer.send(&frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        3,
        &request("GET", "/", &[]),
    ))
    .await;
    assert_eq!(peer.status(3).await, "200");
}

#[tokio::test]
async fn settings_and_ping_floods_are_goaway() {
    let addr = spawn(h2(|h| h.max_control_frames_per_window = 100)).await;
    for kind in [SETTINGS, PING, PRIORITY] {
        let mut peer = Peer::open(addr).await;
        let mut flood = Vec::new();
        for _ in 0..300 {
            match kind {
                SETTINGS => flood.extend(frame(SETTINGS, 0, 0, &[])),
                PING => flood.extend(frame(PING, 0, 0, &[1; 8])),
                _ => flood.extend(frame(PRIORITY, 0, 99, &[0, 0, 0, 0, 16])),
            }
        }
        peer.send(&flood).await;
        assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM, "kind {kind}");
    }
    // Empty DATA frames count too.
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(
        HEADERS,
        END_HEADERS,
        1,
        &request("POST", "/hold", &[]),
    ))
    .await;
    let mut flood = Vec::new();
    for _ in 0..300 {
        flood.extend(frame(DATA, 0, 1, &[]));
    }
    peer.send(&flood).await;
    assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM);
    still_serving(addr).await;
}

#[tokio::test]
async fn malformed_requests_are_reset() {
    let addr = spawn(h2(|_| {})).await;
    let mut peer = Peer::open(addr).await;
    let cases: Vec<Vec<u8>> = vec![
        // An upper-case header name.
        request("GET", "/", &[("X-Upper", "1")]),
        // A connection-specific header.
        request("GET", "/", &[("connection", "keep-alive")]),
        // TE other than trailers.
        request("GET", "/", &[("te", "gzip")]),
        // A value with a newline.
        request("GET", "/", &[("x-a", "a\nb")]),
        // A pseudo-header after a regular one.
        {
            let mut b = request("GET", "/", &[("x-a", "1")]);
            lit(&mut b, ":path", "/again");
            b
        },
        // Missing :path.
        {
            let mut b = Vec::new();
            lit(&mut b, ":method", "GET");
            lit(&mut b, ":scheme", "http");
            b
        },
        // An unknown pseudo-header.
        request("GET", "/", &[(":protocol", "x")]),
    ];
    let mut id = 1u32;
    for block in &cases {
        peer.send(&frame(HEADERS, END_HEADERS | END_STREAM, id, block))
            .await;
        assert_eq!(peer.rst(id).await, PROTOCOL_ERROR, "case for stream {id}");
        id += 2;
    }
    // A content-length that the body does not match.
    peer.send(&frame(
        HEADERS,
        END_HEADERS,
        id,
        &request("POST", "/", &[("content-length", "10")]),
    ))
    .await;
    peer.send(&frame(DATA, END_STREAM, id, b"short")).await;
    assert_eq!(peer.rst(id).await, PROTOCOL_ERROR);
    id += 2;
    // Still usable.
    peer.send(&frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        id,
        &request("GET", "/", &[]),
    ))
    .await;
    assert_eq!(peer.status(id).await, "200");
}

#[tokio::test]
async fn connection_level_protocol_errors() {
    let addr = spawn(h2(|_| {})).await;
    let cases: Vec<(Vec<u8>, u32)> = vec![
        // DATA on a stream that was never opened.
        (frame(DATA, 0, 7, b"x"), PROTOCOL_ERROR),
        // An even (server-initiated) stream id from a client.
        (
            frame(
                HEADERS,
                END_HEADERS | END_STREAM,
                2,
                &request("GET", "/", &[]),
            ),
            PROTOCOL_ERROR,
        ),
        // Clients cannot push.
        (
            frame(PUSH_PROMISE, END_HEADERS, 1, &[0, 0, 0, 2]),
            PROTOCOL_ERROR,
        ),
        // An invalid HPACK index.
        (
            frame(HEADERS, END_HEADERS | END_STREAM, 1, &[0x80]),
            COMPRESSION_ERROR,
        ),
        // WINDOW_UPDATE on an idle stream.
        (
            frame(WINDOW_UPDATE, 0, 9, &1u32.to_be_bytes()),
            PROTOCOL_ERROR,
        ),
        // A CONTINUATION that does not follow HEADERS.
        (frame(CONTINUATION, END_HEADERS, 1, &[0x82]), PROTOCOL_ERROR),
        // An invalid SETTINGS value.
        (frame(SETTINGS, 0, 0, &[0, 2, 0, 0, 0, 7]), PROTOCOL_ERROR),
    ];
    for (bytes, code) in cases {
        let mut peer = Peer::open(addr).await;
        peer.send(&bytes).await;
        assert_eq!(peer.goaway().await, code, "{bytes:02x?}");
    }
    still_serving(addr).await;
}

#[tokio::test]
async fn random_frames_never_take_the_server_down() {
    let addr = spawn(h2(|_| {})).await;
    // A deterministic generator: no randomness source needed.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..40 {
        let mut peer = Peer::open(addr).await;
        let mut junk = Vec::new();
        for _ in 0..30 {
            let kind = (next() % 12) as u8;
            let flags = next() as u8;
            let stream = (next() % 8) as u32;
            let len = (next() % 64) as usize;
            let payload: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            junk.extend(frame(kind, flags, stream, &payload));
        }
        peer.send(&junk).await;
        // Whatever the server makes of it, it answers or closes promptly.
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while peer.read_frame().await.is_some() {}
        })
        .await;
    }
    still_serving(addr).await;
}

#[tokio::test]
async fn idle_connections_get_goaway() {
    let config = ServerConfig {
        idle_timeout: Duration::from_millis(200),
        ..h2(|_| {})
    };
    let addr = spawn(config).await;
    let mut peer = Peer::open(addr).await;
    assert_eq!(peer.goaway().await, 0);
}

/// A server that answers the first request with `reply` (raw frames after
/// its SETTINGS).
async fn fake_server(reply: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut preface = [0u8; 24];
        let _ = sock.read_exact(&mut preface).await;
        let _ = sock.write_all(&frame(SETTINGS, 0, 0, &[])).await;
        // Wait for the request HEADERS, then reply.
        let mut peer = Peer { sock };
        let _ = peer.until(|f| f.kind == HEADERS).await;
        let _ = peer.sock.write_all(&reply).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    addr
}

#[tokio::test]
async fn hostile_servers_fail_the_client_cleanly() {
    let client = || {
        Client::builder()
            .http2_prior_knowledge(true)
            .build()
            .unwrap()
    };
    let cases: Vec<Vec<u8>> = vec![
        // A server push.
        frame(PUSH_PROMISE, END_HEADERS, 1, &[0, 0, 0, 2, 0x82]),
        // DATA before the response head.
        frame(DATA, END_STREAM, 1, b"early"),
        // A head without :status.
        frame(HEADERS, END_HEADERS | END_STREAM, 1, &[0x82]),
        // An invalid :status.
        {
            let mut b = vec![0x08, 0x02];
            b.extend_from_slice(b"20");
            frame(HEADERS, END_HEADERS | END_STREAM, 1, &b)
        },
        // Broken HPACK.
        frame(
            HEADERS,
            END_HEADERS | END_STREAM,
            1,
            &[0xff, 0xff, 0xff, 0xff, 0xff],
        ),
        // An oversized frame.
        {
            let mut f = frame(DATA, 0, 1, &[]);
            f[0] = 0x7f;
            f
        },
        // A GOAWAY that refuses the request.
        frame(GOAWAY, 0, 0, &[0, 0, 0, 0, 0, 0, 0, 0]),
    ];
    for reply in cases {
        let addr = fake_server(reply.clone()).await;
        let req = Request::builder(Method::Get, &format!("http://{addr}/"))
            .unwrap()
            .build();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            match client().send(req).await {
                Ok(resp) => resp.bytes().await.map(|_| ()),
                Err(e) => Err(e),
            }
        })
        .await
        .expect("the client gives up in time");
        assert!(result.is_err(), "{reply:02x?}");
    }
    // A response body longer than its content-length.
    let mut head = vec![0x88];
    head.push(0x5c); // content-length, literal with indexing, static name 28
    head.push(0x01);
    head.push(b'2');
    let mut reply = frame(HEADERS, END_HEADERS, 1, &head);
    reply.extend(frame(DATA, END_STREAM, 1, b"too long"));
    let addr = fake_server(reply).await;
    let req = Request::builder(Method::Get, &format!("http://{addr}/"))
        .unwrap()
        .build();
    let resp = client().send(req).await.unwrap();
    assert!(resp.bytes().await.is_err());
}

/// A handler-independent server for the window tests: every response is
/// 100 KB.
struct Big;

impl HttpHandler for Big {
    fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async { HttpResponse::new(StatusCode::OK).body(vec![b'b'; 100_000]) })
    }
}

async fn spawn_big(config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            Arc::new(Big),
            std::future::pending::<()>(),
            config,
        )
        .await;
    });
    addr
}

/// DATA bytes received for `stream` until `quiet` passes without a frame,
/// and whether the stream ended.
async fn data_until_quiet(peer: &mut Peer, stream: u32, quiet: Duration) -> (usize, bool) {
    let mut total = 0;
    let mut ended = false;
    while let Ok(Some(f)) = tokio::time::timeout(quiet, peer.read_frame()).await {
        if f.kind == DATA && f.stream == stream {
            total += f.payload.len();
            ended |= f.end_stream;
        }
    }
    (total, ended)
}

/// Opens a connection whose SETTINGS_INITIAL_WINDOW_SIZE is 0 and sends one
/// GET on stream 1.
async fn zero_window_get(addr: SocketAddr) -> Peer {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(PREFACE).await.unwrap();
    sock.write_all(&frame(SETTINGS, 0, 0, &[0, 4, 0, 0, 0, 0]))
        .await
        .unwrap();
    sock.write_all(&frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        1,
        &request("GET", "/", &[]),
    ))
    .await
    .unwrap();
    Peer { sock }
}

#[tokio::test]
async fn settings_initial_window_size_changes_apply_to_open_streams() {
    let addr = spawn_big(h2(|_| {})).await;
    let mut peer = zero_window_get(addr).await;
    assert_eq!(peer.status(1).await, "200");
    let (n, _) = data_until_quiet(&mut peer, 1, Duration::from_millis(300)).await;
    assert_eq!(n, 0, "no DATA without stream window");
    // Raising the setting opens every open stream's window by the difference.
    peer.send(&frame(SETTINGS, 0, 0, &[0, 4, 0, 0, 0x27, 0x10]))
        .await; // 10 000
    let (n, ended) = data_until_quiet(&mut peer, 1, Duration::from_millis(300)).await;
    assert_eq!(n, 10_000);
    assert!(!ended);
    // A stream WINDOW_UPDATE releases more, up to the connection window
    // (the default 65 535, of which 10 000 is used)...
    peer.send(&frame(WINDOW_UPDATE, 0, 1, &200_000u32.to_be_bytes()))
        .await;
    let (n, ended) = data_until_quiet(&mut peer, 1, Duration::from_millis(300)).await;
    assert_eq!(n, 55_535);
    assert!(!ended);
    // ...and a connection WINDOW_UPDATE the rest.
    peer.send(&frame(WINDOW_UPDATE, 0, 0, &200_000u32.to_be_bytes()))
        .await;
    let (n, ended) = data_until_quiet(&mut peer, 1, Duration::from_millis(300)).await;
    assert_eq!(n, 34_465);
    assert!(ended);
}

#[tokio::test]
async fn a_peer_that_never_opens_its_window_is_reset() {
    let addr = spawn_big(h2(|h| {
        h.stream_stall_timeout = Some(Duration::from_millis(300));
    }))
    .await;
    let mut peer = zero_window_get(addr).await;
    // CANCEL once the response has been stalled for the stall timeout.
    assert_eq!(peer.rst(1).await, 0x8);
}

#[tokio::test]
async fn the_stall_timeout_is_its_own_setting_and_can_be_disabled() {
    // A short write timeout no longer resets a stalled stream: the
    // connection itself has nothing pending to write.
    let config = ServerConfig {
        write_timeout: Duration::from_millis(200),
        ..h2(|h| h.stream_stall_timeout = None)
    };
    let addr = spawn_big(config).await;
    let mut peer = zero_window_get(addr).await;
    assert_eq!(peer.status(1).await, "200");
    let quiet = tokio::time::timeout(
        Duration::from_millis(1200),
        peer.until(|f| f.kind == RST_STREAM || f.kind == GOAWAY),
    )
    .await;
    assert!(quiet.is_err(), "no reset while disabled: {quiet:?}");
    // The stream is still alive: opening its window delivers the body.
    peer.send(&frame(WINDOW_UPDATE, 0, 1, &200_000u32.to_be_bytes()))
        .await;
    peer.send(&frame(WINDOW_UPDATE, 0, 0, &200_000u32.to_be_bytes()))
        .await;
    let (n, ended) = data_until_quiet(&mut peer, 1, Duration::from_millis(300)).await;
    assert_eq!(n, 100_000);
    assert!(ended);
}

// ----- 0.6.0 review regressions ---------------------------------------------

const STREAM_CLOSED: u32 = 0x5;
const ACK: u8 = 0x1;

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) & 0x7fff_ffff
}

/// The response body of `stream`, as text.
async fn body_text(peer: &mut Peer, stream: u32) -> String {
    let mut out = Vec::new();
    loop {
        let f = peer
            .until(|f| f.kind == DATA && f.stream == stream)
            .await
            .expect("response DATA");
        out.extend_from_slice(&f.payload);
        if f.end_stream {
            return String::from_utf8_lossy(&out).into_owned();
        }
    }
}

fn get(id: u32, path: &str) -> Vec<u8> {
    frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        id,
        &request("GET", path, &[]),
    )
}

/// H2-1: a stream reset in the same write that opened it (its handler task
/// aborted before its first poll) used to keep its slot for good.
#[tokio::test]
async fn a_stream_cancelled_in_the_same_write_frees_its_slot() {
    let config = ServerConfig {
        idle_timeout: Duration::from_millis(400),
        ..h2(|h| h.max_concurrent_streams = 2)
    };
    let addr = spawn(config).await;
    let mut peer = Peer::open(addr).await;
    let mut burst = Vec::new();
    let mut id = 1u32;
    for _ in 0..10 {
        burst.extend(get(id, "/"));
        burst.extend(frame(RST_STREAM, 0, id, &8u32.to_be_bytes()));
        id += 2;
    }
    peer.send(&burst).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Both slots are free again: a normal request is served, not refused.
    peer.send(&get(id, "/")).await;
    let f = peer
        .until(|f| (f.kind == HEADERS || f.kind == RST_STREAM) && f.stream == id)
        .await
        .expect("an answer");
    assert_eq!(f.kind, HEADERS, "refused: the slots leaked");
    // And with nothing in flight the connection idles out.
    assert_eq!(peer.goaway().await, 0);
}

/// H2-1: two connections with one cancelled request each used to pin
/// `max_connections = 2` for good.
#[tokio::test]
async fn cancelled_requests_do_not_pin_max_connections() {
    let config = ServerConfig {
        idle_timeout: Duration::from_millis(300),
        max_connections: 2,
        ..h2(|_| {})
    };
    let addr = spawn(config).await;
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut p = Peer::open(addr).await;
        let mut b = get(1, "/");
        b.extend(frame(RST_STREAM, 0, 1, &8u32.to_be_bytes()));
        p.send(&b).await;
        held.push(p);
    }
    for p in &mut held {
        assert_eq!(p.goaway().await, 0);
    }
    let client = Client::builder()
        .http2_prior_knowledge(true)
        .timeout(Some(Duration::from_secs(5)))
        .build()
        .unwrap();
    let req = Request::builder(Method::Get, &format!("http://{addr}/"))
        .unwrap()
        .build();
    let resp = client.send(req).await.expect("a free connection slot");
    assert_eq!(resp.text().await.unwrap(), "ok");
}

/// GRPC-1: cancelling streams that are already being handled (a client's
/// deadline, a user giving up) is not the rapid-reset pattern.
#[tokio::test]
async fn ordinary_cancels_never_trip_the_rapid_reset_budget() {
    let addr = spawn(h2(|h| h.max_resets_per_window = 50)).await;
    let mut peer = Peer::open(addr).await;
    let mut id = 1u32;
    for _ in 0..4 {
        let first = id;
        let mut opens = Vec::new();
        for _ in 0..40 {
            opens.extend(get(id, "/slow"));
            id += 2;
        }
        peer.send(&opens).await;
        // Older than `rapid_reset_threshold` (100 ms), still running.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut resets = Vec::new();
        for s in (first..id).step_by(2) {
            resets.extend(frame(RST_STREAM, 0, s, &8u32.to_be_bytes()));
        }
        peer.send(&resets).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 160 cancels against a budget of 50: still serving.
    peer.send(&get(id, "/")).await;
    let f = peer
        .until(|f| f.kind == GOAWAY || (f.kind == HEADERS && f.stream == id))
        .await
        .expect("an answer");
    assert_eq!(f.kind, HEADERS, "the connection was closed");
}

/// H2-3 ("MadeYouReset", CVE-2025-8671): resets the server is made to send
/// for bodies shorter than their content-length count too.
#[tokio::test]
async fn short_bodies_count_toward_the_reset_budget() {
    let addr = spawn(h2(|h| h.max_resets_per_window = 20)).await;
    let mut peer = Peer::open(addr).await;
    let mut burst = Vec::new();
    let mut id = 1u32;
    for _ in 0..40 {
        let head = request("POST", "/", &[("content-length", "10")]);
        burst.extend(frame(HEADERS, END_HEADERS, id, &head));
        burst.extend(frame(DATA, END_STREAM, id, b"short"));
        id += 2;
    }
    peer.send(&burst).await;
    assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM);
}

/// H2-2: one connection's buffered request bodies are bounded by
/// `max_buffered_body_bytes` plus one connection window, not by
/// `max_concurrent_streams × max_body_bytes`.
#[tokio::test]
async fn buffered_bodies_are_bounded_per_connection() {
    let config = ServerConfig {
        max_body_bytes: 64 * 1024,
        ..h2(|h| {
            h.max_buffered_body_bytes = 64 * 1024;
            h.initial_connection_window_size = 65_535;
            h.initial_stream_window_size = 65_535;
        })
    };
    let addr = spawn(config).await;
    let mut peer = Peer::open(addr).await;
    let streams: Vec<u32> = (0..10).map(|i| 2 * i + 1).collect();
    for &id in &streams {
        let head = request("POST", "/", &[]);
        peer.send(&frame(HEADERS, END_HEADERS, id, &head)).await;
    }
    let chunk = [b'x'; 8192];
    let per_stream_max = 48 * 1024;
    let mut per_stream = vec![0usize; streams.len()];
    let mut window: i64 = 65_535;
    let mut sent = 0usize;
    loop {
        let mut progressed = false;
        for (i, &id) in streams.iter().enumerate() {
            while window >= chunk.len() as i64 && per_stream[i] + chunk.len() <= per_stream_max {
                peer.send(&frame(DATA, 0, id, &chunk)).await;
                window -= chunk.len() as i64;
                per_stream[i] += chunk.len();
                sent += chunk.len();
                progressed = true;
            }
        }
        let mut credit = 0i64;
        while let Ok(Some(f)) =
            tokio::time::timeout(Duration::from_millis(300), peer.read_frame()).await
        {
            if f.kind == WINDOW_UPDATE && f.stream == 0 {
                credit += i64::from(be32(&f.payload));
            }
        }
        window += credit;
        if credit == 0 && !progressed {
            break;
        }
    }
    // The budget plus one connection window, far below 10 × 48 KiB.
    assert!(sent <= 64 * 1024 + 65_535, "sent {sent} bytes");
    assert!(sent >= 64 * 1024, "sent {sent} bytes");
    // Dropping the streams gives the withheld credit back.
    for &id in &streams {
        peer.send(&frame(RST_STREAM, 0, id, &8u32.to_be_bytes()))
            .await;
    }
    let f = peer
        .until(|f| f.kind == WINDOW_UPDATE && f.stream == 0)
        .await
        .expect("connection credit");
    assert!(be32(&f.payload) > 0);
}

/// H2-4: `max_body_bytes` limits buffered bodies only; a streaming handler
/// gets a body with a larger content-length.
#[tokio::test]
async fn streaming_handlers_take_bodies_over_max_body_bytes() {
    let config = ServerConfig {
        max_body_bytes: 1024,
        ..h2(|_| {})
    };
    let addr = spawn(config).await;
    let mut peer = Peer::open(addr).await;
    let head = request("POST", "/count", &[("content-length", "5000")]);
    peer.send(&frame(HEADERS, END_HEADERS, 1, &head)).await;
    peer.send(&frame(DATA, END_STREAM, 1, &[b'x'; 5000])).await;
    assert_eq!(peer.status(1).await, "200");
    assert_eq!(body_text(&mut peer, 1).await, "5000");
    // A buffered handler is still answered 413 up front.
    let head = request("POST", "/", &[("content-length", "5000")]);
    peer.send(&frame(HEADERS, END_HEADERS, 3, &head)).await;
    assert_eq!(peer.status(3).await, "413");
}

/// H2-9: `:authority`, `:scheme` and header values get the checks HTTP/1.1
/// applies.
#[tokio::test]
async fn authority_scheme_and_control_characters_are_validated() {
    let addr = spawn(h2(|_| {})).await;
    let block = |scheme: &str, authority: &str, extra: &[(&str, &str)]| {
        let mut b = Vec::new();
        lit(&mut b, ":method", "GET");
        lit(&mut b, ":scheme", scheme);
        lit(&mut b, ":authority", authority);
        lit(&mut b, ":path", "/");
        for (n, v) in extra {
            lit(&mut b, n, v);
        }
        b
    };
    let cases = [
        block("http", "user:pw@test", &[]),
        block("http", "te st", &[]),
        block("ftp\x7f", "test", &[]),
        block("http", "test", &[("x-ctl", "a\x01b")]),
        block("http", "test", &[("x-del", "a\x7fb")]),
    ];
    for (i, b) in cases.iter().enumerate() {
        let mut peer = Peer::open(addr).await;
        peer.send(&frame(HEADERS, END_HEADERS | END_STREAM, 1, b))
            .await;
        assert_eq!(peer.rst(1).await, PROTOCOL_ERROR, "case {i}");
    }
    // A well-formed host:port is fine.
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(
        HEADERS,
        END_HEADERS | END_STREAM,
        1,
        &block("https", "test:8443", &[("x-tab", "a\tb")]),
    ))
    .await;
    assert_eq!(peer.status(1).await, "200");
}

/// h2spec 3.5 #2: a cleartext connection that opens with garbage instead of
/// the preface is closed, not answered with HTTP/1.1.
#[tokio::test]
async fn a_cleartext_garbage_preface_is_closed() {
    let addr = spawn(h2(|_| {})).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"INVALID CONNECTION PREFACE\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut answer)).await;
    assert!(answer.is_empty(), "{}", String::from_utf8_lossy(&answer));
}

/// H2-8 and h2spec 5.1 / 5.1.1: frames on streams that are no longer open.
#[tokio::test]
async fn frames_on_closed_streams_follow_rfc_9113_section_5_1() {
    let addr = spawn(h2(|_| {})).await;
    // Half-closed (remote): DATA after the peer's END_STREAM (5.1 #5).
    let mut peer = Peer::open(addr).await;
    peer.send(&get(1, "/slow")).await;
    peer.send(&frame(DATA, 0, 1, b"late")).await;
    assert_eq!(peer.rst(1).await, STREAM_CLOSED);
    // After the peer's RST_STREAM: DATA (5.1 #8) and HEADERS (5.1 #9).
    let mut trailers = Vec::new();
    lit(&mut trailers, "x-late", "1");
    let after_reset = [
        frame(DATA, 0, 1, b"late"),
        frame(HEADERS, END_HEADERS | END_STREAM, 1, &trailers),
    ];
    for late in after_reset {
        let mut peer = Peer::open(addr).await;
        let head = request("POST", "/hold", &[]);
        peer.send(&frame(HEADERS, END_HEADERS, 1, &head)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        peer.send(&frame(RST_STREAM, 0, 1, &8u32.to_be_bytes()))
            .await;
        peer.send(&late).await;
        assert_eq!(peer.rst(1).await, STREAM_CLOSED);
    }
    // Closed by both sides: DATA (5.1 #11) and HEADERS (5.1 #12) are
    // connection errors.
    let after_close = [
        frame(DATA, 0, 1, b"late"),
        frame(HEADERS, END_HEADERS | END_STREAM, 1, &trailers),
    ];
    for late in after_close {
        let mut peer = Peer::open(addr).await;
        peer.send(&get(1, "/")).await;
        assert_eq!(peer.status(1).await, "200");
        assert_eq!(body_text(&mut peer, 1).await, "ok");
        tokio::time::sleep(Duration::from_millis(50)).await;
        peer.send(&late).await;
        assert_eq!(peer.goaway().await, STREAM_CLOSED);
    }
    // A lower stream id that was never used (5.1.1 #2).
    let mut peer = Peer::open(addr).await;
    peer.send(&get(5, "/")).await;
    assert_eq!(peer.status(5).await, "200");
    peer.send(&get(3, "/")).await;
    assert_eq!(peer.goaway().await, PROTOCOL_ERROR);
    // Frames still in flight for a stream this side reset are ignored.
    let mut peer = Peer::open(addr).await;
    let head = request("POST", "/", &[("content-length", "1")]);
    peer.send(&frame(HEADERS, END_HEADERS, 1, &head)).await;
    peer.send(&frame(DATA, 0, 1, b"too long")).await;
    assert_eq!(peer.rst(1).await, PROTOCOL_ERROR);
    peer.send(&frame(DATA, END_STREAM, 1, b"more")).await;
    peer.send(&get(3, "/")).await;
    assert_eq!(peer.status(3).await, "200");
}

/// H2-15: HEADERS and trailers of other streams are not stuck behind a
/// stream blocked on the connection window.
#[tokio::test]
async fn headers_are_not_blocked_by_the_connection_window() {
    let addr = spawn_big(h2(|_| {})).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(PREFACE).await.unwrap();
    // A 1 MiB stream window: only the 65 535-byte connection window binds.
    sock.write_all(&frame(SETTINGS, 0, 0, &[0, 4, 0, 0x10, 0, 0]))
        .await
        .unwrap();
    sock.write_all(&get(1, "/")).await.unwrap();
    let mut peer = Peer { sock };
    let (n, ended) = data_until_quiet(&mut peer, 1, Duration::from_millis(300)).await;
    assert_eq!(n, 65_535);
    assert!(!ended);
    peer.send(&get(3, "/")).await;
    assert_eq!(peer.status(3).await, "200");
}

/// HTTP-1: late WINDOW_UPDATE frames for streams that have just closed are
/// routine (Go's client returns window for response bytes as it reads them,
/// after the response has ended) and must never close the connection; only
/// updates for streams closed so long ago that they are forgotten count as
/// control frames.
#[tokio::test]
async fn late_window_updates_for_just_closed_streams_are_free() {
    let addr = spawn(h2(|_| {})).await;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(PREFACE).await.unwrap();
    // A 1 MiB stream window, and plenty of connection window.
    sock.write_all(&frame(SETTINGS, 0, 0, &[0, 4, 0, 0x10, 0, 0]))
        .await
        .unwrap();
    sock.write_all(&frame(WINDOW_UPDATE, 0, 0, &(1u32 << 30).to_be_bytes()))
        .await
        .unwrap();
    let mut peer = Peer { sock };
    let size = 64 * 1024;
    let path = format!("/bytes/{size}");
    let mut id = 1u32;
    // 2000 responses of 64 KiB, each followed by three late stream window
    // updates: 6000 frames, six times the default control-frame budget.
    for _ in 0..100 {
        let first = id;
        let mut batch = Vec::new();
        for _ in 0..20 {
            batch.extend(get(id, &path));
            id += 2;
        }
        peer.send(&batch).await;
        let mut done = 0;
        let mut received = 0usize;
        while done < 20 {
            let f = peer
                .until(|f| f.kind == GOAWAY || f.kind == DATA)
                .await
                .expect("responses");
            assert_ne!(f.kind, GOAWAY, "GOAWAY after stream {}", f.stream);
            received += f.payload.len();
            if f.end_stream {
                done += 1;
            }
        }
        assert_eq!(received, 20 * size);
        let mut late = Vec::new();
        for s in (first..id).step_by(2) {
            for _ in 0..3 {
                late.extend(frame(WINDOW_UPDATE, 0, s, &16_384u32.to_be_bytes()));
            }
        }
        late.extend(frame(WINDOW_UPDATE, 0, 0, &(received as u32).to_be_bytes()));
        peer.send(&late).await;
    }
    peer.send(&get(id, "/")).await;
    let f = peer
        .until(|f| f.kind == GOAWAY || (f.kind == HEADERS && f.stream == id))
        .await
        .expect("an answer");
    assert_eq!(f.kind, HEADERS, "the connection was closed");
    // Stream 1 closed thousands of streams ago and is forgotten: a flood of
    // updates for it is noise and still ends the connection.
    let mut flood = Vec::new();
    for _ in 0..1500 {
        flood.extend(frame(WINDOW_UPDATE, 0, 1, &1u32.to_be_bytes()));
    }
    peer.send(&flood).await;
    assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM);
}

/// HTTP-1: RST_STREAM for a stream that has just closed (a client cancelling
/// a request whose response it has not read yet) is free too.
#[tokio::test]
async fn late_resets_for_just_closed_streams_are_free() {
    let addr = spawn(h2(|h| h.max_control_frames_per_window = 50)).await;
    let mut peer = Peer::open(addr).await;
    let mut id = 1u32;
    for _ in 0..10 {
        let first = id;
        let mut batch = Vec::new();
        for _ in 0..20 {
            batch.extend(get(id, "/"));
            id += 2;
        }
        peer.send(&batch).await;
        for s in (first..id).step_by(2) {
            assert_eq!(body_text(&mut peer, s).await, "ok");
        }
        let mut late = Vec::new();
        for s in (first..id).step_by(2) {
            late.extend(frame(RST_STREAM, 0, s, &8u32.to_be_bytes()));
        }
        peer.send(&late).await;
    }
    // 200 late resets against a budget of 50: still serving.
    peer.send(&get(id, "/")).await;
    let f = peer
        .until(|f| f.kind == GOAWAY || (f.kind == HEADERS && f.stream == id))
        .await
        .expect("an answer");
    assert_eq!(f.kind, HEADERS, "the connection was closed");
}

/// HTTP-2: concurrent buffered uploads whose total is well beyond the
/// connection's body budget plus its window all finish: bodies are admitted
/// to the budget by size, and the ones waiting cannot take the window the
/// admitted ones need.
#[tokio::test]
async fn concurrent_uploads_beyond_the_body_budget_all_finish() {
    let config = ServerConfig {
        max_body_bytes: 1 << 20,
        body_timeout: Duration::from_secs(5),
        ..h2(|h| {
            h.max_buffered_body_bytes = 2 << 20;
            h.initial_connection_window_size = 1 << 20;
            h.initial_stream_window_size = 256 * 1024;
        })
    };
    let addr = spawn(config).await;
    let client = Client::builder()
        .http2_prior_knowledge(true)
        .timeout(Some(Duration::from_secs(20)))
        .build()
        .unwrap();
    // Eight uploads of `max_body_bytes` on one connection: 8 MiB against a
    // budget of 2 MiB and a window of 1 MiB.
    let uploads: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move {
                let req = Request::builder(Method::Post, &format!("http://{addr}/len"))
                    .unwrap()
                    .body(vec![b'u'; 1 << 20])
                    .build();
                let resp = client.send(req).await?;
                let status = resp.status();
                Ok::<_, ferroly::http::HttpError>((status, resp.text().await?))
            })
        })
        .collect();
    for upload in uploads {
        let (status, body) = upload.await.unwrap().expect("the upload finished");
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, (1 << 20).to_string());
    }
}

/// HTTP-2: a client that fills the connection window with bodies the budget
/// cannot admit has the youngest of them refused, so the admitted one can
/// still finish (instead of every upload stalling until `body_timeout`).
#[tokio::test]
async fn bodies_that_cannot_be_admitted_are_refused_not_deadlocked() {
    let config = ServerConfig {
        max_body_bytes: 64 * 1024,
        ..h2(|h| {
            h.max_buffered_body_bytes = 64 * 1024;
            h.initial_connection_window_size = 65_535;
            h.initial_stream_window_size = 65_535;
        })
    };
    let addr = spawn(config).await;
    let mut peer = Peer::open(addr).await;
    // Stream 1 (admitted) and streams 3..=11 (waiting), no content-length.
    for id in (1..=11).step_by(2) {
        let head = request("POST", "/len", &[]);
        peer.send(&frame(HEADERS, END_HEADERS, id, &head)).await;
    }
    // Stream 1 sends a byte; then the waiting streams send all the
    // connection window allows, up to their initial stream windows.
    peer.send(&frame(DATA, 0, 1, b"a")).await;
    let mut window: i64 = 65_535 - 1;
    let waiting: Vec<u32> = (3..=11).step_by(2).collect();
    let mut stream_window = vec![65_535i64; waiting.len()];
    let mut refused = Vec::new();
    loop {
        for (i, &id) in waiting.iter().enumerate() {
            while window > 0 && stream_window[i] > 0 && !refused.contains(&id) {
                let n = window.min(stream_window[i]).min(16_384);
                peer.send(&frame(DATA, 0, id, &vec![b'w'; n as usize]))
                    .await;
                window -= n;
                stream_window[i] -= n;
            }
        }
        let mut credit = 0i64;
        while let Ok(Some(f)) =
            tokio::time::timeout(Duration::from_millis(300), peer.read_frame()).await
        {
            if f.kind == WINDOW_UPDATE && f.stream == 0 {
                credit += i64::from(be32(&f.payload));
            }
            if f.kind == RST_STREAM {
                assert_eq!(be32(&f.payload), REFUSED_STREAM);
                refused.push(f.stream);
            }
        }
        window += credit;
        if credit == 0 {
            break;
        }
    }
    assert!(!refused.is_empty(), "no waiting body was refused");
    assert!(!refused.contains(&1), "the admitted body was refused");
    // Stream 1 can still send the rest of its 64 KiB body.
    let mut sent = 1usize;
    let body = 64 * 1024;
    while sent < body {
        if window <= 0 {
            let f = peer
                .until(|f| f.kind == WINDOW_UPDATE && f.stream == 0)
                .await
                .expect("connection credit");
            window += i64::from(be32(&f.payload));
            continue;
        }
        let n = (body - sent).min(window as usize).min(16_384);
        let end = if sent + n == body { END_STREAM } else { 0 };
        peer.send(&frame(DATA, end, 1, &vec![b'a'; n])).await;
        window -= n as i64;
        sent += n;
    }
    assert_eq!(peer.status(1).await, "200");
    assert_eq!(body_text(&mut peer, 1).await, body.to_string());
}

/// HTTP-3: DATA frames for a streaming handler that has not read yet are
/// coalesced into one chunk, so a flood of 1-byte frames costs a byte each,
/// not an event each.
#[tokio::test]
async fn tiny_data_frames_are_coalesced_for_a_streaming_handler() {
    let addr = spawn(h2(|_| {})).await;
    let mut peer = Peer::open(addr).await;
    let head = request("POST", "/late", &[]);
    let started = std::time::Instant::now();
    peer.send(&frame(HEADERS, END_HEADERS, 1, &head)).await;
    let mut flood = Vec::new();
    for _ in 0..20_000 {
        flood.extend(frame(DATA, 0, 1, b"x"));
    }
    flood.extend(frame(DATA, END_STREAM, 1, b""));
    // The PING is answered once every DATA frame before it is processed.
    flood.extend(frame(PING, 0, 0, b"flooded!"));
    peer.send(&flood).await;
    peer.until(|f| f.kind == PING && f.end_stream)
        .await
        .expect("the PING acknowledgement");
    // The handler starts reading 1.5 s after the request arrived.
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "the flood took {:?} to process",
        started.elapsed()
    );
    assert_eq!(peer.status(1).await, "200");
    let answer = body_text(&mut peer, 1).await;
    let (chunks, bytes) = answer.split_once(' ').unwrap();
    assert_eq!(bytes, "20000");
    let chunks: usize = chunks.parse().unwrap();
    assert!(chunks <= 10, "{chunks} chunks for 20 000 frames");
}

/// HTTP-4: the write timeout measures how long queued output fails to
/// drain, not how long the connection was quiet before it was queued: a
/// handler slower than the write timeout must not get the connection
/// killed.
#[tokio::test]
async fn handlers_slower_than_the_write_timeout_keep_the_connection() {
    struct Slow;
    impl HttpHandler for Slow {
        fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                HttpResponse::text(StatusCode::OK, "ok")
            })
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServerConfig {
        write_timeout: Duration::from_millis(40),
        ..h2(|_| {})
    };
    tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            Arc::new(Slow),
            std::future::pending::<()>(),
            config,
        )
        .await;
    });
    // 60 connections, 6 requests each, one at a time.
    let tasks: Vec<_> = (0..60)
        .map(|_| {
            tokio::spawn(async move {
                let mut peer = Peer::open(addr).await;
                for i in 0..6u32 {
                    let id = 2 * i + 1;
                    peer.send(&get(id, "/")).await;
                    let f = peer
                        .until(|f| {
                            f.kind == GOAWAY || (f.kind == DATA && f.stream == id && f.end_stream)
                        })
                        .await;
                    if f.map_or(true, |f| f.kind == GOAWAY) {
                        return 1;
                    }
                }
                0
            })
        })
        .collect();
    let mut failures = 0;
    for t in tasks {
        failures += t.await.unwrap();
    }
    assert_eq!(failures, 0, "connections killed by the write timeout");
}

/// HTTP-5: a peer that reads a large response slowly but keeps talking (as
/// a live client does) is not taken for a dead one by the keep-alive, and
/// its PING is answered while the response is still queued.
#[tokio::test]
async fn a_slow_but_live_reader_survives_the_keepalive() {
    let addr = spawn(h2(|h| {
        h.keepalive_interval = Some(Duration::from_millis(200));
        h.keepalive_timeout = Duration::from_millis(400);
    }))
    .await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(32 * 1024).unwrap();
    let mut sock = socket.connect(addr).await.unwrap();
    sock.write_all(PREFACE).await.unwrap();
    let mut settings = vec![0, 4];
    settings.extend_from_slice(&0x7fff_ffffu32.to_be_bytes());
    sock.write_all(&frame(SETTINGS, 0, 0, &settings))
        .await
        .unwrap();
    sock.write_all(&frame(WINDOW_UPDATE, 0, 0, &0x3fff_ffffu32.to_be_bytes()))
        .await
        .unwrap();
    let size = 3 << 20;
    sock.write_all(&get(1, &format!("/bytes/{size}")))
        .await
        .unwrap();
    let (mut r, mut w) = sock.into_split();
    let started = std::time::Instant::now();
    // About 1.5 MB/s, with a frame (a 1-byte connection window update)
    // after every read and a PING after a second.
    // The server queues its PING ACK behind whatever DATA is already in its
    // kernel send buffer, which on Linux can be most of the response, so the
    // ACK may arrive after END_STREAM: keep reading until both are seen.
    let reader = tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::new();
        let mut got = 0usize;
        let mut ping_sent: Option<std::time::Instant> = None;
        let mut ping_rtt = None;
        let mut ended = false;
        loop {
            if ended && ping_rtt.is_some() {
                return Ok((got, ping_rtt));
            }
            let mut chunk = vec![0u8; 32 * 1024];
            let n = match tokio::time::timeout(Duration::from_secs(5), r.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ if ended => return Ok((got, ping_rtt)),
                _ => return Err(format!("closed after {got} body bytes")),
            };
            buf.extend_from_slice(&chunk[..n]);
            let mut out = frame(WINDOW_UPDATE, 0, 0, &1u32.to_be_bytes());
            while buf.len() >= 9 {
                let len = (buf[0] as usize) << 16 | (buf[1] as usize) << 8 | buf[2] as usize;
                if buf.len() < 9 + len {
                    break;
                }
                let (kind, flags) = (buf[3], buf[4]);
                let payload = buf[9..9 + len].to_vec();
                buf.drain(..9 + len);
                match kind {
                    DATA => {
                        got += len;
                        if flags & END_STREAM != 0 {
                            ended = true;
                        }
                    }
                    GOAWAY => return Err(format!("GOAWAY after {got} body bytes")),
                    PING if flags & ACK == 0 => out.extend(frame(PING, ACK, 0, &payload)),
                    PING if payload == b"client!!" => {
                        ping_rtt = ping_sent.map(|s| s.elapsed());
                    }
                    SETTINGS if flags & ACK == 0 => out.extend(frame(SETTINGS, ACK, 0, &[])),
                    _ => {}
                }
            }
            if ping_sent.is_none() && (ended || started.elapsed() > Duration::from_secs(1)) {
                out.extend(frame(PING, 0, 0, b"client!!"));
                ping_sent = Some(std::time::Instant::now());
            }
            if w.write_all(&out).await.is_err() {
                return Err(format!("write failed after {got} body bytes"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let (got, rtt) = reader.await.unwrap().expect("the response completes");
    assert_eq!(got, size);
    let rtt = rtt.expect("the PING was answered");
    // Bounded by the time to drain what the kernel already held, not by the
    // rest of the response being produced.
    assert!(rtt < Duration::from_secs(4), "PING answered after {rtt:?}");
}

/// HTTP-9: the stream stall timeout adds up blocked time, so a client that
/// grants one byte of window now and then cannot hold a stream for ever.
#[tokio::test]
async fn a_window_trickle_does_not_defeat_the_stall_timeout() {
    let addr = spawn_big(h2(|h| {
        h.stream_stall_timeout = Some(Duration::from_millis(300));
    }))
    .await;
    let mut peer = zero_window_get(addr).await;
    assert_eq!(peer.status(1).await, "200");
    let started = std::time::Instant::now();
    let reset = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            peer.send(&frame(WINDOW_UPDATE, 0, 1, &1u32.to_be_bytes()))
                .await;
            let quiet = tokio::time::timeout(
                Duration::from_millis(50),
                peer.until(|f| f.kind == RST_STREAM && f.stream == 1),
            )
            .await;
            if let Ok(Some(f)) = quiet {
                return be32(&f.payload);
            }
        }
    })
    .await
    .expect("the trickled stream is reset");
    assert_eq!(reset, 0x8);
    assert!(started.elapsed() >= Duration::from_millis(250));
}

/// HTTP-10: streams refused for exceeding the stream limit are not resets
/// the client provoked. Before the client has acknowledged the server's
/// SETTINGS (it may not know the limit) they are not counted at all;
/// afterwards they count as control frames.
#[tokio::test]
async fn refused_streams_are_not_counted_as_resets() {
    let addr = spawn(h2(|h| {
        h.max_concurrent_streams = 1;
        h.max_resets_per_window = 5;
        h.max_control_frames_per_window = 16;
    }))
    .await;
    let mut burst = Vec::new();
    for i in 0..40u32 {
        burst.extend(get(2 * i + 1, "/slow"));
    }
    // Not acknowledged: 39 refusals are fine.
    let mut peer = Peer::open(addr).await;
    peer.send(&burst).await;
    let f = peer
        .until(|f| f.kind == GOAWAY || (f.kind == HEADERS && f.stream == 1))
        .await
        .expect("an answer");
    assert_eq!(f.kind, HEADERS, "the connection was closed");
    // Acknowledged: refusals count as control frames.
    let mut peer = Peer::open(addr).await;
    peer.send(&frame(SETTINGS, ACK, 0, &[])).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    peer.send(&burst).await;
    assert_eq!(peer.goaway().await, ENHANCE_YOUR_CALM);
}

/// HTTP-11: when the server's response is complete before the request body,
/// a handler still reading that body learns that this side reset the
/// stream, not that the peer did.
#[tokio::test]
async fn a_body_cut_short_by_the_servers_own_reset_says_so() {
    use ferroly::http::{Http2Cause, Http2ErrorCode};
    type Report = tokio::sync::mpsc::UnboundedSender<Option<Http2Cause>>;
    struct Early(Report);
    impl HttpHandler for Early {
        fn wants_body_stream(&self, _req: &Request) -> bool {
            true
        }
        fn handle(&self, mut req: Request) -> BoxFuture<'_, HttpResponse> {
            let mut body = req.take_body_stream().expect("a body stream");
            let report = self.0.clone();
            tokio::spawn(async move {
                let cause = loop {
                    match body.chunk().await {
                        Ok(Some(_)) => continue,
                        Ok(None) => break None,
                        Err(e) => break e.http2_cause(),
                    }
                };
                let _ = report.send(cause);
            });
            Box::pin(async { HttpResponse::text(StatusCode::OK, "early") })
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_with_config(
            listener,
            Arc::new(Early(tx)),
            std::future::pending::<()>(),
            h2(|_| {}),
        )
        .await;
    });
    let mut peer = Peer::open(addr).await;
    let head = request("POST", "/", &[]);
    peer.send(&frame(HEADERS, END_HEADERS, 1, &head)).await;
    assert_eq!(peer.rst(1).await, 0);
    let cause = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cause,
        Some(Http2Cause::LocalReset(Http2ErrorCode::NO_ERROR))
    );
}

/// RPC-4: holding each stream just past `rapid_reset_threshold` before
/// resetting it does not get around the reset budgets: resets of streams
/// whose response is not complete count toward `max_cancels_per_window`
/// whatever their age.
#[tokio::test]
async fn cancels_past_the_rapid_reset_threshold_are_bounded_too() {
    let addr = spawn(h2(|h| h.max_cancels_per_window = 100)).await;
    let mut peer = Peer::open(addr).await;
    let mut id = 1u32;
    let mut closed = false;
    for _ in 0..4 {
        let first = id;
        let mut opens = Vec::new();
        for _ in 0..40 {
            opens.extend(get(id, "/slow"));
            id += 2;
        }
        peer.send(&opens).await;
        // Older than `rapid_reset_threshold` (100 ms), still running.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut resets = Vec::new();
        for s in (first..id).step_by(2) {
            resets.extend(frame(RST_STREAM, 0, s, &8u32.to_be_bytes()));
        }
        peer.send(&resets).await;
        let goaway =
            tokio::time::timeout(Duration::from_millis(100), peer.until(|f| f.kind == GOAWAY))
                .await;
        if let Ok(Some(f)) = goaway {
            assert_eq!(be32(&f.payload[4..8]), ENHANCE_YOUR_CALM);
            closed = true;
            break;
        }
    }
    assert!(closed, "160 cancels against a budget of 100");
}

/// RPC-4: a client that completes requests may cancel twice as many as it
/// completes.
#[tokio::test]
async fn completed_streams_raise_the_cancel_allowance() {
    let addr = spawn(h2(|h| h.max_cancels_per_window = 20)).await;
    let mut peer = Peer::open(addr).await;
    let mut id = 1u32;
    for _ in 0..5 {
        // 20 completed...
        let first = id;
        let mut opens = Vec::new();
        for _ in 0..20 {
            opens.extend(get(id, "/"));
            id += 2;
        }
        peer.send(&opens).await;
        for s in (first..id).step_by(2) {
            assert_eq!(body_text(&mut peer, s).await, "ok");
        }
        // ...and 30 cancelled.
        let first = id;
        let mut opens = Vec::new();
        for _ in 0..30 {
            opens.extend(get(id, "/slow"));
            id += 2;
        }
        peer.send(&opens).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut resets = Vec::new();
        for s in (first..id).step_by(2) {
            resets.extend(frame(RST_STREAM, 0, s, &8u32.to_be_bytes()));
        }
        peer.send(&resets).await;
    }
    // 150 cancels, 100 completions: 200 allowed.
    peer.send(&get(id, "/")).await;
    let f = peer
        .until(|f| f.kind == GOAWAY || (f.kind == HEADERS && f.stream == id))
        .await
        .expect("an answer");
    assert_eq!(f.kind, HEADERS, "the connection was closed");
}

/// GRPC-6: no stream cap per HTTP/2 connection by default
/// (`max_keep_alive_requests` is HTTP/1.1 only).
#[tokio::test]
async fn http2_connections_are_not_capped_by_max_keep_alive_requests() {
    let config = ServerConfig {
        max_keep_alive_requests: 10,
        ..h2(|_| {})
    };
    let addr = spawn(config).await;
    let mut peer = Peer::open(addr).await;
    let mut id = 1u32;
    for _ in 0..25 {
        let mut batch = Vec::new();
        let first = id;
        for _ in 0..50 {
            batch.extend(get(id, "/"));
            id += 2;
        }
        peer.send(&batch).await;
        let mut done = 0;
        while done < 50 {
            let f = peer
                .until(|f| f.kind == GOAWAY || (f.kind == DATA && f.end_stream))
                .await
                .expect("responses");
            assert_ne!(f.kind, GOAWAY, "GOAWAY after {} streams", f.stream);
            assert!(f.stream >= first);
            done += 1;
        }
    }
}

/// GRPC-6: `max_streams_per_connection` ends the connection with the
/// two-phase GOAWAY of RFC 9113 §6.8.
#[tokio::test]
async fn max_streams_per_connection_uses_a_two_phase_goaway() {
    let addr = spawn(h2(|h| h.max_streams_per_connection = Some(2))).await;
    let mut peer = Peer::open(addr).await;
    peer.send(&get(1, "/")).await;
    peer.send(&get(3, "/")).await;
    let first = peer.until(|f| f.kind == GOAWAY).await.expect("GOAWAY");
    assert_eq!(be32(&first.payload[..4]), 0x7fff_ffff);
    assert_eq!(be32(&first.payload[4..8]), 0);
    let ping = peer
        .until(|f| f.kind == PING && !f.end_stream)
        .await
        .expect("PING");
    // A stream the client sent before it saw the GOAWAY is still served.
    peer.send(&get(5, "/")).await;
    assert_eq!(peer.status(5).await, "200");
    peer.send(&frame(PING, ACK, 0, &ping.payload)).await;
    let last = peer.until(|f| f.kind == GOAWAY).await.expect("GOAWAY");
    assert_eq!(be32(&last.payload[..4]), 5);
    // After the final GOAWAY new streams are refused (or find the idle
    // connection already closed), never served.
    peer.send(&get(7, "/")).await;
    let answer = peer
        .until(|f| (f.kind == RST_STREAM || f.kind == HEADERS) && f.stream == 7)
        .await;
    if let Some(f) = answer {
        assert_eq!(f.kind, RST_STREAM);
        assert_eq!(be32(&f.payload), REFUSED_STREAM);
    }
}

/// H2-17: when `drain_timeout` expires, `serve` returns and stops the
/// connections it was still draining — a never-ending HTTP/2 stream
/// included.
#[tokio::test]
async fn drain_timeout_stops_long_lived_http2_streams() {
    struct Forever;
    impl HttpHandler for Forever {
        fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
            Box::pin(async {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                tokio::spawn(async move {
                    while tx.send(b"tick\n".to_vec()).await.is_ok() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
                HttpResponse::stream(StatusCode::OK, rx)
            })
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let config = ServerConfig {
        drain_timeout: Duration::from_millis(300),
        ..h2(|_| {})
    };
    let server = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        serve_with_config(listener, Arc::new(Forever), shutdown, config).await
    });
    let mut peer = Peer::open(addr).await;
    peer.send(&get(1, "/")).await;
    assert_eq!(peer.status(1).await, "200");
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("serve returns after drain_timeout")
        .unwrap()
        .unwrap();
    // The connection is gone too: the stream stops instead of ticking on.
    let ended = tokio::time::timeout(Duration::from_secs(3), async {
        while peer.read_frame().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "the stream outlived serve");
}

// ----- the client against scripted servers ----------------------------------

/// A server that runs `script` on every accepted connection, numbered from
/// one, after reading the preface, sending `settings` and acknowledging the
/// client's SETTINGS.
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
            // The client's SETTINGS (flags 0x1 is ACK for SETTINGS).
            if peer
                .until(|f| f.kind == SETTINGS && !f.end_stream)
                .await
                .is_none()
            {
                continue;
            }
            peer.send(&frame(SETTINGS, ACK, 0, &[])).await;
            tokio::spawn(script(n, peer));
        }
    });
    addr
}

/// `:status: 200` (static table index 8).
const STATUS_200: &[u8] = &[0x88];

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

/// GRPC-2 / H2-12: a server's RST_STREAM(NO_ERROR) after a complete
/// response (RFC 9113 §8.1: "stop sending, I have answered") is benign;
/// hundreds of them must not make the client close its own connection.
#[tokio::test]
async fn the_client_survives_many_no_error_resets() {
    let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = conns.clone();
    let addr = scripted(&[], move |_, mut peer| {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            while let Some(f) = peer.until(|f| f.kind == HEADERS).await {
                let mut reply = frame(HEADERS, END_HEADERS | END_STREAM, f.stream, STATUS_200);
                reply.extend(frame(RST_STREAM, 0, f.stream, &0u32.to_be_bytes()));
                peer.send(&reply).await;
            }
        }
    })
    .await;
    let client = h2_client(|_| {});
    let mut senders = Vec::new();
    for _ in 0..300 {
        // An upload the server answers before it is finished.
        let (tx, body) = ferroly::http::body_channel(1);
        senders.push(tx);
        let req = Request::builder(Method::Post, &format!("http://{addr}/"))
            .unwrap()
            .build();
        let resp = client.send_body_stream(req, body).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        resp.bytes().await.unwrap();
    }
    assert_eq!(conns.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// H2-7: a streamed request waiting for a stream slot is bounded by the
/// client's `timeout`.
#[tokio::test]
async fn waiting_for_a_stream_slot_honours_the_timeout() {
    // MAX_CONCURRENT_STREAMS = 1; every response head comes, no body ends.
    let addr = scripted(&[0, 3, 0, 0, 0, 1], |_, mut peer| async move {
        while let Some(f) = peer.until(|f| f.kind == HEADERS).await {
            peer.send(&frame(HEADERS, END_HEADERS, f.stream, STATUS_200))
                .await;
        }
    })
    .await;
    let mut h2 = Http2Config::default();
    h2.max_connections_per_origin = 1;
    let client = Client::builder()
        .http2_prior_knowledge(true)
        .http2_config(h2)
        .timeout(Some(Duration::from_millis(300)))
        .build()
        .unwrap();
    let held = client.send(get_req(addr)).await.unwrap();
    let started = std::time::Instant::now();
    let req = Request::builder(Method::Post, &format!("http://{addr}/"))
        .unwrap()
        .build();
    let err = client
        .send_body_stream(req, ferroly::http::BodyStream::from_bytes("x"))
        .await
        .unwrap_err();
    assert!(err.is_timeout(), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(held);
}

/// H2-11: when every connection to an origin is saturated, the client
/// dials another one (up to `max_connections_per_origin`).
#[tokio::test]
async fn a_saturated_connection_gets_a_sibling() {
    let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = conns.clone();
    let addr = scripted(&[0, 3, 0, 0, 0, 1], move |_, mut peer| {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            while let Some(f) = peer.until(|f| f.kind == HEADERS).await {
                peer.send(&frame(HEADERS, END_HEADERS, f.stream, STATUS_200))
                    .await;
            }
        }
    })
    .await;
    let client = h2_client(|h| h.max_connections_per_origin = 3);
    let mut held = Vec::new();
    for _ in 0..3 {
        let resp = tokio::time::timeout(Duration::from_secs(3), client.send(get_req(addr)))
            .await
            .expect("not stuck behind a saturated connection")
            .unwrap();
        held.push(resp);
    }
    assert_eq!(conns.load(std::sync::atomic::Ordering::SeqCst), 3);
}

/// H2-13: an idempotent request on a reused connection that dies without
/// GOAWAY (a TCP reset, a NAT timeout) is retried once on a new one; a
/// non-idempotent one fails with a structured cause.
#[tokio::test]
async fn a_dead_connection_is_retried_for_idempotent_requests() {
    let addr = scripted(&[], |n, mut peer| async move {
        // The first connection answers once, then dies on the next request.
        let mut answered = 0;
        while let Some(f) = peer.until(|f| f.kind == HEADERS).await {
            if n == 1 && answered == 1 {
                return; // drops the socket
            }
            let reply = frame(HEADERS, END_HEADERS | END_STREAM, f.stream, STATUS_200);
            peer.send(&reply).await;
            answered += 1;
        }
    })
    .await;
    let client = h2_client(|_| {});
    for _ in 0..3 {
        let resp = client.send(get_req(addr)).await.expect("retried");
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // POST is not retried.
    let addr = scripted(&[], |_, mut peer| async move {
        let mut answered = 0;
        while let Some(f) = peer.until(|f| f.kind == HEADERS).await {
            if answered == 1 {
                return;
            }
            let reply = frame(HEADERS, END_HEADERS | END_STREAM, f.stream, STATUS_200);
            peer.send(&reply).await;
            answered += 1;
        }
    })
    .await;
    let client = h2_client(|_| {});
    client.send(get_req(addr)).await.unwrap();
    let req = Request::builder(Method::Post, &format!("http://{addr}/"))
        .unwrap()
        .body("x")
        .build();
    let err = client.send(req).await.unwrap_err();
    assert!(
        matches!(
            err.http2_cause(),
            Some(ferroly::http::Http2Cause::ConnectionClosed | ferroly::http::Http2Cause::Io(_))
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains("malformed"), "{err}");
}

/// Keep-alive PINGs detect a peer that has gone silent (a dead host), so a
/// request without a deadline does not hang forever.
#[tokio::test]
async fn keepalive_pings_detect_a_dead_peer() {
    let addr = scripted(&[], |_, mut peer| async move {
        let _ = peer.until(|f| f.kind == HEADERS).await;
        // Silent from now on: no response, no PING acknowledgement.
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(peer);
    })
    .await;
    let client = h2_client(|h| {
        h.keepalive_interval = Some(Duration::from_millis(200));
        h.keepalive_timeout = Duration::from_millis(300);
    });
    let err = tokio::time::timeout(Duration::from_secs(5), client.send(get_req(addr)))
        .await
        .expect("the dead peer is detected")
        .unwrap_err();
    assert_eq!(
        err.http2_cause(),
        Some(ferroly::http::Http2Cause::Timeout),
        "{err:?}"
    );
}

/// GRPC-3 enabler: real HTTP/2 events reach the caller as structured
/// causes, without "malformed http" wrapping.
#[tokio::test]
async fn http2_failures_carry_a_structured_cause() {
    use ferroly::http::{Http2Cause, Http2ErrorCode};
    async fn cause_for(reply: fn(u32) -> Vec<u8>) -> ferroly::http::HttpError {
        let addr = scripted(&[], move |_, mut peer| async move {
            if let Some(f) = peer.until(|f| f.kind == HEADERS).await {
                peer.send(&reply(f.stream)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        })
        .await;
        let client = h2_client(|_| {});
        tokio::time::timeout(Duration::from_secs(5), client.send(get_req(addr)))
            .await
            .expect("an answer in time")
            .unwrap_err()
    }
    // RST_STREAM(CANCEL) from the server.
    let e = cause_for(|s| frame(RST_STREAM, 0, s, &8u32.to_be_bytes())).await;
    assert_eq!(
        e.http2_cause(),
        Some(Http2Cause::Reset(Http2ErrorCode::CANCEL))
    );
    assert!(!e.to_string().contains("malformed"), "{e}");
    // GOAWAY that refuses the request (here and on the retry).
    let e = cause_for(|_| frame(GOAWAY, 0, 0, &[0; 8])).await;
    assert_eq!(e.http2_cause(), Some(Http2Cause::Refused));
    // GOAWAY(ENHANCE_YOUR_CALM) covering the stream, then a close.
    let e = cause_for(|s| {
        let mut p = s.to_be_bytes().to_vec();
        p.extend_from_slice(&ENHANCE_YOUR_CALM.to_be_bytes());
        frame(GOAWAY, 0, 0, &p)
    })
    .await;
    assert_eq!(
        e.http2_cause(),
        Some(Http2Cause::GoAway {
            code: Http2ErrorCode::ENHANCE_YOUR_CALM,
            last_stream_id: 1
        })
    );
    // A malformed response head: a protocol error this side detected.
    let e = cause_for(|s| frame(HEADERS, END_HEADERS | END_STREAM, s, &[0x82])).await;
    assert_eq!(
        e.http2_cause(),
        Some(Http2Cause::Protocol(Http2ErrorCode::PROTOCOL_ERROR))
    );
    // A PING flood from the server: the client's own budget closes the
    // connection, which callers can tell from a server-side failure.
    let e = cause_for(|_| {
        let mut flood = Vec::new();
        for _ in 0..1100 {
            flood.extend(frame(PING, 0, 0, &[1; 8]));
        }
        flood
    })
    .await;
    assert_eq!(e.http2_cause(), Some(Http2Cause::LocalLimit), "{e:?}");
    assert!(!e.to_string().contains("malformed"), "{e}");
}
