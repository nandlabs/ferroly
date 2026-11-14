#![cfg(feature = "ws")]
//! End-to-end happy paths: the in-house client against the in-house server
//! (reply-style adapter, connection handler, lifecycle component), close
//! codes in both directions, and the option defaults.

mod ws_common;

use std::time::Duration;

use ferroly::lifecycle::{
    Component, ComponentManager, ComponentState, HealthStatus, LifecycleError,
};
use ferroly::ws::{
    close_code, server, Message, WsClient, WsConn, WsError, WsOptions, WsServer, WsServerConfig,
    DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_MESSAGE_SIZE,
};
use tokio::net::TcpListener;
use ws_common::{echo, fast_config, fast_options, start, within};

/// The original `serve(listener, on_message)` shape still compiles and
/// works.
async fn spawn_reply_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = server::serve(listener, |msg| match msg {
            Message::Text(t) => Some(Message::text(format!("echo: {t}"))),
            other => Some(other),
        })
        .await;
    });
    format!("ws://{addr}/ws")
}

#[tokio::test]
async fn client_server_echo() {
    let url = spawn_reply_server().await;

    let mut client = WsClient::dial(&url, WsOptions::default()).await.unwrap();

    client.send(Message::text("hello")).await.unwrap();
    let reply = client.recv().await.unwrap();
    assert_eq!(reply, Message::text("echo: hello"));

    client.send(Message::binary(vec![1, 2, 3])).await.unwrap();
    let reply = client.recv().await.unwrap();
    assert_eq!(reply, Message::Binary(vec![1, 2, 3]));

    // A larger payload exercises the 16-bit extended length path.
    let big = "x".repeat(500);
    client.send(Message::text(big.as_str())).await.unwrap();
    let reply = client.recv().await.unwrap();
    assert_eq!(reply, Message::text(format!("echo: {big}")));

    // And one that needs the 64-bit length form.
    let huge = vec![7u8; 70_000];
    client.send(Message::binary(huge.clone())).await.unwrap();
    assert_eq!(client.recv().await.unwrap(), Message::Binary(huge));

    client.close().await.unwrap();
}

#[tokio::test]
async fn reply_server_with_options_applies_the_size_caps() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let options = WsOptions {
            max_frame_size: Some(16),
            max_message_size: Some(16),
            ..WsOptions::default()
        };
        let _ = server::serve_with_options(listener, Some, options).await;
    });
    let mut client = WsClient::dial(&format!("ws://{addr}/"), fast_options())
        .await
        .unwrap();
    client.send(Message::text("fits")).await.unwrap();
    assert_eq!(client.recv().await, Some(Message::text("fits")));
    client.send(Message::text("x".repeat(64))).await.unwrap();
    assert_eq!(
        within("close", client.recv()).await.map(|m| match m {
            Message::Close { code, .. } => code,
            other => panic!("expected a close, got {other:?}"),
        }),
        Some(close_code::MESSAGE_TOO_BIG)
    );
}

#[test]
fn options_defaults_are_finite_and_message_helpers_work() {
    // W6: the defaults must be bounded.
    let opts = WsOptions::default();
    assert_eq!(opts.max_message_size, Some(DEFAULT_MAX_MESSAGE_SIZE));
    assert_eq!(opts.max_frame_size, Some(DEFAULT_MAX_FRAME_SIZE));
    assert!(opts.connect_timeout <= Duration::from_secs(60));
    assert!(opts.handshake_timeout <= Duration::from_secs(60));
    assert!(opts.send_queue_capacity > 0 && opts.send_queue_capacity < 100_000);

    let opts = WsOptions {
        max_message_size: Some(1024),
        max_frame_size: Some(512),
        ..WsOptions::default()
    };
    assert_eq!(opts.max_message_size, Some(1024));
    assert_eq!(opts.max_frame_size, Some(512));

    let server = WsServerConfig::default();
    assert_eq!(server.max_frame_size, DEFAULT_MAX_FRAME_SIZE);
    assert!(server.max_connections > 0);

    let t = Message::text("hi");
    assert_eq!(t.as_text(), Some("hi"));
    assert_eq!(t.len(), 2);
    let b = Message::binary(vec![1, 2, 3]);
    assert!(b.as_text().is_none());
    assert_eq!(b.as_binary(), Some(&[1u8, 2, 3][..]));
    assert!(Message::close(1000, "").is_close());
    assert!(close_code::is_sendable(1000) && !close_code::is_sendable(1006));
}

