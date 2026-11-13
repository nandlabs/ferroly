//! WebSocket server: an accept loop that validates the opening handshake
//! and hands every upgraded connection to an asynchronous handler.
//!
//! Entry points:
//!
//! - [`serve_with_config`] runs the accept loop with a [`WsHandler`], a
//!   [`WsServerConfig`] and a shutdown future;
//! - [`serve`] / [`serve_with_options`] are the original reply-per-message
//!   shape, kept as thin adapters;
//! - [`WsServer`] wraps the same loop as a lifecycle [`Component`].
//!
//! Every path shuts down the same way: stop accepting, send close `1001`
//! (going away) to each live connection, wait up to
//! [`drain_timeout`](WsServerConfig::drain_timeout) for the close handshakes
//! and handlers to finish, then drop whatever is left.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::{timeout, timeout_at, Instant};

use ferroly::http::io::{read_request_head_with, HeadError};
use ferroly::http::transport::accept_tls;
use ferroly::http::{Conn, HeaderMap, StatusCode};
use ferroly::lifecycle::{BoxFuture, Component, HealthStatus, LifecycleError};

use super::conn::{self, Incoming, Shared, WsSender};
use super::handshake::accept_key;
use super::message::close_code;
use super::observer::WsEvent;
use super::upgrade::{
    is_managed_response_header, select_subprotocol, validate, ConnInfo, UpgradeAccept,
    UpgradeReject, UpgradeRequest, Validated,
};
use super::{Message, WsError, WsOptions, WsServerConfig};

/// How long a refused handshake's error response may take to write.
const REJECT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// After an error response, how long (and how much) unread request input is
/// drained so that closing the socket does not reset the response away.
const LINGER_TIMEOUT: Duration = Duration::from_secs(1);
const LINGER_BYTES: usize = 256 * 1024;

/// Handles one upgraded connection.
///
/// The server calls [`handle`](Self::handle) once per connection, on its own
/// task, with a [`WsConn`]. The connection stays open until the handler
/// returns (it is then closed with code `1000`), the peer closes it, or a
/// limit, timeout or shutdown ends it. A handler that panics closes its
/// connection with code `1011`; other connections are unaffected.
///
/// The connection's slot (see
/// [`max_connections`](WsServerConfig::max_connections)) is held until the
/// handler returns, so a handler must return once
/// [`recv`](WsConn::recv) yields `None` or a send fails.
///
/// Async closures of the form `Fn(WsConn) -> impl Future<Output = ()>`
/// implement this trait:
///
/// ```no_run
/// use ferroly::ws::{server, Message, WsConn, WsServerConfig};
/// use tokio::net::TcpListener;
///
/// # async fn ex() -> Result<(), ferroly::ws::WsError> {
/// let listener = TcpListener::bind("127.0.0.1:9001").await.unwrap();
/// server::serve_with_config(
///     listener,
///     |mut conn: WsConn| async move {
///         while let Some(msg) = conn.recv().await {
///             match msg {
///                 Message::Text(t) => {
///                     if conn.send(Message::text(format!("echo: {t}"))).await.is_err() {
///                         break;
///                     }
///                 }
///                 Message::Binary(_) => {
///                     let _ = conn.close(1003, "text only");
///                 }
///                 Message::Close { .. } => break,
///             }
///         }
///     },
///     std::future::pending::<()>(),
///     WsServerConfig::default(),
/// )
/// .await
/// # }
/// ```
pub trait WsHandler: Send + Sync + 'static {
    /// Runs the connection to completion.
    fn handle(&self, conn: WsConn) -> BoxFuture<'_, ()>;
}

impl<F, Fut> WsHandler for F
where
    F: Fn(WsConn) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn handle(&self, conn: WsConn) -> BoxFuture<'_, ()> {
        Box::pin(self(conn))
    }
}

/// Adapts the original `Fn(Message) -> Option<Message>` callback to a
/// [`WsHandler`]: each text or binary message is passed to the callback and
/// a returned message is sent back.
struct ReplyHandler<F>(F);

