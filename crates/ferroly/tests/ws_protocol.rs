#![cfg(feature = "ws")]
//! RFC 6455 conformance, checked with a raw TCP peer that breaks one rule at
//! a time: frame-level strictness (W2, W5), the server's handshake
//! validation (W5) and the client's validation of the server (W5, W6).

mod ws_common;

use ferroly::ws::{close_code, Message, WsClient, WsError, WsOptions, WsServerConfig};
use tokio::net::TcpListener;
use ws_common::{
    close_body, echo, fast_config, fast_options, http_exchange, start, status_of, upgrade_request,
    within, Raw, FIN, KEY, OP_BINARY, OP_CLOSE, OP_CONT, OP_PING, OP_PONG, OP_TEXT,
};

async fn echo_server() -> ws_common::TestServer {
    start(fast_config(), echo).await
}

/// Sends one offending frame and expects the server to fail the connection
/// with `code`.
async fn expect_server_close(bytes_for: impl FnOnce(&mut Vec<u8>), code: u16) {
    let server = echo_server().await;
    let mut raw = Raw::connect(server.addr).await;
    let mut bytes = Vec::new();
    bytes_for(&mut bytes);
    raw.send_bytes(&bytes).await;
    assert_eq!(raw.read_close().await.0, code);
    raw.expect_eof().await;
    // Close our side too, so the server does not wait for it while draining.
    drop(raw);
    server.stop().await;
}

const MASK: Option<[u8; 4]> = Some([0xA1, 0xB2, 0xC3, 0xD4]);

// ---- W5: frame-level strictness on the server ------------------------------

#[tokio::test]
async fn w5_server_rejects_unmasked_client_frames() {
    expect_server_close(
        |b| b.extend(ws_common::frame(FIN | OP_TEXT, b"hello", None)),
        close_code::PROTOCOL_ERROR,
    )
    .await;
}

#[tokio::test]
async fn w5_server_rejects_reserved_bits() {
    for rsv in [0x40u8, 0x20, 0x10, 0x70] {
        expect_server_close(
            |b| b.extend(ws_common::frame(FIN | rsv | OP_TEXT, b"hello", MASK)),
            close_code::PROTOCOL_ERROR,
        )
        .await;
    }
}

#[tokio::test]
async fn w5_server_rejects_unknown_opcodes() {
    for opcode in [0x3u8, 0x7, 0xB, 0xF] {
        expect_server_close(
            |b| b.extend(ws_common::frame(FIN | opcode, b"", MASK)),
            close_code::PROTOCOL_ERROR,
        )
        .await;
    }
}

#[tokio::test]
async fn w5_invalid_utf8_text_is_rejected_not_replaced() {
    // A lone continuation byte, and an overlong encoding.
    for bad in [&[0x80u8][..], &[0xC0, 0xAF], &[b'o', b'k', 0xFF]] {
        expect_server_close(
            |b| b.extend(ws_common::frame(FIN | OP_TEXT, bad, MASK)),
            close_code::INVALID_PAYLOAD,
        )
        .await;
    }
}

#[tokio::test]
async fn w5_utf8_is_validated_across_fragments() {
    let server = echo_server().await;

    // A multi-byte character split across two fragments is valid.
    let mut raw = Raw::connect(server.addr).await;
    let euro = "€".as_bytes();
    raw.send(OP_TEXT, &euro[..2]).await;
    raw.send(FIN | OP_CONT, &euro[2..]).await;
    assert_eq!(raw.read_text().await, "echo: €");

    // An invalid sequence that only shows in the second fragment.
    raw.send(OP_TEXT, &euro[..1]).await;
    raw.send(FIN | OP_CONT, b"A").await;
    assert_eq!(raw.read_close().await.0, close_code::INVALID_PAYLOAD);

    // A message that ends in the middle of a character.
    let mut raw = Raw::connect(server.addr).await;
    raw.send(OP_TEXT, b"ok").await;
    raw.send(FIN | OP_CONT, &euro[..2]).await;
    assert_eq!(raw.read_close().await.0, close_code::INVALID_PAYLOAD);

    // Binary messages are not subject to UTF-8 validation.
    let mut raw = Raw::connect(server.addr).await;
    raw.send(FIN | OP_BINARY, &[0xFF, 0xFE]).await;
    assert_eq!(raw.read().await, Some((FIN | OP_BINARY, vec![0xFF, 0xFE])));
    server.stop().await;
}

