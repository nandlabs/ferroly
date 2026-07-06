//! WebSocket server: an accept loop performing the in-house upgrade handshake,
//! then driving a per-message handler.
//!
//! Two entry points: the free [`serve`] / [`serve_with_options`] functions run
//! an accept loop directly, and [`WsServer`] wraps the same loop as a lifecycle
//! [`Component`] with graceful shutdown.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use ferroly::http::Conn;
use ferroly::lifecycle::{BoxFuture, Component, LifecycleError};

use super::conn::{read_loop, write_loop, Outgoing, Reader};
use super::{handshake, Message, WsError, WsOptions};

/// Maximum concurrently-served WebSocket connections (backpressure bound).
const MAX_CONNECTIONS: usize = 1024;
/// Deadline for completing the upgrade handshake read; bounds slow-loris.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default per-frame payload cap applied by [`serve`] (16 MiB).
const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
/// Default reassembled-message cap applied by [`serve`] (64 MiB).
const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

type Handler = Arc<dyn Fn(Message) -> Option<Message> + Send + Sync>;

/// The size caps [`serve`] applies by default. Opt out (or tune) by calling
/// [`serve_with_options`] / [`WsServer::with_options`] with your own
/// [`WsOptions`] (where `None` means unbounded).
fn default_capped_options() -> WsOptions {
    WsOptions {
        max_frame_size: Some(DEFAULT_MAX_FRAME_SIZE),
        max_message_size: Some(DEFAULT_MAX_MESSAGE_SIZE),
    }
}

/// Serves WebSocket connections, calling `on_message` for each inbound message
/// and sending back any returned reply. Applies safe default frame/message size
/// caps (16 MiB / 64 MiB); use [`serve_with_options`] to change them. Runs until
/// the listener is dropped.
///
/// ```no_run
/// use ferroly::ws::{server, Message};
/// use tokio::net::TcpListener;
///
/// # async fn ex() -> Result<(), ferroly::ws::WsError> {
/// let listener = TcpListener::bind("127.0.0.1:9001").await.unwrap();
/// server::serve(listener, |msg| match msg {
///     Message::Text(t) => Some(Message::text(format!("echo: {t}"))),
///     Message::Binary(b) => Some(Message::Binary(b)),
/// })
/// .await
/// # }
/// ```
pub async fn serve<F>(listener: TcpListener, on_message: F) -> Result<(), WsError>
where
    F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
{
    serve_with_options(listener, on_message, default_capped_options()).await
}

/// Like [`serve`], but with explicit [`WsOptions`] (frame/message size caps).
/// A `None` cap means unbounded — the explicit opt-out.
pub async fn serve_with_options<F>(
    listener: TcpListener,
    on_message: F,
    options: WsOptions,
) -> Result<(), WsError>
where
    F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
{
    let handler: Handler = Arc::new(on_message);
    accept_loop(listener, handler, options, std::future::pending::<()>()).await
}