impl<F> WsHandler for ReplyHandler<F>
where
    F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
{
    fn handle(&self, mut conn: WsConn) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            while let Some(msg) = conn.recv().await {
                if msg.is_close() {
                    break;
                }
                if let Some(reply) = (self.0)(msg) {
                    if conn.send(reply).await.is_err() {
                        break;
                    }
                }
            }
        })
    }
}

/// One server-side WebSocket connection, as given to a [`WsHandler`].
///
/// It carries the connection's id, the upgrade request (path, query,
/// headers, peer address, TLS state), any context the
/// [`UpgradeHook`](super::UpgradeHook) attached, and the two directions of
/// the connection: [`recv`](Self::recv) for incoming messages and
/// [`send`](Self::send) / [`close`](Self::close) for outgoing ones. Use
/// [`sender`](Self::sender) to get a clonable handle for sending from other
/// tasks (or to register the connection in a [`Hub`](super::Hub)).
pub struct WsConn {
    request: Arc<UpgradeRequest>,
    context: Option<Arc<dyn Any + Send + Sync>>,
    subprotocol: Option<String>,
    sender: WsSender,
    incoming: Incoming,
}

impl std::fmt::Debug for WsConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConn")
            .field("id", &self.id())
            .field("path", &self.request.path())
            .field("peer_addr", &self.request.peer_addr())
            .field("subprotocol", &self.subprotocol)
            .finish()
    }
}

impl WsConn {
    /// The connection id: unique in the process, stable for the life of the
    /// connection.
    pub fn id(&self) -> u64 {
        self.sender.id()
    }

    /// The upgrade request this connection was opened with.
    pub fn request(&self) -> &UpgradeRequest {
        &self.request
    }

    /// The request path (shortcut for `request().path()`).
    pub fn path(&self) -> &str {
        self.request.path()
    }

    /// The request headers (shortcut for `request().headers()`).
    pub fn headers(&self) -> &HeaderMap {
        self.request.headers()
    }

    /// The peer's address (shortcut for `request().peer_addr()`).
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.request.peer_addr()
    }

    /// The context value the [`UpgradeHook`](super::UpgradeHook) attached
    /// with [`UpgradeAccept::context`], if it is a `T`.
    pub fn context<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.context.as_deref().and_then(|any| any.downcast_ref())
    }

    /// The subprotocol negotiated in the handshake, if any.
    pub fn subprotocol(&self) -> Option<&str> {
        self.subprotocol.as_deref()
    }

    /// A clonable handle for sending on this connection from other tasks.
    pub fn sender(&self) -> WsSender {
        self.sender.clone()
    }

    /// Queues a message; see [`WsSender::send`] for the backpressure rules.
    pub async fn send(&self, msg: Message) -> Result<(), WsError> {
        self.sender.send(msg).await
    }

    /// Queues a message without waiting; see [`WsSender::try_send`].
    pub fn try_send(&self, msg: Message) -> Result<(), WsError> {
        self.sender.try_send(msg)
    }

    /// Starts the close handshake; see [`WsSender::close`].
    pub fn close(&self, code: u16, reason: &str) -> Result<(), WsError> {
        self.sender.close(code, reason)
    }

    /// The next incoming message.
    ///
    /// The last message of every connection is a [`Message::Close`] saying
    /// why it ended (the peer's close code, or a locally generated one such
    /// as `1001` on shutdown or `1006` when the transport dropped); after it
    /// this returns `None`.
    pub async fn recv(&mut self) -> Option<Message> {
        self.incoming.recv().await
    }
}