#[tokio::test]
async fn w5_fragmentation_sequencing_is_enforced() {
    // A continuation frame with nothing to continue.
    expect_server_close(
        |b| b.extend(ws_common::frame(FIN | OP_CONT, b"x", MASK)),
        close_code::PROTOCOL_ERROR,
    )
    .await;
    // A new data frame in the middle of a fragmented message.
    expect_server_close(
        |b| {
            b.extend(ws_common::frame(OP_TEXT, b"start", MASK));
            b.extend(ws_common::frame(FIN | OP_TEXT, b"again", MASK));
        },
        close_code::PROTOCOL_ERROR,
    )
    .await;
    expect_server_close(
        |b| {
            b.extend(ws_common::frame(OP_BINARY, b"start", MASK));
            b.extend(ws_common::frame(OP_BINARY, b"again", MASK));
        },
        close_code::PROTOCOL_ERROR,
    )
    .await;
}

#[tokio::test]
async fn w5_control_frames_may_interleave_with_fragments() {
    let server = echo_server().await;
    let mut raw = Raw::connect(server.addr).await;
    raw.send(OP_TEXT, b"frag").await;
    raw.send(FIN | OP_PING, b"mid").await;
    raw.send(FIN | OP_CONT, b"mented").await;
    assert_eq!(raw.read().await, Some((FIN | OP_PONG, b"mid".to_vec())));
    assert_eq!(raw.read_text().await, "echo: fragmented");
    server.stop().await;
}

#[tokio::test]
async fn w5_close_frame_payload_is_validated() {
    // A 1-byte payload cannot hold a status code.
    expect_server_close(
        |b| b.extend(ws_common::frame(FIN | OP_CLOSE, &[0x03], MASK)),
        close_code::PROTOCOL_ERROR,
    )
    .await;
    // Codes that must never appear on the wire.
    for code in [0u16, 999, 1004, 1005, 1006, 1015, 1016, 2999, 5000] {
        expect_server_close(
            |b| {
                b.extend(ws_common::frame(
                    FIN | OP_CLOSE,
                    &close_body(code, ""),
                    MASK,
                ))
            },
            close_code::PROTOCOL_ERROR,
        )
        .await;
    }
    // The reason must be UTF-8.
    expect_server_close(
        |b| {
            let mut body = close_body(1000, "");
            body.extend_from_slice(&[0xFF, 0xFE]);
            b.extend(ws_common::frame(FIN | OP_CLOSE, &body, MASK));
        },
        close_code::INVALID_PAYLOAD,
    )
    .await;
}

#[tokio::test]
async fn w5_close_handshake_echoes_the_status_code() {
    let server = echo_server().await;

    for code in [1000u16, 1001, 3000, 4999] {
        let mut raw = Raw::connect(server.addr).await;
        raw.send(FIN | OP_CLOSE, &close_body(code, "bye")).await;
        assert_eq!(raw.read_close().await.0, code);
        raw.expect_eof().await;
    }

    // An empty close frame is answered with an empty close frame.
    let mut raw = Raw::connect(server.addr).await;
    raw.send(FIN | OP_CLOSE, &[]).await;
    assert_eq!(raw.read().await, Some((FIN | OP_CLOSE, Vec::new())));
    raw.expect_eof().await;
    server.stop().await;
}