/// The shared accept loop: bounded concurrency, resilient accept, and a
/// `shutdown` future that stops accepting when it resolves.
async fn accept_loop<S>(
    listener: TcpListener,
    handler: Handler,
    options: WsOptions,
    shutdown: S,
) -> Result<(), WsError>
where
    S: Future<Output = ()> + Send,
{
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::pin!(shutdown);
    loop {
        let permit = limit
            .clone()
            .acquire_owned()
            .await
            .expect("connection semaphore never closed");
        let accepted = tokio::select! {
            r = listener.accept() => r,
            _ = &mut shutdown => {
                drop(permit);
                return Ok(());
            }
        };
        let (sock, _addr) = match accepted {
            Ok(pair) => pair,
            // Transient accept failures (fd exhaustion, aborted connections)
            // must not tear down the whole listener: back off and continue.
            Err(_e) => {
                drop(permit);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        let handler = handler.clone();
        let options = options.clone();
        tokio::spawn(async move {
            let _ = serve_conn(Box::new(sock) as Conn, handler, options).await;
            drop(permit);
        });
    }
}

async fn serve_conn(conn: Conn, on_message: Handler, options: WsOptions) -> Result<(), WsError> {
    let mut reader = BufReader::new(conn);
    let (_method, _target, headers) = timeout(
        HANDSHAKE_TIMEOUT,
        ferroly::http::io::read_request_head(&mut reader),
    )
    .await
    .map_err(|_| WsError::Connect("handshake read timed out".into()))?
    .map_err(|e| WsError::Connect(e.to_string()))?;

    let key = headers
        .get("sec-websocket-key")
        .ok_or_else(|| WsError::Connect("missing Sec-WebSocket-Key".into()))?;
    let accept = handshake::accept_key(key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    reader
        .write_all(response.as_bytes())
        .await
        .map_err(|e| WsError::Connect(e.to_string()))?;
    reader
        .flush()
        .await
        .map_err(|e| WsError::Connect(e.to_string()))?;

    let leftover = reader.buffer().to_vec();
    let conn = reader.into_inner();
    let (read_half, write_half) = tokio::io::split(conn);

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel();
    // Frame/message size caps come from options (safe defaults via `serve`).
    let reader = Reader::new(read_half, leftover, options.max_frame_size);

    let read_task = tokio::spawn(read_loop(
        reader,
        in_tx,
        out_tx.clone(),
        options.max_message_size,
    ));
    let write_task = tokio::spawn(write_loop(write_half, out_rx, false));

    while let Some(msg) = in_rx.recv().await {
        if let Some(reply) = on_message(msg) {
            if out_tx.send(Outgoing::Msg(reply)).is_err() {
                break;
            }
        }
    }

    let _ = out_tx.send(Outgoing::Close);
    read_task.abort();
    let _ = write_task.await;
    Ok(())
}

// ---- lifecycle Component -------------------------------------------------

struct Running {
    shutdown: oneshot::Sender<()>,
    join: JoinHandle<()>,
}

/// A WebSocket server wrapped as a lifecycle [`Component`], with graceful
/// shutdown. `start` binds the address and runs the accept loop; `stop` signals
/// the loop to stop accepting and awaits it. Applies the same safe default size
/// caps as [`serve`] unless overridden with [`with_options`](Self::with_options).
///
/// ```no_run
/// use ferroly::ws::{Message, WsServer};
/// use ferroly::lifecycle::Component;
///
/// # async fn ex() -> Result<(), ferroly::lifecycle::LifecycleError> {
/// let server = WsServer::new("ws", "127.0.0.1:9001", |msg| match msg {
///     Message::Text(t) => Some(Message::text(format!("echo: {t}"))),
///     Message::Binary(b) => Some(Message::Binary(b)),
/// });
/// server.start().await?;
/// // … later, on shutdown …
/// server.stop().await?;
/// # Ok(())
/// # }
/// ```
pub struct WsServer {
    id: String,
    addr: String,
    options: WsOptions,
    handler: Handler,
    running: tokio::sync::Mutex<Option<Running>>,
    local_addr: std::sync::Mutex<Option<SocketAddr>>,
}

impl WsServer {
    /// Creates a server with `id`, a bind `addr` (`host:port`), and a per-message
    /// handler. Safe default frame/message size caps are applied.
    pub fn new<F>(id: impl Into<String>, addr: impl Into<String>, on_message: F) -> Self
    where
        F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
    {
        Self {
            id: id.into(),
            addr: addr.into(),
            options: default_capped_options(),
            handler: Arc::new(on_message),
            running: tokio::sync::Mutex::new(None),
            local_addr: std::sync::Mutex::new(None),
        }
    }

    /// Overrides the frame/message size caps (a `None` cap means unbounded).
    pub fn with_options(mut self, options: WsOptions) -> Self {
        self.options = options;
        self
    }

    /// The actual bound address once [`start`](Component::start) has run — useful
    /// when binding to port `0`.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        *self.local_addr.lock().unwrap()
    }
}

impl Component for WsServer {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let listener = TcpListener::bind(&self.addr)
                .await
                .map_err(|e| LifecycleError::failure(self.id.clone(), e))?;
            if let Ok(a) = listener.local_addr() {
                *self.local_addr.lock().unwrap() = Some(a);
            }
            let (tx, rx) = oneshot::channel::<()>();
            let handler = self.handler.clone();
            let options = self.options.clone();
            let join = tokio::spawn(async move {
                let _ = accept_loop(listener, handler, options, async move {
                    let _ = rx.await;
                })
                .await;
            });
            *self.running.lock().await = Some(Running { shutdown: tx, join });
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            if let Some(running) = self.running.lock().await.take() {
                let _ = running.shutdown.send(());
                let _ = running.join.await;
            }
            Ok(())
        })
    }
}