/// Serves WebSocket connections, calling `on_message` for each inbound text
/// or binary message and sending back any returned reply.
///
/// This is the original reply-only shape, kept as a thin adapter over
/// [`serve_with_config`] with [`WsServerConfig::default`]. Runs until the
/// returned future is dropped (live connections then receive close `1001`).
///
/// ```no_run
/// use ferroly::ws::{server, Message};
/// use tokio::net::TcpListener;
///
/// # async fn ex() -> Result<(), ferroly::ws::WsError> {
/// let listener = TcpListener::bind("127.0.0.1:9001").await.unwrap();
/// server::serve(listener, |msg| match msg {
///     Message::Text(t) => Some(Message::text(format!("echo: {t}"))),
///     other => Some(other),
/// })
/// .await
/// # }
/// ```
pub async fn serve<F>(listener: TcpListener, on_message: F) -> Result<(), WsError>
where
    F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
{
    serve_with_config(
        listener,
        ReplyHandler(on_message),
        std::future::pending::<()>(),
        WsServerConfig::default(),
    )
    .await
}

/// Like [`serve`], but with the frame and message size caps taken from
/// `options` (`None` means unbounded). Only
/// [`max_frame_size`](WsOptions::max_frame_size) and
/// [`max_message_size`](WsOptions::max_message_size) are used; for every
/// other setting use [`serve_with_config`].
pub async fn serve_with_options<F>(
    listener: TcpListener,
    on_message: F,
    options: WsOptions,
) -> Result<(), WsError>
where
    F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
{
    serve_with_config(
        listener,
        ReplyHandler(on_message),
        std::future::pending::<()>(),
        with_size_caps(WsServerConfig::default(), &options),
    )
    .await
}

fn with_size_caps(mut config: WsServerConfig, options: &WsOptions) -> WsServerConfig {
    config.max_frame_size = options.max_frame_size.unwrap_or(usize::MAX);
    config.max_message_size = options.max_message_size.unwrap_or(usize::MAX);
    config
}

/// Serves WebSocket connections on `listener` until `shutdown` resolves.
///
/// Each accepted connection performs the TLS handshake (when
/// [`tls`](WsServerConfig::tls) is set), has its upgrade request validated,
/// checked against the [`origin_policy`](WsServerConfig::origin_policy) and
/// passed to the [`upgrade_hook`](WsServerConfig::upgrade_hook), and is then
/// given to `handler` as a [`WsConn`].
///
/// When `shutdown` resolves the server stops accepting, sends close `1001`
/// to every live connection, waits up to
/// [`drain_timeout`](WsServerConfig::drain_timeout) for them to finish, and
/// returns. Dropping the returned future instead triggers the same sequence
/// on a background task.
pub async fn serve_with_config<H, S>(
    listener: TcpListener,
    handler: H,
    shutdown: S,
    config: WsServerConfig,
) -> Result<(), WsError>
where
    H: WsHandler,
    S: Future<Output = ()> + Send,
{
    run(listener, Inner::new(config, Arc::new(handler)), shutdown).await;
    Ok(())
}

// ---- accept loop ---------------------------------------------------------

/// State shared by the accept loop and every connection task.
struct Inner {
    config: WsServerConfig,
    handler: Arc<dyn WsHandler>,
    /// Upgraded connections, so shutdown can close each of them.
    live: Mutex<HashMap<u64, Arc<Shared>>>,
    /// Set to `true` once shutdown starts.
    stopping: watch::Sender<bool>,
}

impl Inner {
    fn new(config: WsServerConfig, handler: Arc<dyn WsHandler>) -> Arc<Self> {
        Arc::new(Self {
            config,
            handler,
            live: Mutex::new(HashMap::new()),
            stopping: watch::channel(false).0,
        })
    }

    fn live(&self) -> MutexGuard<'_, HashMap<u64, Arc<Shared>>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: WsEvent) {
        if let Some(observer) = &self.config.observer {
            observer.on_event(&event);
        }
    }
}

/// Owns the connection tasks. If the accept loop's future is dropped without
/// a shutdown signal, the drain still happens, on a background task.
struct Tasks {
    inner: Arc<Inner>,
    set: Option<JoinSet<()>>,
}

impl Drop for Tasks {
    fn drop(&mut self) {
        let Some(set) = self.set.take() else {
            return;
        };
        if set.is_empty() {
            return;
        }
        // Without a runtime the set is simply dropped, which aborts every
        // connection task.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(drain(self.inner.clone(), set));
        }
    }
}

