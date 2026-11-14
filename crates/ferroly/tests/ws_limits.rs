#![cfg(feature = "ws")]
//! Resource bounds and timeouts: bounded queues and backpressure (W1), idle
//! timeout, write timeout, keepalive and the connection limit (W3), graceful
//! shutdown (W4), and the client's dial timeouts (W6).

mod ws_common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferroly::lifecycle::Component;
use ferroly::ws::{
    close_code, Message, TimeoutKind, WsClient, WsConn, WsError, WsEvent, WsOptions, WsServer,
    WsServerConfig,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use ws_common::{
    close_body, echo, fast_config, fast_options, start, within, Raw, Recorder, FIN, OP_CLOSE,
    OP_PING, OP_PONG,
};

const BIG: usize = 256 * 1024;

fn close_of(msg: Option<Message>) -> (u16, String) {
    match msg {
        Some(Message::Close { code, reason }) => (code, reason),
        other => panic!("expected a close, got {other:?}"),
    }
}

// ---- W1: bounded queues ----------------------------------------------------

#[tokio::test]
async fn w1_a_peer_that_never_reads_cannot_grow_the_send_queue() {
    let recorder = Recorder::new();
    let config = WsServerConfig {
        send_queue_capacity: 4,
        send_timeout: Duration::from_millis(200),
        write_timeout: Some(Duration::from_secs(3)),
        close_timeout: Duration::from_millis(300),
        ..fast_config()
    }
    .observer(recorder.clone());
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let server = start(config, move |conn: WsConn| {
        let result_tx = result_tx.clone();
        async move {
            let mut queued = 0usize;
            let error = loop {
                match conn.send(Message::binary(vec![0u8; BIG])).await {
                    Ok(()) => queued += 1,
                    Err(error) => break error,
                }
            };
            let _ = result_tx.send((queued, error));
        }
    })
    .await;

    // A peer that completes the handshake and then never reads a byte.
    let raw = Raw::connect(server.addr).await;
    let started = Instant::now();
    let (queued, error) = within("handler result", result_rx.recv()).await.unwrap();

    // The send that could not be queued within the send timeout fails...
    assert!(matches!(error, WsError::QueueFull), "{error:?}");
    // ...after a bounded number of messages: what the kernel buffers took,
    // plus the queue. Without a bound this loop would run until memory ran
    // out.
    assert!(queued < 400, "queued {queued} messages of {BIG} bytes");
    assert!(started.elapsed() < Duration::from_secs(4));
    // ...and the slow consumer is closed with 1008.
    let closed = recorder
        .wait_for("closed event", |e| matches!(e, WsEvent::Closed { .. }))
        .await;
    assert!(matches!(
        closed,
        WsEvent::Closed {
            code: close_code::POLICY_VIOLATION,
            ..
        }
    ));
    assert_eq!(
        recorder.count(|e| matches!(e, WsEvent::QueueFull { .. })),
        1
    );
    drop(raw);
    server.stop().await;
}

#[tokio::test]
async fn w1_try_send_reports_a_full_queue_without_closing() {
    let config = WsServerConfig {
        send_queue_capacity: 2,
        ..fast_config()
    };
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let server = start(config, move |mut conn: WsConn| {
        let result_tx = result_tx.clone();
        async move {
            // No await between these calls, so the writer cannot drain the
            // queue in between: exactly `capacity` messages fit.
            let mut queued = 0usize;
            let error = loop {
                match conn.try_send(Message::text(format!("m{queued}"))) {
                    Ok(()) => queued += 1,
                    Err(error) => break error,
                }
            };
            let open_after = !conn.sender().is_closed();
            let _ = result_tx.send((queued, error, open_after));
            // The connection still works once the queue drains.
            conn.send(Message::text("after")).await.unwrap();
            while conn.recv().await.is_some() {}
        }
    })
    .await;

    let mut raw = Raw::connect(server.addr).await;
    let (queued, error, open_after) = within("handler result", result_rx.recv()).await.unwrap();
    assert_eq!(queued, 2);
    assert!(matches!(error, WsError::QueueFull));
    assert!(open_after);
    assert_eq!(raw.read_text().await, "m0");
    assert_eq!(raw.read_text().await, "m1");
    assert_eq!(raw.read_text().await, "after");
    server.stop().await;
}

#[tokio::test]
async fn w1_client_sends_are_bounded_too() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // A fake server that upgrades and then never reads.
    let accepting = tokio::spawn(async move { Raw::accept(&listener, "").await.0 });
    let options = WsOptions {
        send_queue_capacity: 2,
        send_timeout: Duration::from_millis(200),
        write_timeout: Some(Duration::from_secs(3)),
        ..fast_options()
    };
    let mut client = WsClient::dial(&format!("ws://{addr}/"), options)
        .await
        .unwrap();
    let _raw = accepting.await.unwrap();

    let mut queued = 0usize;
    let error = within("client send loop", async {
        loop {
            match client.send(Message::binary(vec![0u8; BIG])).await {
                Ok(()) => queued += 1,
                Err(error) => break error,
            }
        }
    })
    .await;
    assert!(matches!(error, WsError::QueueFull), "{error:?}");
    assert!(queued < 400, "queued {queued}");
    assert_eq!(
        close_of(within("close", client.recv()).await).0,
        close_code::POLICY_VIOLATION
    );
    assert!(matches!(
        client.send(Message::text("x")).await,
        Err(WsError::Closed)
    ));
}

