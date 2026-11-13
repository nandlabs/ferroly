//! Shared helpers for the `ws_*` integration suites: a raw TCP WebSocket
//! peer (so tests can send input the real client would refuse to send) and
//! a test-server launcher with short timeouts.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::ws::{
    server, Message, WsConn, WsEvent, WsHandler, WsObserver, WsOptions, WsServerConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// The RFC 6455 §1.3 sample key and its accept value.
pub const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
pub const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// Upper bound for any single step of a test; hitting it is a failure.
pub const STEP: Duration = Duration::from_secs(5);

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;
pub const FIN: u8 = 0x80;

/// Awaits `fut`, panicking if it takes longer than [`STEP`].
pub async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(STEP, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out: {what}"),
    }
}

/// A server configuration with short timeouts and no keepalive, so tests
/// are fast and nothing fires unless a test asks for it.
pub fn fast_config() -> WsServerConfig {
    WsServerConfig {
        handshake_timeout: Duration::from_secs(2),
        idle_timeout: None,
        ping_interval: None,
        write_timeout: Some(Duration::from_secs(2)),
        close_timeout: Duration::from_millis(500),
        drain_timeout: Duration::from_secs(1),
        send_timeout: Duration::from_secs(1),
        ..WsServerConfig::default()
    }
}

/// Client options with short timeouts.
pub fn fast_options() -> WsOptions {
    WsOptions {
        connect_timeout: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(2),
        close_timeout: Duration::from_millis(500),
        ..WsOptions::default()
    }
}

/// Echoes text (prefixed) and binary messages until the connection ends.
pub async fn echo(mut conn: WsConn) {
    while let Some(msg) = conn.recv().await {
        let reply = match msg {
            Message::Text(text) => Message::text(format!("echo: {text}")),
            Message::Binary(bytes) => Message::Binary(bytes),
            Message::Close { .. } => break,
        };
        if conn.send(reply).await.is_err() {
            break;
        }
    }
}

/// A server running on an ephemeral port.
pub struct TestServer {
    pub addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{}", self.addr, path)
    }

    /// Signals shutdown and waits for the drain to finish.
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            within("server drain", task).await.unwrap();
        }
    }

    /// Aborts the serve future without a shutdown signal (the drop path).
    pub fn abort(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        // Keep the shutdown sender alive until the task is gone so that the
        // abort, not the signal, is what ends the server.
        std::mem::forget(self.shutdown.take());
    }
}

pub async fn start(config: WsServerConfig, handler: impl WsHandler) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown, signal) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let _ = signal.await;
        };
        server::serve_with_config(listener, handler, shutdown, config)
            .await
            .unwrap();
    });
    TestServer {
        addr,
        shutdown: Some(shutdown),
        task: Some(task),
    }
}

/// An observer that records every event.
#[derive(Default)]
pub struct Recorder {
    events: Mutex<Vec<WsEvent>>,
}

impl WsObserver for Recorder {
    fn on_event(&self, event: &WsEvent) {
        self.events.lock().unwrap().push(*event);
    }
}

impl Recorder {
    pub fn new() -> Arc<Recorder> {
        Arc::new(Recorder::default())
    }