async fn run<S>(listener: TcpListener, inner: Arc<Inner>, shutdown: S)
where
    S: Future<Output = ()> + Send,
{
    let max = inner
        .config
        .max_connections
        .clamp(1, Semaphore::MAX_PERMITS);
    // A permit is taken *before* accepting, so no more sockets are pulled
    // off the listen queue than can be served.
    let limit = Arc::new(Semaphore::new(max));
    let mut tasks = Tasks {
        inner: inner.clone(),
        set: Some(JoinSet::new()),
    };
    tokio::pin!(shutdown);
    'accept: while let Some(set) = tasks.set.as_mut() {
        let permit = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break 'accept,
                // Reap finished connection tasks as they end.
                Some(_) = set.join_next() => {}
                permit = limit.clone().acquire_owned() => match permit {
                    Ok(permit) => break permit,
                    Err(_) => break 'accept,
                },
            }
        };
        let (sock, peer) = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break 'accept,
                Some(_) = set.join_next() => {}
                accepted = listener.accept() => match accepted {
                    Ok(pair) => break pair,
                    // Transient accept failures (descriptor exhaustion,
                    // aborted connections) must not stop the listener.
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                },
            }
        };
        let _ = sock.set_nodelay(true);
        set.spawn(connection(inner.clone(), sock, peer, permit));
    }
    // Stop accepting before draining.
    drop(listener);
    if let Some(set) = tasks.set.take() {
        drain(inner, set).await;
    }
}

/// Graceful shutdown: tell every live connection the server is going away,
/// wait a bounded time for them to finish, then abort what is left.
async fn drain(inner: Arc<Inner>, mut set: JoinSet<()>) {
    inner.stopping.send_replace(true);
    let live: Vec<Arc<Shared>> = inner.live().values().cloned().collect();
    for conn in live {
        conn.request_close(
            Some(close_code::GOING_AWAY),
            "server shutting down",
            true,
            true,
        );
    }
    let deadline = tokio::time::sleep(inner.config.drain_timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            next = set.join_next() => {
                if next.is_none() {
                    return;
                }
            }
            _ = &mut deadline => break,
        }
    }
    set.shutdown().await;
}

// ---- one connection ------------------------------------------------------

/// Removes a connection from the live table when its task ends.
struct Registered {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.inner.live().remove(&self.id);
    }
}

/// Aborts the handler task if the connection task is itself aborted.
struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

async fn connection(
    inner: Arc<Inner>,
    sock: TcpStream,
    peer: SocketAddr,
    permit: OwnedSemaphorePermit,
) {
    // The slot is released only when everything below is finished.
    let _permit = permit;
    let mut stopping = inner.stopping.subscribe();
    let upgraded = tokio::select! {
        biased;
        // Shutting down: a connection still in its handshake is dropped.
        _ = stopped(&mut stopping) => return,
        upgraded = handshake(&inner, sock, peer) => match upgraded {
            Some(upgraded) => upgraded,
            None => return,
        },
    };

    let (sender, incoming, driver) =
        conn::establish(upgraded.conn, upgraded.leftover, inner.config.conn_params());
    let shared = sender.shared().clone();
    inner.live().insert(shared.id(), shared.clone());
    let _registered = Registered {
        inner: inner.clone(),
        id: shared.id(),
    };
    // Shutdown may have begun between the handshake and the registration,
    // in which case the drain did not see this connection.
    if *stopping.borrow() {
        shared.request_close(
            Some(close_code::GOING_AWAY),
            "server shutting down",
            true,
            true,
        );
    }

    let conn = WsConn {
        request: Arc::new(upgraded.request),
        context: upgraded.context,
        subprotocol: upgraded.subprotocol,
        sender,
        incoming,
    };
    let handler = inner.handler.clone();
    // The handler runs on its own task so that a panic in it is contained.
    let mut handler_task = tokio::spawn(async move { handler.handle(conn).await });
    let _abort = AbortOnDrop(handler_task.abort_handle());

    tokio::pin!(driver);
    tokio::select! {
        biased;
        _ = &mut driver => {
            // The connection is over; the handler sees the end of the
            // stream and returns.
            let _ = (&mut handler_task).await;
        }
        result = &mut handler_task => {
            match result {
                Ok(()) => {
                    shared.request_close(Some(close_code::NORMAL), "", true, false);
                }
                Err(_) => {
                    shared.request_close(
                        Some(close_code::INTERNAL_ERROR),
                        "internal error",
                        false,
                        true,
                    );
                }
            }
            driver.await;
        }
    }
}