#[tokio::test]
async fn w5_frame_and_message_size_caps_close_with_1009() {
    let config = WsServerConfig {
        max_frame_size: 64,
        max_message_size: 100,
        ..fast_config()
    };
    let server = start(config, echo).await;

    // One frame over the frame cap: refused from the header alone (only the
    // header is sent, the payload never is).
    let mut raw = Raw::connect(server.addr).await;
    let oversized = ws_common::frame(FIN | OP_BINARY, &[0u8; 65], MASK);
    raw.send_bytes(&oversized[..2 + 4]).await;
    assert_eq!(raw.read_close().await.0, close_code::MESSAGE_TOO_BIG);

    // A claimed 2^62-byte frame must not be allocated.
    let mut raw = Raw::connect(server.addr).await;
    let mut huge = vec![FIN | OP_BINARY, 0x80 | 127];
    huge.extend_from_slice(&(1u64 << 62).to_be_bytes());
    huge.extend_from_slice(&[0, 0, 0, 0]);
    raw.send_bytes(&huge).await;
    assert_eq!(raw.read_close().await.0, close_code::MESSAGE_TOO_BIG);

    // A 64-bit length with the top bit set is a protocol error.
    let mut raw = Raw::connect(server.addr).await;
    let mut bad = vec![FIN | OP_BINARY, 0x80 | 127];
    bad.extend_from_slice(&(1u64 << 63).to_be_bytes());
    raw.send_bytes(&bad).await;
    assert_eq!(raw.read_close().await.0, close_code::PROTOCOL_ERROR);

    // Fragments that each fit the frame cap but together exceed the message
    // cap.
    let mut raw = Raw::connect(server.addr).await;
    raw.send(OP_BINARY, &[0u8; 60]).await;
    raw.send(FIN | OP_CONT, &[0u8; 60]).await;
    assert_eq!(raw.read_close().await.0, close_code::MESSAGE_TOO_BIG);

    // Exactly at the caps is fine.
    let mut raw = Raw::connect(server.addr).await;
    raw.send(OP_BINARY, &[1u8; 64]).await;
    raw.send(FIN | OP_CONT, &[1u8; 36]).await;
    assert_eq!(raw.read().await, Some((FIN | OP_BINARY, vec![1u8; 100])));
    server.stop().await;
}

// ---- W2: control frames ----------------------------------------------------

#[tokio::test]
async fn w2_control_frames_must_be_final_and_at_most_125_bytes() {
    // The review's probe: a 200-byte non-FIN ping used to be answered.
    expect_server_close(
        |b| b.extend(ws_common::frame(OP_PING, &[0u8; 200], MASK)),
        close_code::PROTOCOL_ERROR,
    )
    .await;
    // Non-FIN alone.
    for opcode in [OP_PING, OP_PONG, OP_CLOSE] {
        expect_server_close(
            |b| b.extend(ws_common::frame(opcode, b"", MASK)),
            close_code::PROTOCOL_ERROR,
        )
        .await;
    }
    // Oversized alone (126 bytes uses the 16-bit length form).
    for opcode in [OP_PING, OP_PONG, OP_CLOSE] {
        expect_server_close(
            |b| b.extend(ws_common::frame(FIN | opcode, &[0u8; 126], MASK)),
            close_code::PROTOCOL_ERROR,
        )
        .await;
    }
}

#[tokio::test]
async fn w2_ping_payload_is_echoed_unchanged_and_unsolicited_pongs_are_ignored() {
    let server = echo_server().await;
    let mut raw = Raw::connect(server.addr).await;

    for payload in [&b""[..], b"x", &[0u8, 255, 1, 254], &[7u8; 125]] {
        raw.send(FIN | OP_PING, payload).await;
        assert_eq!(raw.read().await, Some((FIN | OP_PONG, payload.to_vec())));
    }

    // Unsolicited pongs produce no reply and do not disturb the connection.
    raw.send(FIN | OP_PONG, b"nobody asked").await;
    raw.send(FIN | OP_PONG, b"").await;
    raw.send_text("still here").await;
    assert_eq!(
        raw.read().await,
        Some((FIN | OP_TEXT, b"echo: still here".to_vec()))
    );
    server.stop().await;
}