#[tokio::test]
async fn w1_unread_incoming_messages_push_back_on_the_sender() {
    let recorder = Recorder::new();
    let config = WsServerConfig {
        recv_queue_capacity: 2,
        ..fast_config()
    }
    .observer(recorder.clone());
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let release_rx = std::sync::Mutex::new(Some(release_rx));
    let received = Arc::new(AtomicUsize::new(0));
    let counted = received.clone();
    let server = start(config, move |mut conn: WsConn| {
        let release = release_rx.lock().unwrap().take();
        let counted = counted.clone();
        async move {
            // The application is busy: it does not call `recv` yet.
            if let Some(release) = release {
                let _ = release.await;
            }
            while let Some(msg) = conn.recv().await {
                if msg.is_close() {
                    break;
                }
                counted.fetch_add(1, Ordering::SeqCst);
            }
        }
    })
    .await;

    let mut raw = Raw::connect(server.addr).await;
    // 64 MiB of messages: far more than loopback socket buffers hold.
    let one = ws_common::frame(FIN | 0x2, &vec![0u8; 64 * 1024], Some([1, 2, 3, 4]));
    let mut sent = 0usize;
    let stalled = tokio::time::timeout(Duration::from_millis(700), async {
        for _ in 0..1024 {
            raw.stream.write_all(&one).await.unwrap();
            sent += 1;
        }
    })
    .await;
    // The writer stalls: the server stopped reading instead of buffering.
    assert!(stalled.is_err(), "all {sent} messages were absorbed");
    // The server holds at most the queue plus the message being delivered.
    assert!(recorder.count(|e| matches!(e, WsEvent::MessageReceived { .. })) <= 3);

    // Once the application reads, the stream flows again and nothing that
    // was sent is lost.
    release_tx.send(()).unwrap();
    within("drain", async {
        while received.load(Ordering::SeqCst) < sent {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    server.stop().await;
}

// ---- W3: idle timeout, write timeout, keepalive, connection limit ----------

#[tokio::test]
async fn w3_idle_connections_are_closed_with_1001() {
    let recorder = Recorder::new();
    let config = WsServerConfig {
        idle_timeout: Some(Duration::from_millis(300)),
        ..fast_config()
    }
    .observer(recorder.clone());
    let server = start(config, echo).await;

    // An active client outlives several idle periods.
    let mut active = Raw::connect(server.addr).await;
    // A silent client is closed.
    let mut silent = Raw::connect(server.addr).await;
    let started = Instant::now();
    for n in 0..12 {
        active.send_text(&format!("tick {n}")).await;
        assert_eq!(active.read_text().await, format!("echo: tick {n}"));
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert!(started.elapsed() > Duration::from_millis(600));

    assert_eq!(
        silent.read_close().await,
        (close_code::GOING_AWAY, "idle timeout".to_string())
    );
    silent.expect_eof().await;
    assert_eq!(
        recorder.count(|e| matches!(
            e,
            WsEvent::TimedOut {
                kind: TimeoutKind::Idle,
                ..
            }
        )),
        1
    );
    server.stop().await;
}

#[tokio::test]
async fn w3_a_stalled_write_times_out_and_frees_the_connection() {
    let recorder = Recorder::new();
    let config = WsServerConfig {
        write_timeout: Some(Duration::from_millis(250)),
        send_timeout: Duration::from_secs(4),
        send_queue_capacity: 2,
        close_timeout: Duration::from_millis(300),
        ..fast_config()
    }
    .observer(recorder.clone());
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let server = start(config, move |conn: WsConn| {
        let result_tx = result_tx.clone();
        async move {
            let error = loop {
                if let Err(error) = conn.send(Message::binary(vec![0u8; BIG])).await {
                    break error;
                }
            };
            let _ = result_tx.send(error);
        }
    })
    .await;

    let _raw = Raw::connect(server.addr).await; // never reads
    let error = within("handler result", result_rx.recv()).await.unwrap();
    assert!(matches!(error, WsError::Closed), "{error:?}");
    recorder
        .wait_for("write timeout", |e| {
            matches!(
                e,
                WsEvent::TimedOut {
                    kind: TimeoutKind::Write,
                    ..
                }
            )
        })
        .await;
    let closed = recorder
        .wait_for("closed", |e| matches!(e, WsEvent::Closed { .. }))
        .await;
    assert!(matches!(
        closed,
        WsEvent::Closed {
            code: close_code::ABNORMAL,
            ..
        }
    ));
    server.stop().await;
}

#[tokio::test]
async fn w3_keepalive_pings_drop_a_peer_that_does_not_answer() {
    let recorder = Recorder::new();
    let config = WsServerConfig {
        ping_interval: Some(Duration::from_millis(60)),
        pong_timeout: Duration::from_millis(400),
        ..fast_config()
    }
    .observer(recorder.clone());
    let server = start(config, echo).await;

    // A conforming client answers pings and stays connected well past the
    // pong deadline.
    let mut good = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();

    // A raw peer that reads the pings but never answers them.
    let mut deaf = Raw::connect(server.addr).await;
    let (b0, ping_payload) = deaf.read().await.expect("a ping");
    assert_eq!(b0, FIN | OP_PING);
    assert_eq!(
        deaf.read_close().await,
        (close_code::INTERNAL_ERROR, "keepalive timeout".to_string())
    );

    // A pong with the wrong payload is unsolicited: it does not count.
    let mut liar = Raw::connect(server.addr).await;
    let (b0, _) = liar.read().await.expect("a ping");
    assert_eq!(b0, FIN | OP_PING);
    liar.send(FIN | OP_PONG, b"not what you sent").await;
    assert_eq!(liar.read_close().await.0, close_code::INTERNAL_ERROR);

    // A pong with the right payload keeps a raw peer alive.
    let mut honest = Raw::connect(server.addr).await;
    for _ in 0..4 {
        let (b0, payload) = honest.read().await.expect("a ping");
        assert_eq!(b0, FIN | OP_PING);
        honest.send(FIN | OP_PONG, &payload).await;
    }
    drop(honest);
    assert!(!ping_payload.is_empty());

    good.send(Message::text("alive")).await.unwrap();
    assert_eq!(
        within("echo", good.recv()).await,
        Some(Message::text("echo: alive"))
    );
    assert!(recorder.count(|e| matches!(e, WsEvent::PongReceived { .. })) >= 4);
    assert_eq!(
        recorder.count(|e| matches!(
            e,
            WsEvent::TimedOut {
                kind: TimeoutKind::Pong,
                ..
            }
        )),
        2
    );
    server.stop().await;
}

#[tokio::test]
async fn w3_client_keepalive_detects_a_dead_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepting = tokio::spawn(async move { Raw::accept(&listener, "").await.0 });
    let options = fast_options().keepalive(Duration::from_millis(50), Duration::from_millis(120));
    let mut client = WsClient::dial(&format!("ws://{addr}/"), options)
        .await
        .unwrap();
    let mut raw = accepting.await.unwrap();

    // The fake server sees the (masked) ping and stays silent.
    let (b0, _) = raw.read().await.expect("a ping");
    assert_eq!(b0, FIN | OP_PING);
    assert_eq!(
        close_of(within("close", client.recv()).await),
        (close_code::INTERNAL_ERROR, "keepalive timeout".to_string())
    );
    assert_eq!(raw.read_close().await.0, close_code::INTERNAL_ERROR);
}

#[tokio::test]
async fn w3_max_connections_is_configurable_and_enforced() {
    let config = WsServerConfig {
        max_connections: 2,
        ..fast_config()
    };
    let server = start(config, echo).await;
    let url = server.url("/");

    let first = WsClient::dial(&url, fast_options()).await.unwrap();
    let mut second = WsClient::dial(&url, fast_options()).await.unwrap();

    // The third connection is not accepted while both slots are taken: its
    // handshake gets no answer.
    let impatient = WsOptions {
        handshake_timeout: Duration::from_millis(300),
        ..fast_options()
    };
    assert!(matches!(
        WsClient::dial(&url, impatient).await,
        Err(WsError::Timeout(_))
    ));
    // The others are unaffected.
    second.send(Message::text("still")).await.unwrap();
    assert_eq!(second.recv().await, Some(Message::text("echo: still")));

    // A slot frees and the next connection is served.
    first.close().await.unwrap();
    let mut third = within("third dial", WsClient::dial(&url, fast_options()))
        .await
        .unwrap();
    third.send(Message::text("in")).await.unwrap();
    assert_eq!(third.recv().await, Some(Message::text("echo: in")));
    server.stop().await;
}

#[tokio::test]
async fn w3_silent_clients_cannot_pin_every_slot() {
    let config = WsServerConfig {
        max_connections: 2,
        handshake_timeout: Duration::from_millis(250),
        idle_timeout: Some(Duration::from_millis(250)),
        ..fast_config()
    };
    let server = start(config, echo).await;

    // Two connections that never send a handshake.
    let mut mute_a = TcpStream::connect(server.addr).await.unwrap();
    let _mute_b = TcpStream::connect(server.addr).await.unwrap();
    // The handshake timeout evicts them, so a real client gets in.
    let mut client = within("dial", WsClient::dial(&server.url("/"), fast_options()))
        .await
        .unwrap();
    client.send(Message::text("hi")).await.unwrap();
    assert_eq!(client.recv().await, Some(Message::text("echo: hi")));
    // The mute connection was closed by the server without a response.
    let mut rest = Vec::new();
    within(
        "mute connection closed",
        tokio::io::AsyncReadExt::read_to_end(&mut mute_a, &mut rest),
    )
    .await
    .unwrap();
    assert!(rest.is_empty());

    // A slow-loris that trickles half a request head is answered with 408.
    let mut slow = TcpStream::connect(server.addr).await.unwrap();
    slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    within(
        "408",
        tokio::io::AsyncReadExt::read_to_end(&mut slow, &mut response),
    )
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 408 "));
    server.stop().await;
}

// ---- W4: graceful shutdown -------------------------------------------------

#[tokio::test]
async fn w4_shutdown_sends_close_1001_to_live_connections() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let server = start(fast_config(), move |mut conn: WsConn| {
        let seen_tx = seen_tx.clone();
        async move {
            let mut last = None;
            while let Some(msg) = conn.recv().await {
                if let Message::Text(text) = &msg {
                    let _ = conn.send(Message::text(format!("echo: {text}"))).await;
                }
                last = Some(msg);
            }
            let _ = seen_tx.send(last);
        }
    })
    .await;
    let url = server.url("/");

    let mut one = WsClient::dial(&url, fast_options()).await.unwrap();
    let mut two = WsClient::dial(&url, fast_options()).await.unwrap();
    let mut raw = Raw::connect(server.addr).await;
    one.send(Message::text("a")).await.unwrap();
    assert_eq!(one.recv().await, Some(Message::text("echo: a")));

    let stopping = tokio::spawn(server.stop());

    // Every live connection is told the server is going away.
    for client in [&mut one, &mut two] {
        assert_eq!(
            close_of(within("close", client.recv()).await),
            (close_code::GOING_AWAY, "server shutting down".to_string())
        );
        assert_eq!(client.recv().await, None);
    }
    assert_eq!(raw.read_close().await.0, close_code::GOING_AWAY);
    raw.send(FIN | OP_CLOSE, &close_body(1001, "")).await;
    raw.expect_eof().await;

    // The handlers saw it too, and stop() returned once they finished —
    // well inside the drain timeout, because every peer answered.
    let started = Instant::now();
    within("stop", stopping).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(900));
    for _ in 0..3 {
        let last = within("handler", seen_rx.recv()).await.unwrap();
        assert_eq!(close_of(last).0, close_code::GOING_AWAY);
    }
}

#[tokio::test]
async fn w4_drain_is_bounded_when_peers_or_handlers_do_not_cooperate() {
    let config = WsServerConfig {
        drain_timeout: Duration::from_millis(300),
        close_timeout: Duration::from_secs(4),
        ..fast_config()
    };
    // A handler that ignores the end of the stream.
    let server = start(config, |conn: WsConn| async move {
        let _conn = conn;
        tokio::time::sleep(Duration::from_secs(3600)).await;
    })
    .await;

    // A peer that never answers the close frame.
    let mut silent = Raw::connect(server.addr).await;
    // And a connection still in its handshake.
    let mut half_open = TcpStream::connect(server.addr).await.unwrap();
    half_open.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    server.stop().await;
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(250), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");

    // The close frame was still sent before the connection was dropped.
    assert_eq!(silent.read_close().await.0, close_code::GOING_AWAY);
    silent.expect_eof().await;
    let mut rest = Vec::new();
    within(
        "half-open closed",
        tokio::io::AsyncReadExt::read_to_end(&mut half_open, &mut rest),
    )
    .await
    .ok();
}

#[tokio::test]
async fn w4_dropping_the_serve_future_also_sends_1001() {
    let server = start(fast_config(), echo).await;
    let mut client = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    client.send(Message::text("x")).await.unwrap();
    assert_eq!(client.recv().await, Some(Message::text("echo: x")));
    let addr = server.addr;

    // No shutdown signal: the task running `serve_with_config` is aborted.
    server.abort();
    assert_eq!(
        close_of(within("close", client.recv()).await).0,
        close_code::GOING_AWAY
    );
    // And the listener is gone.
    within("refused", async {
        loop {
            if TcpStream::connect(addr).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn w4_component_stop_sends_1001_and_tracks_connections() {
    let server = WsServer::with_handler("ws", "127.0.0.1:0", echo).with_config(fast_config());
    server.start().await.unwrap();
    let url = format!("ws://{}/", server.local_addr().unwrap());

    let mut clients = Vec::new();
    for _ in 0..3 {
        let mut client = WsClient::dial(&url, fast_options()).await.unwrap();
        client.send(Message::text("up")).await.unwrap();
        assert_eq!(client.recv().await, Some(Message::text("echo: up")));
        clients.push(client);
    }
    assert_eq!(server.connections(), 3);

    let started = Instant::now();
    within("stop", server.stop()).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(900));
    assert_eq!(server.connections(), 0);
    for client in &mut clients {
        assert_eq!(
            close_of(within("close", client.recv()).await).0,
            close_code::GOING_AWAY
        );
    }
}

// ---- W6: client dial timeouts ----------------------------------------------

#[tokio::test]
async fn w6_dial_times_out_when_the_server_never_answers_the_upgrade() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accepts TCP, reads the request, says nothing.
    let holding = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(stream);
    });
    let options = WsOptions {
        handshake_timeout: Duration::from_millis(200),
        ..fast_options()
    };
    let started = Instant::now();
    let result = WsClient::dial(&format!("ws://{addr}/"), options).await;
    assert!(
        matches!(&result, Err(WsError::Timeout(what)) if what == "handshake"),
        "{:?}",
        result.err()
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    holding.abort();
}

#[tokio::test]
async fn w6_dial_times_out_when_the_connection_cannot_be_established() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accepts TCP but never speaks TLS, so a `wss` connect cannot finish.
    let holding = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(stream);
    });
    let options = WsOptions {
        connect_timeout: Duration::from_millis(200),
        ..fast_options()
    };
    let started = Instant::now();
    let result = WsClient::dial(&format!("wss://{addr}/"), options).await;
    assert!(
        matches!(&result, Err(WsError::Timeout(what)) if what == "connect"),
        "{:?}",
        result.err()
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    holding.abort();
}