struct Upgraded {
    conn: Conn,
    /// Bytes the client sent after its request head: the start of the frame
    /// stream.
    leftover: Vec<u8>,
    request: UpgradeRequest,
    context: Option<Arc<dyn Any + Send + Sync>>,
    subprotocol: Option<String>,
}

/// Runs the opening handshake, bounded as a whole by the handshake timeout.
/// `None` means the connection was refused (and answered) or failed.
async fn handshake(inner: &Inner, sock: TcpStream, peer: SocketAddr) -> Option<Upgraded> {
    let config = &inner.config;
    let deadline = conn::after(Instant::now(), config.handshake_timeout);
    let mut info = ConnInfo {
        peer_addr: Some(peer),
        local_addr: sock.local_addr().ok(),
        tls: config.tls.is_some(),
        peer_certificates: Vec::new(),
    };
    let conn: Conn = match &config.tls {
        Some(tls) => match timeout_at(deadline, accept_tls(sock, tls)).await {
            Ok(Ok((conn, certificates))) => {
                info.peer_certificates = certificates;
                conn
            }
            _ => return None,
        },
        None => Box::new(sock),
    };
    let mut stream = BufReader::new(conn);

    let limits = config.head_limits();
    let mut started = false;
    let read = read_request_head_with(&mut stream, &limits, &mut started);
    let head = match timeout_at(deadline, read).await {
        Ok(Ok(head)) => head,
        Ok(Err(HeadError::Bad { status, reason })) => {
            refuse(inner, &mut stream, UpgradeReject::new(status).body(reason)).await;
            return None;
        }
        Ok(Err(_)) => return None,
        Err(_) => {
            if started {
                let reject =
                    UpgradeReject::new(StatusCode::REQUEST_TIMEOUT).body("request timeout");
                refuse(inner, &mut stream, reject).await;
            }
            return None;
        }
    };

    let Validated { request, key } = match validate(head, info, &config.origin_policy) {
        Ok(validated) => validated,
        Err(reject) => {
            refuse(inner, &mut stream, reject).await;
            return None;
        }
    };

    let accept = match &config.upgrade_hook {
        Some(hook) => match timeout_at(deadline, hook.on_upgrade(&request)).await {
            Ok(Ok(accept)) => accept,
            Ok(Err(reject)) => {
                refuse(inner, &mut stream, reject).await;
                return None;
            }
            Err(_) => {
                let reject = UpgradeReject::new(StatusCode::SERVICE_UNAVAILABLE)
                    .body("upgrade check timed out");
                refuse(inner, &mut stream, reject).await;
                return None;
            }
        },
        None => UpgradeAccept::new(),
    };
    let UpgradeAccept {
        context,
        subprotocol,
        headers,
    } = accept;
    let subprotocol =
        match select_subprotocol(request.subprotocols(), &config.subprotocols, subprotocol) {
            Ok(subprotocol) => subprotocol,
            Err(()) => {
                let reject = UpgradeReject::new(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("selected subprotocol was not offered");
                refuse(inner, &mut stream, reject).await;
                return None;
            }
        };

    let mut response = String::from(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n",
    );
    response.push_str("Sec-WebSocket-Accept: ");
    response.push_str(&accept_key(&key));
    response.push_str("\r\n");
    if let Some(name) = &subprotocol {
        response.push_str("Sec-WebSocket-Protocol: ");
        response.push_str(name);
        response.push_str("\r\n");
    }
    // No `Sec-WebSocket-Extensions`: no extension is implemented, so none is
    // ever accepted.
    for (name, value) in headers.iter() {
        if !is_managed_response_header(name) {
            push_header(&mut response, name, value);
        }
    }
    response.push_str("\r\n");
    let write = async {
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await
    };
    if !matches!(timeout_at(deadline, write).await, Ok(Ok(()))) {
        return None;
    }

    let leftover = stream.buffer().to_vec();
    Some(Upgraded {
        conn: stream.into_inner(),
        leftover,
        request,
        context,
        subprotocol,
    })
}

fn push_header(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str(": ");
    out.push_str(value);
    out.push_str("\r\n");
}

/// Answers a refused upgrade with a plain HTTP error response and closes
/// the connection.
async fn refuse(inner: &Inner, stream: &mut BufReader<Conn>, reject: UpgradeReject) {
    inner.emit(WsEvent::HandshakeRejected {
        status: reject.status.as_u16(),
    });
    let mut response = format!(
        "HTTP/1.1 {} {}\r\n",
        reject.status.as_u16(),
        reject.status.reason()
    );
    // A `426` names the protocol to upgrade to; RFC 9110 §7.8 then requires
    // the `upgrade` connection option alongside it.
    let upgrade = reject.headers.contains("upgrade");
    response.push_str(if upgrade {
        "Connection: Upgrade, close\r\n"
    } else {
        "Connection: close\r\n"
    });
    if !reject.headers.contains("content-type") {
        response.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    }
    response.push_str(&format!("Content-Length: {}\r\n", reject.body.len()));
    for (name, value) in reject.headers.iter() {
        let managed = ["connection", "content-length", "transfer-encoding"]
            .iter()
            .any(|m| name.eq_ignore_ascii_case(m));
        if !managed {
            push_header(&mut response, name, value);
        }
    }
    response.push_str("\r\n");
    response.push_str(&reject.body);

    let write = async {
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        stream.shutdown().await
    };
    if !matches!(timeout(REJECT_WRITE_TIMEOUT, write).await, Ok(Ok(()))) {
        return;
    }
    // Closing a socket that still holds unread input resets the connection,
    // which can destroy the response before the client reads it.
    let _ = timeout(LINGER_TIMEOUT, async {
        let mut seen = 0usize;
        while seen <= LINGER_BYTES {
            match stream.fill_buf().await {
                Ok([]) | Err(_) => break,
                Ok(buf) => {
                    let n = buf.len();
                    stream.consume(n);
                    seen += n;
                }
            }
        }
    })
    .await;
}

// ---- lifecycle Component -------------------------------------------------

struct Running {
    /// Dropping (or firing) this starts the graceful shutdown.
    shutdown: oneshot::Sender<()>,
    join: JoinHandle<()>,
    inner: Arc<Inner>,
}

/// A WebSocket server wrapped as a lifecycle [`Component`].
///
/// `start` binds the address and runs the accept loop; `stop` stops
/// accepting, sends close `1001` to every live connection, waits up to
/// [`drain_timeout`](WsServerConfig::drain_timeout) and returns. The
/// component is restartable (`start` after `stop` binds again), `stop` is
/// safe to call when it is not running, and both are cancel-safe: if the
/// manager abandons a `stop` that is taking too long, the shutdown still
/// completes in the background.
///
/// [`health`](Component::health) reports `Down` if the accept loop has died
/// and `Degraded` while every connection slot is in use.
///
/// ```no_run
/// use ferroly::lifecycle::Component;
/// use ferroly::ws::{Message, WsConn, WsServer, WsServerConfig};
///
/// # async fn ex() -> Result<(), ferroly::lifecycle::LifecycleError> {
/// let server = WsServer::with_handler("ws", "127.0.0.1:9001", |mut conn: WsConn| async move {
///     while let Some(Message::Text(text)) = conn.recv().await {
///         if conn.send(Message::text(text)).await.is_err() {
///             break;
///         }
///     }
/// })
/// .with_config(WsServerConfig {
///     max_connections: 10_000,
///     ..WsServerConfig::default()
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
    config: WsServerConfig,
    handler: Arc<dyn WsHandler>,
    running: Mutex<Option<Running>>,
    local_addr: Mutex<Option<SocketAddr>>,
}