#[tokio::test]
async fn w2_a_ping_flood_is_answered_with_coalesced_pongs() {
    let server = echo_server().await;
    let mut raw = Raw::connect(server.addr).await;

    // 2000 pings written in one burst, read by nobody until afterwards.
    const PINGS: u32 = 2000;
    let mut burst = Vec::new();
    for n in 0..PINGS {
        burst.extend(ws_common::frame(FIN | OP_PING, &n.to_be_bytes(), MASK));
    }
    burst.extend(ws_common::frame(FIN | OP_TEXT, b"done", MASK));
    raw.send_bytes(&burst).await;

    // Every pong answers a ping that was really sent, in order; pongs for
    // pings that were overtaken may be skipped (RFC 6455 §5.5.3), but the
    // last ping is always answered, and never more pongs than pings.
    let mut pongs = 0u32;
    let mut last = None;
    loop {
        match raw.read().await.expect("frame") {
            (b0, payload) if b0 == FIN | OP_PONG => {
                let n = u32::from_be_bytes(payload.try_into().unwrap());
                if let Some(prev) = last {
                    assert!(n > prev, "pongs out of order");
                }
                last = Some(n);
                pongs += 1;
            }
            (b0, payload) => {
                assert_eq!((b0, payload), (FIN | OP_TEXT, b"echo: done".to_vec()));
                break;
            }
        }
    }
    assert_eq!(last, Some(PINGS - 1));
    assert!(pongs <= PINGS);
    server.stop().await;
}

// ---- W5: the server's handshake validation ---------------------------------

