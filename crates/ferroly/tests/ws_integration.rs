#![cfg(feature = "ws")]
//! End-to-end: start the in-house WebSocket echo server on an ephemeral port,
//! connect with the in-house client, exchange messages, and close.

use ferroly::ws::{server, Message, WsClient, WsOptions, WsServer};
use tokio::net::TcpListener;

async fn spawn_echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = server::serve(listener, |msg| match msg {
            Message::Text(t) => Some(Message::text(format!("echo: {t}"))),
            Message::Binary(b) => Some(Message::Binary(b)),
        })
        .await;
    });
    format!("ws://{addr}/ws")
}

#[tokio::test]
async fn client_server_echo() {
    let url = spawn_echo_server().await;

    let mut client = WsClient::dial(&url, WsOptions::default()).await.unwrap();

    client.send(Message::text("hello")).unwrap();
    let reply = client.recv().await.unwrap();
    assert_eq!(reply, Message::text("echo: hello"));

    client.send(Message::binary(vec![1, 2, 3])).unwrap();
    let reply = client.recv().await.unwrap();
    assert_eq!(reply, Message::Binary(vec![1, 2, 3]));

    // A larger payload exercises the 16-bit extended length path.
    let big = "x".repeat(500);
    client.send(Message::text(big.as_str())).unwrap();
    let reply = client.recv().await.unwrap();
    assert_eq!(reply, Message::text(format!("echo: {big}")));

    client.close().await.unwrap();
}

#[test]
fn options_and_message_helpers() {
    let opts = WsOptions {
        max_message_size: Some(1024),
        max_frame_size: Some(512),
    };
    assert_eq!(opts.max_message_size, Some(1024));
    assert_eq!(opts.max_frame_size, Some(512));

    let t = Message::text("hi");
    assert_eq!(t.as_text(), Some("hi"));
    assert!(Message::binary(vec![1, 2, 3]).as_text().is_none());
}

#[tokio::test]
async fn dial_bad_url_errors() {
    // no server here -> connection error
    let result = WsClient::dial("ws://127.0.0.1:1/ws", WsOptions::default()).await;
    assert!(matches!(result, Err(ferroly::ws::WsError::Connect(_))));
}

#[tokio::test]
async fn ws_server_component_start_serves_and_stops() {
    use ferroly::lifecycle::Component;

    let server = WsServer::new("ws", "127.0.0.1:0", |msg| match msg {
        Message::Text(t) => Some(Message::text(format!("echo: {t}"))),
        Message::Binary(b) => Some(Message::Binary(b)),
    });

    server.start().await.unwrap();
    let addr = server.local_addr().expect("bound after start");
    let url = format!("ws://{addr}/ws");

    let mut client = WsClient::dial(&url, WsOptions::default()).await.unwrap();
    client.send(Message::text("hi")).unwrap();
    assert_eq!(client.recv().await.unwrap(), Message::text("echo: hi"));
    client.close().await.unwrap();

    // Graceful stop: the accept loop halts and stop() returns.
    server.stop().await.unwrap();

    // After stop, new connections are refused (nothing is accepting).
    let after = WsClient::dial(&url, WsOptions::default()).await;
    assert!(after.is_err());
}
