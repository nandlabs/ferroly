#![cfg(feature = "http")]
//! Drives the in-house HTTP client against a local canned TCP server.

use ferroly::http::{Client, Method, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawns a one-shot server that reads a request and replies with `response`.
async fn canned_server(response: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        sock.write_all(response).await.unwrap();
        sock.flush().await.unwrap();
    });
    format!("http://{addr}/")
}

#[tokio::test]
async fn content_length_response() {
    let url = canned_server(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").await;
    let client = Client::new();
    let req = Request::builder(Method::Get, &url).unwrap().build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.is_success());
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn chunked_response_streams() {
    let url = canned_server(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n6\r\n World\r\n0\r\n\r\n",
    )
    .await;
    let client = Client::new();
    let req = Request::builder(Method::Get, &url).unwrap().build();
    let mut resp = client.send(req).await.unwrap();

    let mut chunks = Vec::new();
    while let Some(c) = resp.chunk().await.unwrap() {
        chunks.push(String::from_utf8(c).unwrap());
    }
    assert_eq!(chunks, vec!["Hello".to_string(), " World".to_string()]);
}

#[tokio::test]
async fn posts_body_and_headers() {
    let url = canned_server(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok").await;
    let client = Client::new();
    let req = Request::builder(Method::Post, &url)
        .unwrap()
        .header("Content-Type", "application/json")
        .body(b"{}".to_vec())
        .build();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    assert_eq!(resp.text().await.unwrap(), "ok");
}