impl WsServer {
    /// Creates a server with `id`, a bind `addr` (`host:port`), and a
    /// reply-per-message callback (the original shape; see [`serve`]).
    pub fn new<F>(id: impl Into<String>, addr: impl Into<String>, on_message: F) -> Self
    where
        F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
    {
        Self::with_handler(id, addr, ReplyHandler(on_message))
    }

    /// Creates a server with `id`, a bind `addr` (`host:port`), and a
    /// per-connection [`WsHandler`].
    pub fn with_handler(
        id: impl Into<String>,
        addr: impl Into<String>,
        handler: impl WsHandler,
    ) -> Self {
        Self {
            id: id.into(),
            addr: addr.into(),
            config: WsServerConfig::default(),
            handler: Arc::new(handler),
            running: Mutex::new(None),
            local_addr: Mutex::new(None),
        }
    }

    /// Overrides the frame and message size caps (a `None` cap means
    /// unbounded). Only those two fields of `options` are used; prefer
    /// [`with_config`](Self::with_config).
    pub fn with_options(mut self, options: WsOptions) -> Self {
        self.config = with_size_caps(self.config, &options);
        self
    }

    /// Replaces the whole server configuration. Takes effect at the next
    /// [`start`](Component::start).
    pub fn with_config(mut self, config: WsServerConfig) -> Self {
        self.config = config;
        self
    }