#[tokio::test]
async fn dial_bad_url_errors() {
    // no server here -> connection error
    let result = WsClient::dial("ws://127.0.0.1:1/ws", WsOptions::default()).await;
    assert!(matches!(result, Err(WsError::Connect(_))));
    let result = WsClient::dial("not a url", WsOptions::default()).await;
    assert!(matches!(result, Err(WsError::Connect(_))));
}

#[tokio::test]
async fn client_close_code_and_reason_reach_the_server_handler() {
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let seen_tx = std::sync::Mutex::new(Some(seen_tx));
    let server = start(fast_config(), move |mut conn: WsConn| {
        let seen_tx = seen_tx.lock().unwrap().take();
        async move {
            let mut last = None;
            while let Some(msg) = conn.recv().await {
                last = Some(msg);
            }
            if let Some(tx) = seen_tx {
                let _ = tx.send(last);
            }
        }
    })
    .await;

    let client = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    client.close_with(4321, "client says bye").await.unwrap();

    let last = within("handler result", seen_rx).await.unwrap();
    assert_eq!(last, Some(Message::close(4321, "client says bye")));
    server.stop().await;
}

#[tokio::test]
async fn server_close_code_and_reason_reach_the_client() {
    let server = start(fast_config(), |mut conn: WsConn| async move {
        // Queued data is flushed before the close frame.
        conn.send(Message::text("before close")).await.unwrap();
        conn.close(4000, "server says bye").unwrap();
        // Closing is idempotent and reserved codes are refused.
        assert!(conn.close(4001, "ignored").is_ok());
        assert!(matches!(conn.close(1006, ""), Err(WsError::Protocol(_))));
        assert!(matches!(
            conn.send(Message::text("too late")).await,
            Err(WsError::Closed)
        ));
        while conn.recv().await.is_some() {}
    })
    .await;

    let mut client = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    assert_eq!(client.recv().await, Some(Message::text("before close")));
    assert_eq!(
        within("close", client.recv()).await,
        Some(Message::close(4000, "server says bye"))
    );
    assert_eq!(client.recv().await, None);
    assert!(client.is_closed());
    assert!(matches!(
        client.send(Message::text("x")).await,
        Err(WsError::Closed)
    ));
    server.stop().await;
}

#[tokio::test]
async fn a_handler_that_returns_closes_with_1000_and_a_panic_with_1011() {
    let server = start(fast_config(), |mut conn: WsConn| async move {
        if let Some(Message::Text(text)) = conn.recv().await {
            if text == "panic" {
                panic!("handler blew up (expected by the test)");
            }
        }
    })
    .await;

    let mut client = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    client.send(Message::text("return")).await.unwrap();
    assert_eq!(
        within("close", client.recv()).await,
        Some(Message::close(1000, ""))
    );

    let mut client = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    client.send(Message::text("panic")).await.unwrap();
    assert_eq!(
        within("close", client.recv()).await,
        Some(Message::close(1011, "internal error"))
    );

    // The panic did not take the server down.
    let mut client = WsClient::dial(&server.url("/"), fast_options())
        .await
        .unwrap();
    client.send(Message::text("return")).await.unwrap();
    assert!(within("close", client.recv()).await.unwrap().is_close());
    server.stop().await;
}

#[tokio::test]
async fn ws_server_component_start_serves_and_stops() {
    let server = WsServer::new("ws", "127.0.0.1:0", |msg| match msg {
        Message::Text(t) => Some(Message::text(format!("echo: {t}"))),
        other => Some(other),
    })
    .with_config(fast_config());
    assert_eq!(server.id(), "ws");
    assert_eq!(server.health(), None);
    assert_eq!(server.connections(), 0);

    server.start().await.unwrap();
    assert_eq!(server.health(), Some(HealthStatus::Up));
    let addr = server.local_addr().expect("bound after start");
    let url = format!("ws://{addr}/ws");

    let mut client = WsClient::dial(&url, WsOptions::default()).await.unwrap();
    client.send(Message::text("hi")).await.unwrap();
    assert_eq!(client.recv().await.unwrap(), Message::text("echo: hi"));
    assert_eq!(server.connections(), 1);
    client.close().await.unwrap();

    // Graceful stop: the accept loop halts and stop() returns.
    server.stop().await.unwrap();
    assert_eq!(server.health(), None);

    // After stop, new connections are refused (nothing is accepting).
    let after = WsClient::dial(&url, WsOptions::default()).await;
    assert!(after.is_err());
}