    pub fn events(&self) -> Vec<WsEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn count(&self, pred: impl Fn(&WsEvent) -> bool) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| pred(e))
            .count()
    }

    /// Waits until at least one recorded event matches `pred`.
    pub async fn wait_for(&self, what: &str, pred: impl Fn(&WsEvent) -> bool) -> WsEvent {
        within(what, async {
            loop {
                if let Some(found) = self.events.lock().unwrap().iter().find(|e| pred(e)) {
                    return *found;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
    }
}

/// Builds a well-formed upgrade request; `extra` is raw header lines, each
/// ending in CRLF.
pub fn upgrade_request(host: &str, path: &str, extra: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\n{extra}\r\n"
    )
}

/// Reads an HTTP head up to and including the blank line, one byte at a
/// time so that no byte after it is consumed.
pub async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    within("reading an HTTP head", async {
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            match stream.read(&mut byte).await {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
    })
    .await;
    String::from_utf8_lossy(&head).into_owned()
}

/// Sends raw bytes as an HTTP request and returns the whole response (the
/// server is expected to close the connection).
pub async fn http_exchange(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    within(
        "reading an HTTP response",
        stream.read_to_end(&mut response),
    )
    .await
    .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// The status code of a raw HTTP response.
pub fn status_of(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP response: {response:?}"))
}

/// Encodes one frame with an explicit first byte (FIN/RSV/opcode), masked
/// with `mask` when given.
pub fn frame(b0: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = vec![b0];
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        n if n < 126 => out.push(mask_bit | n as u8),
        n if n <= 0xffff => {
            out.push(mask_bit | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(mask_bit | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// A close-frame payload.
pub fn close_body(code: u16, reason: &str) -> Vec<u8> {
    let mut out = code.to_be_bytes().to_vec();
    out.extend_from_slice(reason.as_bytes());
    out
}

/// A raw WebSocket peer over TCP: it speaks frames by hand and enforces
/// nothing, so it can break any rule on purpose.
pub struct Raw {
    pub stream: TcpStream,
    /// Whether this peer plays the client (and so masks what it sends).
    client: bool,
}

impl Raw {
    /// Connects to a server and completes a normal opening handshake.
    pub async fn connect(addr: SocketAddr) -> Raw {
        Self::connect_with(addr, "/", "").await.0
    }

    /// Like [`connect`](Self::connect) with a path and extra header lines;
    /// also returns the server's response head.
    pub async fn connect_with(addr: SocketAddr, path: &str, extra: &str) -> (Raw, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = upgrade_request(&addr.to_string(), path, extra);
        stream.write_all(request.as_bytes()).await.unwrap();
        let head = read_head(&mut stream).await;
        assert!(
            head.starts_with("HTTP/1.1 101 "),
            "expected a 101, got: {head:?}"
        );
        assert!(head.contains(&format!("Sec-WebSocket-Accept: {ACCEPT}\r\n")));
        (
            Raw {
                stream,
                client: true,
            },
            head,
        )
    }

    /// Plays the server: accepts one connection, reads the client's upgrade
    /// request and answers it with a correct `101` plus `extra` header
    /// lines. Returns the peer and the request head.
    pub async fn accept(listener: &TcpListener, extra: &str) -> (Raw, String) {
        let (mut stream, _) = within("accept", listener.accept()).await.unwrap();
        let head = read_head(&mut stream).await;
        let key = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim().to_string())
            })
            .expect("the client sent a key");
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n{extra}\r\n",
            accept_for(&key)
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        (
            Raw {
                stream,
                client: false,
            },
            head,
        )
    }

    fn mask(&self) -> Option<[u8; 4]> {
        self.client.then_some([0x12, 0x34, 0x56, 0x78])
    }

    /// Sends a frame with the correct masking for this peer's role.
    pub async fn send(&mut self, b0: u8, payload: &[u8]) {
        let bytes = frame(b0, payload, self.mask());
        self.send_bytes(&bytes).await;
    }

    /// Sends a frame with the **wrong** masking for this peer's role.
    pub async fn send_wrongly_masked(&mut self, b0: u8, payload: &[u8]) {
        let mask = if self.client {
            None
        } else {
            Some([0x12, 0x34, 0x56, 0x78])
        };
        self.send_bytes(&frame(b0, payload, mask)).await;
    }

    pub async fn send_text(&mut self, text: &str) {
        self.send(FIN | OP_TEXT, text.as_bytes()).await;
    }

    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        within("raw write", self.stream.write_all(bytes))
            .await
            .unwrap();
    }

    /// Reads one frame: `(first byte, unmasked payload)`. `None` on EOF or a
    /// reset connection.
    pub async fn read(&mut self) -> Option<(u8, Vec<u8>)> {
        within("raw frame read", async {
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head).await.ok()?;
            let masked = head[1] & 0x80 != 0;
            let len = match head[1] & 0x7f {
                126 => {
                    let mut ext = [0u8; 2];
                    self.stream.read_exact(&mut ext).await.ok()?;
                    u16::from_be_bytes(ext) as usize
                }
                127 => {
                    let mut ext = [0u8; 8];
                    self.stream.read_exact(&mut ext).await.ok()?;
                    u64::from_be_bytes(ext) as usize
                }
                n => n as usize,
            };
            let mut key = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut key).await.ok()?;
            }
            let mut payload = vec![0u8; len];
            self.stream.read_exact(&mut payload).await.ok()?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= key[i % 4];
                }
            }
            // A conforming server never masks; a conforming client always
            // does.
            assert_eq!(masked, !self.client, "wrong masking direction from peer");
            Some((head[0], payload))
        })
        .await
    }

    /// Reads frames until a text frame arrives and returns its text.
    pub async fn read_text(&mut self) -> String {
        loop {
            match self.read().await {
                Some((b0, payload)) if b0 == FIN | OP_TEXT => {
                    return String::from_utf8(payload).unwrap()
                }
                Some((b0, _)) if b0 & 0x0f >= 0x8 => continue,
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }

    /// Reads frames (skipping pings and data) until the close frame and
    /// returns its `(code, reason)`. Panics if the connection ends without
    /// one.
    pub async fn read_close(&mut self) -> (u16, String) {
        loop {
            match self.read().await {
                Some((b0, payload)) if b0 == FIN | OP_CLOSE => {
                    if payload.is_empty() {
                        return (1005, String::new());
                    }
                    let code = u16::from_be_bytes([payload[0], payload[1]]);
                    return (code, String::from_utf8(payload[2..].to_vec()).unwrap());
                }
                Some(_) => continue,
                None => panic!("the connection ended without a close frame"),
            }
        }
    }

    /// Asserts that the peer closes the connection (EOF or reset) without
    /// sending anything else that matters.
    pub async fn expect_eof(&mut self) {
        while self.read().await.is_some() {}
    }
}

/// `Sec-WebSocket-Accept` for a client key.
pub fn accept_for(key: &str) -> String {
    let mut input = key.to_string();
    input.push_str("258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    ferroly::hash::base64::encode(&ferroly::hash::sha1(input.as_bytes()).into_bytes())
}