    /// The actual bound address once [`start`](Component::start) has run —
    /// useful when binding to port `0`. After a restart it is the new
    /// address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        *self.local_addr.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The number of upgraded connections currently open (`0` when the
    /// server is not running).
    pub fn connections(&self) -> usize {
        self.running()
            .as_ref()
            .map_or(0, |running| running.inner.live().len())
    }

    fn running(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_running(&self) -> bool {
        matches!(self.running().as_ref(), Some(running) if !running.join.is_finished())
    }
}

impl Component for WsServer {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            if self.is_running() {
                return Err(LifecycleError::ComponentAlreadyStarted(self.id.clone()));
            }
            // The only await point: if `start` is cancelled here nothing has
            // been stored and the listener is simply dropped.
            let listener = TcpListener::bind(&self.addr)
                .await
                .map_err(|e| LifecycleError::failure(self.id.clone(), e))?;
            let local_addr = listener.local_addr().ok();
            let inner = Inner::new(self.config.clone(), self.handler.clone());
            let (shutdown, signal) = oneshot::channel::<()>();
            let join = tokio::spawn(run(listener, inner.clone(), async move {
                let _ = signal.await;
            }));

            let mut running = self.running();
            if matches!(running.as_ref(), Some(current) if !current.join.is_finished()) {
                // A concurrent `start` won the race: undo this one.
                join.abort();
                return Err(LifecycleError::ComponentAlreadyStarted(self.id.clone()));
            }
            *running = Some(Running {
                shutdown,
                join,
                inner,
            });
            *self.local_addr.lock().unwrap_or_else(|e| e.into_inner()) = local_addr;
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let running = self.running().take();
            if let Some(running) = running {
                let _ = running.shutdown.send(());
                // Bounded by the drain timeout. If this future is dropped
                // while waiting, the accept loop still finishes its drain.
                let _ = running.join.await;
            }
            Ok(())
        })
    }

    fn health(&self) -> Option<HealthStatus> {
        let running = self.running();
        let running = running.as_ref()?;
        if running.join.is_finished() {
            return Some(HealthStatus::Down);
        }
        let saturated = running.inner.live().len() >= running.inner.config.max_connections.max(1);
        Some(if saturated {
            HealthStatus::Degraded
        } else {
            HealthStatus::Up
        })
    }
}