#[tokio::test]
async fn w5_bad_handshakes_get_an_http_error_not_a_101() {
    let server = echo_server().await;
    let host = server.addr.to_string();
    let good = upgrade_request(&host, "/", "");

    // The review's probe: a POST carrying only a key used to get a 101.
    let post = format!("POST / HTTP/1.1\r\nHost: {host}\r\nSec-WebSocket-Key: {KEY}\r\n\r\n");
    let response = http_exchange(server.addr, &post).await;
    assert_eq!(status_of(&response), 405);
    assert!(response.contains("Allow: GET\r\n"));

    let cases: Vec<(String, u16)> = vec![
        // Wrong method with an otherwise perfect upgrade.
        (good.replacen("GET", "POST", 1), 405),
        // HTTP/1.0 cannot upgrade.
        (good.replacen("HTTP/1.1", "HTTP/1.0", 1), 400),
        // A plain GET (no Upgrade header) is told to upgrade.
        (format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"), 426),
        (
            good.replace("Upgrade: websocket\r\n", "Upgrade: h2c\r\n"),
            426,
        ),
        // Connection must contain the upgrade token.
        (
            good.replace("Connection: Upgrade\r\n", "Connection: keep-alive\r\n"),
            400,
        ),
        (good.replace("Connection: Upgrade\r\n", ""), 400),
        // Version must be 13.
        (
            good.replace(
                "Sec-WebSocket-Version: 13\r\n",
                "Sec-WebSocket-Version: 8\r\n",
            ),
            426,
        ),
        (good.replace("Sec-WebSocket-Version: 13\r\n", ""), 426),
        // The key must be 16 bytes of base64.
        (
            good.replace(&format!("Sec-WebSocket-Key: {KEY}\r\n"), ""),
            400,
        ),
        (
            good.replace(KEY, "dG9vIHNob3J0"), // "too short"
            400,
        ),
        (good.replace(KEY, "!!!!IHNhbXBsZSBub25jZQ=="), 400),
        (
            good.replace(
                &format!("Sec-WebSocket-Key: {KEY}\r\n"),
                &format!("Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Key: {KEY}\r\n"),
            ),
            400,
        ),
        // Host is required.
        (good.replace(&format!("Host: {host}\r\n"), ""), 400),
        // An upgrade request has no body.
        (
            good.replace("\r\n\r\n", "\r\nContent-Length: 4\r\n\r\nbody"),
            400,
        ),
        // Malformed HTTP is refused by the strict head parser.
        ("GET  / HTTP/1.1\r\nHost: x\r\n\r\n".to_string(), 400),
        (good.replace("\r\n\r\n", "\r\nBad Header: x\r\n\r\n"), 400),
    ];
    for (request, expected) in cases {
        let response = http_exchange(server.addr, &request).await;
        assert_eq!(status_of(&response), expected, "request: {request:?}");
        assert!(!response.contains("Sec-WebSocket-Accept"), "{response:?}");
        assert!(response.contains("Connection: "), "{response:?}");
    }

    // The 426 for a wrong version names the supported one.
    let v8 = good.replace(
        "Sec-WebSocket-Version: 13\r\n",
        "Sec-WebSocket-Version: 8\r\n",
    );
    let response = http_exchange(server.addr, &v8).await;
    assert!(response.contains("Sec-WebSocket-Version: 13\r\n"));
    // And the 426 for a missing upgrade names the protocol.
    let plain = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
    let response = http_exchange(server.addr, &plain).await;
    assert!(response.contains("Upgrade: websocket\r\n"));

    // Token matching is case-insensitive and list-aware.
    let relaxed = good
        .replace("Upgrade: websocket\r\n", "Upgrade: WebSocket\r\n")
        .replace(
            "Connection: Upgrade\r\n",
            "Connection: keep-alive, upgrade\r\n",
        );
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stream, relaxed.as_bytes())
        .await
        .unwrap();
    let head = ws_common::read_head(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 101 "), "{head:?}");
    drop(stream);
    server.stop().await;
}

#[tokio::test]
async fn w5_oversized_request_heads_are_refused() {
    let config = WsServerConfig {
        max_header_bytes: 512,
        max_line_bytes: 256,
        max_headers: 8,
        ..fast_config()
    };
    let server = start(config, echo).await;
    let host = server.addr.to_string();

    let long_line = upgrade_request(&host, "/", &format!("X-Big: {}\r\n", "a".repeat(400)));
    assert_eq!(
        status_of(&http_exchange(server.addr, &long_line).await),
        431
    );

    let many: String = (0..10).map(|i| format!("X-{i}: v\r\n")).collect();
    let too_many = upgrade_request(&host, "/", &many);
    assert_eq!(status_of(&http_exchange(server.addr, &too_many).await), 431);

    let long_target = upgrade_request(&host, &format!("/{}", "p".repeat(400)), "");
    assert_eq!(
        status_of(&http_exchange(server.addr, &long_target).await),
        414
    );
    server.stop().await;
}

// ---- W5 / W6: the client validates the server ------------------------------

/// Runs `WsClient::dial` against a hand-written response.
async fn dial_against(response: &'static str) -> Result<WsClient, WsError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _request = ws_common::read_head(&mut stream).await;
        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
        // Keep the socket open long enough for the client to decide.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    });
    let options = WsOptions::default()
        .subprotocol("chat")
        .subprotocol("superchat");
    let result = within("dial", WsClient::dial(&format!("ws://{addr}/"), options)).await;
    serving.abort();
    result
}

#[tokio::test]
async fn w6_client_rejects_invalid_handshake_responses() {
    // Not a 101 at all.
    assert!(matches!(
        dial_against("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await,
        Err(WsError::Rejected { status: 200 })
    ));
    assert!(matches!(
        dial_against("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").await,
        Err(WsError::Rejected { status: 401 })
    ));
    // The accept value below is for a different key than the client's.
    let wrong_accept = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                        Connection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
    assert!(matches!(
        dial_against(wrong_accept).await,
        Err(WsError::Connect(_))
    ));
    // A 101 with no accept header.
    assert!(matches!(
        dial_against(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
        )
        .await,
        Err(WsError::Connect(_))
    ));
    // Garbage instead of HTTP.
    assert!(matches!(
        dial_against("WAT\r\n\r\n").await,
        Err(WsError::Connect(_))
    ));
}

/// Dials a fake server that answers with a *correct* accept key plus
/// `extra` header lines (and optionally without `Upgrade`/`Connection`).
async fn dial_fake(extra: &'static str, options: WsOptions) -> Result<WsClient, WsError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = tokio::spawn(async move {
        let (raw, _head) = Raw::accept(&listener, extra).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(raw);
    });
    let result = within("dial", WsClient::dial(&format!("ws://{addr}/"), options)).await;
    serving.abort();
    result
}