#[tokio::test]
async fn ws_server_component_is_restartable_and_stop_is_idempotent() {
    let server = WsServer::with_handler("ws", "127.0.0.1:0", echo).with_config(fast_config());

    // Stopping a component that never started (or is in `Error`) is fine.
    server.stop().await.unwrap();

    server.start().await.unwrap();
    // A second start while running is refused instead of leaking a listener.
    assert!(matches!(
        server.start().await,
        Err(LifecycleError::ComponentAlreadyStarted(_))
    ));
    let first = server.local_addr().unwrap();
    server.stop().await.unwrap();
    server.stop().await.unwrap();

    // Restart: binds again and serves.
    server.start().await.unwrap();
    let second = server.local_addr().unwrap();
    let mut client = WsClient::dial(&format!("ws://{second}/"), fast_options())
        .await
        .unwrap();
    client.send(Message::text("again")).await.unwrap();
    assert_eq!(client.recv().await, Some(Message::text("echo: again")));
    assert_eq!(server.health(), Some(HealthStatus::Up));
    server.stop().await.unwrap();
    let _ = first;
}

#[tokio::test]
async fn ws_server_component_survives_a_cancelled_start_and_stop() {
    let server = std::sync::Arc::new(
        WsServer::with_handler("ws", "127.0.0.1:0", echo).with_config(WsServerConfig {
            drain_timeout: Duration::from_millis(300),
            ..fast_config()
        }),
    );

    // A start that is aborted (as the lifecycle manager does on timeout)
    // must leave the component startable.
    let starting = {
        let server = server.clone();
        tokio::spawn(async move { server.start().await })
    };
    starting.abort();
    let _ = starting.await;
    if server.health().is_none() {
        server.start().await.unwrap();
    }
    assert_eq!(server.health(), Some(HealthStatus::Up));
    let addr = server.local_addr().unwrap();

    // A raw peer that never answers the close frame keeps the drain busy.
    let _silent = ws_common::Raw::connect(addr).await;

    // A stop that is abandoned mid-drain still completes in the background,
    // and the component can be started again afterwards.
    let stopping = {
        let server = server.clone();
        tokio::spawn(async move { server.stop().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    stopping.abort();
    let _ = stopping.await;

    server.stop().await.unwrap();
    within("restart", server.start()).await.unwrap();
    assert_eq!(server.health(), Some(HealthStatus::Up));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn health_is_degraded_when_every_slot_is_taken() {
    let server = WsServer::with_handler("ws", "127.0.0.1:0", echo).with_config(WsServerConfig {
        max_connections: 1,
        ..fast_config()
    });
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();
    assert_eq!(server.health(), Some(HealthStatus::Up));

    let client = WsClient::dial(&format!("ws://{addr}/"), fast_options())
        .await
        .unwrap();
    assert_eq!(server.connections(), 1);
    assert_eq!(server.health(), Some(HealthStatus::Degraded));
    client.close().await.unwrap();
    server.stop().await.unwrap();
}

#[tokio::test]
async fn ws_server_runs_under_the_component_manager() {
    let server = std::sync::Arc::new(
        WsServer::with_handler("ws", "127.0.0.1:0", echo).with_config(fast_config()),
    );
    let manager = ComponentManager::new();
    manager.register(server.clone()).unwrap();

    for round in 0..2 {
        manager.start_all().await.unwrap();
        assert_eq!(manager.get_state("ws"), Some(ComponentState::Running));
        assert_eq!(manager.health().get("ws"), Some(HealthStatus::Up));

        let url = format!("ws://{}/", server.local_addr().unwrap());
        let mut client = WsClient::dial(&url, fast_options()).await.unwrap();
        client.send(Message::text("managed")).await.unwrap();
        assert_eq!(client.recv().await, Some(Message::text("echo: managed")));

        // Stopping through the manager is graceful: the client is told the
        // server is going away.
        within("stop_all", manager.stop_all()).await.unwrap();
        assert_eq!(manager.get_state("ws"), Some(ComponentState::Stopped));
        assert_eq!(
            within("close", client.recv()).await,
            Some(Message::close(
                close_code::GOING_AWAY,
                "server shutting down"
            )),
            "round {round}"
        );
    }
}