#[tokio::test]
async fn w6_client_accepts_only_offered_subprotocols_and_no_extensions() {
    let offering = || WsOptions::default().subprotocol("chat").subprotocol("v2");

    // A correct plain 101 is accepted.
    let client = dial_fake("", fast_options()).await.unwrap();
    assert_eq!(client.subprotocol(), None);

    // The server picks one of the offered subprotocols.
    let client = dial_fake("Sec-WebSocket-Protocol: v2\r\n", offering())
        .await
        .unwrap();
    assert_eq!(client.subprotocol(), Some("v2"));
    assert_eq!(
        client.response_headers().get("sec-websocket-protocol"),
        Some("v2")
    );

    // The server picks something that was not offered.
    assert!(matches!(
        dial_fake("Sec-WebSocket-Protocol: mqtt\r\n", offering()).await,
        Err(WsError::Connect(_))
    ));
    // The server picks a subprotocol although none was offered.
    assert!(matches!(
        dial_fake("Sec-WebSocket-Protocol: chat\r\n", fast_options()).await,
        Err(WsError::Connect(_))
    ));
    // The server answers with two subprotocols.
    assert!(matches!(
        dial_fake(
            "Sec-WebSocket-Protocol: chat\r\nSec-WebSocket-Protocol: v2\r\n",
            offering()
        )
        .await,
        Err(WsError::Connect(_))
    ));
    // The server enables an extension nobody asked for.
    assert!(matches!(
        dial_fake(
            "Sec-WebSocket-Extensions: permessage-deflate\r\n",
            fast_options()
        )
        .await,
        Err(WsError::Connect(_))
    ));
}

/// A client connected to a raw fake server.
async fn client_and_fake_server() -> (WsClient, Raw) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepting = tokio::spawn(async move { Raw::accept(&listener, "").await.0 });
    let client = WsClient::dial(&format!("ws://{addr}/"), fast_options())
        .await
        .unwrap();
    (client, accepting.await.unwrap())
}

fn closed_with(msg: Option<Message>) -> u16 {
    match msg {
        Some(Message::Close { code, .. }) => code,
        other => panic!("expected a close, got {other:?}"),
    }
}

#[tokio::test]
async fn w5_client_rejects_masked_server_frames() {
    let (mut client, mut raw) = client_and_fake_server().await;
    raw.send_wrongly_masked(FIN | OP_TEXT, b"masked").await;
    assert_eq!(
        closed_with(within("close", client.recv()).await),
        close_code::PROTOCOL_ERROR
    );
    assert_eq!(client.recv().await, None);
    // The client told the server why, in a properly masked close frame.
    assert_eq!(raw.read_close().await.0, close_code::PROTOCOL_ERROR);
}

#[tokio::test]
async fn w5_client_applies_the_same_frame_rules_to_the_server() {
    // Reserved bits.
    let (mut client, mut raw) = client_and_fake_server().await;
    raw.send(FIN | 0x40 | OP_TEXT, b"x").await;
    assert_eq!(closed_with(within("close", client.recv()).await), 1002);
    assert_eq!(raw.read_close().await.0, 1002);

    // Invalid UTF-8.
    let (mut client, mut raw) = client_and_fake_server().await;
    raw.send(FIN | OP_TEXT, &[0xFF]).await;
    assert_eq!(closed_with(within("close", client.recv()).await), 1007);
    assert_eq!(raw.read_close().await.0, 1007);

    // A fragmented ping.
    let (mut client, mut raw) = client_and_fake_server().await;
    raw.send(OP_PING, b"").await;
    assert_eq!(closed_with(within("close", client.recv()).await), 1002);
    assert_eq!(raw.read_close().await.0, 1002);

    // A message over the client's cap.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepting = tokio::spawn(async move { Raw::accept(&listener, "").await.0 });
    let options = WsOptions {
        max_message_size: Some(8),
        ..fast_options()
    };
    let mut client = WsClient::dial(&format!("ws://{addr}/"), options)
        .await
        .unwrap();
    let mut raw = accepting.await.unwrap();
    raw.send(FIN | OP_BINARY, &[0u8; 9]).await;
    assert_eq!(closed_with(within("close", client.recv()).await), 1009);
    assert_eq!(raw.read_close().await.0, 1009);
}

#[tokio::test]
async fn client_answers_pings_and_sees_the_servers_close_code() {
    let (mut client, mut raw) = client_and_fake_server().await;
    raw.send(FIN | OP_PING, b"are you there").await;
    assert_eq!(
        raw.read().await,
        Some((FIN | OP_PONG, b"are you there".to_vec()))
    );
    raw.send_text("hello").await;
    assert_eq!(client.recv().await, Some(Message::text("hello")));

    raw.send(FIN | OP_CLOSE, &close_body(4100, "maintenance"))
        .await;
    assert_eq!(
        within("close", client.recv()).await,
        Some(Message::close(4100, "maintenance"))
    );
    // The client echoes the code.
    assert_eq!(raw.read_close().await.0, 4100);
}

#[tokio::test]
async fn a_dropped_transport_is_reported_as_1006() {
    let (mut client, raw) = client_and_fake_server().await;
    drop(raw);
    assert_eq!(
        closed_with(within("close", client.recv()).await),
        close_code::ABNORMAL
    );
    assert_eq!(client.recv().await, None);
}

#[tokio::test]
async fn frames_sent_in_the_same_packet_as_the_handshake_are_not_lost() {
    // Server side: the request head and the first frame arrive together.
    let server = echo_server().await;
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let mut bytes = upgrade_request(&server.addr.to_string(), "/", "").into_bytes();
    bytes.extend(ws_common::frame(FIN | OP_TEXT, b"early", MASK));
    tokio::io::AsyncWriteExt::write_all(&mut stream, &bytes)
        .await
        .unwrap();
    let head = ws_common::read_head(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 101 "), "{head:?}");
    let mut reply = [0u8; 2 + 11];
    within(
        "echo",
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut reply),
    )
    .await
    .unwrap();
    assert_eq!(&reply[2..], b"echo: early");
    drop(stream);
    server.stop().await;

    // Client side: the 101 and the first frame arrive together.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = ws_common::read_head(&mut stream).await;
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap()
            .to_string();
        let mut bytes = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\r\n",
            ws_common::accept_for(&key)
        )
        .into_bytes();
        bytes.extend(ws_common::frame(FIN | OP_TEXT, b"welcome", None));
        tokio::io::AsyncWriteExt::write_all(&mut stream, &bytes)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    });
    let mut client = WsClient::dial(&format!("ws://{addr}/"), fast_options())
        .await
        .unwrap();
    assert_eq!(
        within("first message", client.recv()).await,
        Some(Message::text("welcome"))
    );
    serving.abort();
}
