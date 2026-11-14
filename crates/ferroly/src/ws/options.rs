//! Client options and server configuration.

use std::sync::Arc;
use std::time::Duration;

use ferroly::http::io::HeadLimits;
use ferroly::http::{ClientTlsConfig, HeaderMap, TlsConfig};

use super::conn::ConnParams;
use super::observer::WsObserver;
use super::upgrade::{OriginPolicy, UpgradeAccept, UpgradeHook, UpgradeReject, UpgradeRequest};

/// Default per-frame payload cap (16 MiB).
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
/// Default reassembled-message cap (64 MiB).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

const DEFAULT_SEND_QUEUE: usize = 64;
const DEFAULT_RECV_QUEUE: usize = 16;
const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_PONG_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Options for [`WsClient::dial`](super::WsClient::dial).
///
/// Fields are public — build a value with struct-update syntax
/// (`WsOptions { ping_interval: Some(..), ..WsOptions::default() }`) or the
/// chaining helpers ([`header`](Self::header),
/// [`subprotocol`](Self::subprotocol), …). New fields are added over time, so
/// always end a struct literal with `..WsOptions::default()`.
///
/// The defaults are finite: a 16 MiB frame cap, a 64 MiB message cap, a 10 s
/// connect timeout and a 10 s handshake timeout.
#[derive(Clone)]
pub struct WsOptions {
    /// Maximum accepted reassembled message size in bytes. A larger message
    /// fails the connection with close code `1009`. `None` removes the cap
    /// (not recommended: the peer then controls this process's memory).
    /// Default: 64 MiB.
    pub max_message_size: Option<usize>,
    /// Maximum accepted single-frame payload size in bytes; enforced from the
    /// frame header, before the payload is buffered. `None` removes the cap.
    /// Default: 16 MiB.
    pub max_frame_size: Option<usize>,
    /// Deadline for the TCP connect plus (for `wss`) the TLS handshake.
    /// Default: 10 s.
    pub connect_timeout: Duration,
    /// Deadline for sending the upgrade request and receiving the `101`
    /// response. Default: 10 s.
    pub handshake_timeout: Duration,
    /// Extra request headers for the upgrade request, for example
    /// `Authorization` or `Origin`. Headers the handshake manages itself
    /// (`Host`, `Upgrade`, `Connection`, `Sec-WebSocket-*`, `Content-Length`,
    /// `Transfer-Encoding`) are refused.
    pub headers: HeaderMap,
    /// Subprotocols to offer in `Sec-WebSocket-Protocol`, in order of
    /// preference. The server may select one of them (see
    /// [`WsClient::subprotocol`](super::WsClient::subprotocol)); selecting
    /// anything else fails the handshake.
    pub subprotocols: Vec<String>,
    /// TLS settings for `wss` (custom roots, a client certificate, a minimum
    /// version). `None` uses the bundled web PKI roots.
    pub tls: Option<ClientTlsConfig>,
    /// Send a keepalive ping this often. `None` (the default) sends none.
    pub ping_interval: Option<Duration>,
    /// How long to wait for the pong answering a keepalive ping before the
    /// connection is closed with code `1011`. Default: 10 s.
    pub pong_timeout: Duration,
    /// Close the connection (code `1001`) when no frame of any kind has
    /// arrived for this long. `None` (the default) never does.
    pub idle_timeout: Option<Duration>,
    /// Deadline for the peer to accept one written frame. When it expires
    /// the connection is dropped. `None` waits forever. Default: 30 s.
    pub write_timeout: Option<Duration>,
    /// How long the close handshake may take before the connection is
    /// dropped. Default: 5 s.
    pub close_timeout: Duration,
    /// Capacity of the outgoing message queue. Default: 64 messages.
    pub send_queue_capacity: usize,
    /// Capacity of the incoming message queue. When it is full the client
    /// stops reading from the socket until the application catches up.
    /// Default: 16 messages.
    pub recv_queue_capacity: usize,
    /// How long [`send`](super::WsSender::send) waits for space in a full
    /// outgoing queue before the connection is closed with code `1008`.
    /// Default: 10 s.
    pub send_timeout: Duration,
    /// Receives connection events for metrics.
    pub observer: Option<Arc<dyn WsObserver>>,
}

impl Default for WsOptions {
    fn default() -> Self {
        Self {
            max_message_size: Some(DEFAULT_MAX_MESSAGE_SIZE),
            max_frame_size: Some(DEFAULT_MAX_FRAME_SIZE),
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            headers: HeaderMap::new(),
            subprotocols: Vec::new(),
            tls: None,
            ping_interval: None,
            pong_timeout: DEFAULT_PONG_TIMEOUT,
            idle_timeout: None,
            write_timeout: Some(DEFAULT_WRITE_TIMEOUT),
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
            send_queue_capacity: DEFAULT_SEND_QUEUE,
            recv_queue_capacity: DEFAULT_RECV_QUEUE,
            send_timeout: DEFAULT_SEND_TIMEOUT,
            observer: None,
        }
    }
}

impl std::fmt::Debug for WsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Header values may hold credentials: only the names are printed.
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name).collect();
        f.debug_struct("WsOptions")
            .field("max_message_size", &self.max_message_size)
            .field("max_frame_size", &self.max_frame_size)
            .field("connect_timeout", &self.connect_timeout)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("headers", &header_names)
            .field("subprotocols", &self.subprotocols)
            .field("tls", &self.tls)
            .field("ping_interval", &self.ping_interval)
            .field("pong_timeout", &self.pong_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("write_timeout", &self.write_timeout)
            .field("close_timeout", &self.close_timeout)
            .field("send_queue_capacity", &self.send_queue_capacity)
            .field("recv_queue_capacity", &self.recv_queue_capacity)
            .field("send_timeout", &self.send_timeout)
            .field("observer", &self.observer.is_some())
            .finish()
    }
}

impl WsOptions {
    /// Adds a request header to the upgrade request (see
    /// [`headers`](Self::headers)).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.append(name, value);
        self
    }

    /// Offers a subprotocol (see [`subprotocols`](Self::subprotocols)).
    pub fn subprotocol(mut self, name: impl Into<String>) -> Self {
        self.subprotocols.push(name.into());
        self
    }

    /// Sets the TLS configuration used for `wss`.
    pub fn tls(mut self, tls: ClientTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Enables keepalive pings every `interval`, with `pong_timeout` to
    /// answer each.
    pub fn keepalive(mut self, interval: Duration, pong_timeout: Duration) -> Self {
        self.ping_interval = Some(interval);
        self.pong_timeout = pong_timeout;
        self
    }

    /// Sets the observer that receives connection events.
    pub fn observer(mut self, observer: Arc<dyn WsObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub(crate) fn conn_params(&self) -> ConnParams {
        ConnParams {
            server: false,
            max_frame_size: self.max_frame_size.unwrap_or(usize::MAX),
            max_message_size: self.max_message_size.unwrap_or(usize::MAX),
            idle_timeout: self.idle_timeout,
            write_timeout: self.write_timeout,
            ping_interval: self.ping_interval,
            pong_timeout: self.pong_timeout,
            close_timeout: self.close_timeout,
            send_timeout: self.send_timeout,
            send_queue: self.send_queue_capacity,
            recv_queue: self.recv_queue_capacity,
            observer: self.observer.clone(),
        }
    }
}

/// Limits, timeouts and policies for a WebSocket server.
///
/// Fields are public; build a value with struct-update syntax
/// (`WsServerConfig { max_connections: 10_000, ..WsServerConfig::default() }`)
/// or the chaining helpers. New fields are added over time, so always end a
/// struct literal with `..WsServerConfig::default()`.
///
/// ```
/// use std::time::Duration;
/// use ferroly::ws::{OriginPolicy, WsServerConfig};
///
/// let config = WsServerConfig {
///     max_connections: 10_000,
///     max_message_size: 1024 * 1024,
///     idle_timeout: Some(Duration::from_secs(60)),
///     ..WsServerConfig::default()
/// }
/// .origin_policy(OriginPolicy::allow_list(["https://app.example.com"]))
/// .subprotocols(["chat.v2", "chat.v1"]);
/// # let _ = config;
/// ```
#[derive(Clone)]
pub struct WsServerConfig {
    /// Maximum number of connections served at once (including ones still
    /// in the handshake). Further connections wait in the listen backlog
    /// until a slot frees. Default: 1024.
    pub max_connections: usize,
    /// Maximum accepted single-frame payload in bytes (close code `1009`);
    /// enforced from the frame header. `usize::MAX` removes the cap.
    /// Default: 16 MiB.
    pub max_frame_size: usize,
    /// Maximum accepted reassembled message in bytes (close code `1009`).
    /// `usize::MAX` removes the cap. Default: 64 MiB.
    pub max_message_size: usize,
    /// Deadline for the whole opening handshake: the TLS handshake, reading
    /// the request head, the upgrade hook and writing the response. Bounds
    /// slow-loris attacks. Default: 30 s.
    pub handshake_timeout: Duration,
    /// Close a connection (code `1001`) when no frame of any kind — data,
    /// ping or pong — has arrived for this long. `None` disables it.
    /// Default: 120 s.
    pub idle_timeout: Option<Duration>,
    /// Deadline for the peer to accept one written frame; a client that
    /// stops reading is dropped when it expires. `None` waits forever.
    /// Default: 30 s.
    pub write_timeout: Option<Duration>,
    /// Send a keepalive ping to every connection this often. `None` disables
    /// keepalive. Default: 30 s.
    pub ping_interval: Option<Duration>,
    /// How long a client has to answer a keepalive ping before it is closed
    /// with code `1011`. Default: 10 s.
    pub pong_timeout: Duration,
    /// How long a close handshake may take before the connection is dropped.
    /// Default: 5 s.
    pub close_timeout: Duration,
    /// On shutdown, how long to wait for connections to finish their close
    /// handshake (and handlers to return) before they are dropped. Default:
    /// 10 s.
    pub drain_timeout: Duration,
    /// Capacity of each connection's outgoing message queue. Default: 64.
    pub send_queue_capacity: usize,
    /// Capacity of each connection's incoming message queue; when full the
    /// server stops reading from that client. Default: 16.
    pub recv_queue_capacity: usize,
    /// How long [`send`](super::WsSender::send) waits for space in a full
    /// outgoing queue before the client is closed with code `1008` as a slow
    /// consumer. Default: 10 s.
    pub send_timeout: Duration,
    /// Which `Origin` values may connect. Default:
    /// [`OriginPolicy::SameOrigin`].
    pub origin_policy: OriginPolicy,
    /// Subprotocols this server speaks, in order of preference. The first
    /// one the client also offered is selected. Default: none.
    pub subprotocols: Vec<String>,
    /// Maximum total size of the upgrade request's header section (`431`).
    /// Default: 64 KiB.
    pub max_header_bytes: usize,
    /// Maximum number of header lines in the upgrade request (`431`).
    /// Default: 128.
    pub max_headers: usize,
    /// Maximum length of the request line (`414`) or one header line
    /// (`431`). Default: 16 KiB.
    pub max_line_bytes: usize,
    /// Serve `wss`: every accepted connection performs a TLS handshake with
    /// this configuration first (mutual TLS included, if it is configured).
    /// Default: `None` (plain `ws`).
    pub tls: Option<TlsConfig>,
    /// Called with every valid upgrade request before the `101` is sent; may
    /// reject it or attach per-connection context. Default: none.
    pub upgrade_hook: Option<Arc<dyn UpgradeHook>>,
    /// Receives connection and handshake events for metrics.
    pub observer: Option<Arc<dyn WsObserver>>,
}

impl Default for WsServerConfig {
    fn default() -> Self {
        let head = HeadLimits::server();
        Self {
            max_connections: 1024,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            handshake_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(120)),
            write_timeout: Some(DEFAULT_WRITE_TIMEOUT),
            ping_interval: Some(Duration::from_secs(30)),
            pong_timeout: DEFAULT_PONG_TIMEOUT,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
            drain_timeout: Duration::from_secs(10),
            send_queue_capacity: DEFAULT_SEND_QUEUE,
            recv_queue_capacity: DEFAULT_RECV_QUEUE,
            send_timeout: DEFAULT_SEND_TIMEOUT,
            origin_policy: OriginPolicy::SameOrigin,
            subprotocols: Vec::new(),
            max_header_bytes: head.max_header_bytes,
            max_headers: head.max_headers,
            max_line_bytes: head.max_line_bytes,
            tls: None,
            upgrade_hook: None,
            observer: None,
        }
    }
}

impl std::fmt::Debug for WsServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsServerConfig")
            .field("max_connections", &self.max_connections)
            .field("max_frame_size", &self.max_frame_size)
            .field("max_message_size", &self.max_message_size)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("write_timeout", &self.write_timeout)
            .field("ping_interval", &self.ping_interval)
            .field("pong_timeout", &self.pong_timeout)
            .field("close_timeout", &self.close_timeout)
            .field("drain_timeout", &self.drain_timeout)
            .field("send_queue_capacity", &self.send_queue_capacity)
            .field("recv_queue_capacity", &self.recv_queue_capacity)
            .field("send_timeout", &self.send_timeout)
            .field("origin_policy", &self.origin_policy)
            .field("subprotocols", &self.subprotocols)
            .field("max_header_bytes", &self.max_header_bytes)
            .field("max_headers", &self.max_headers)
            .field("max_line_bytes", &self.max_line_bytes)
            .field("tls", &self.tls.is_some())
            .field("upgrade_hook", &self.upgrade_hook.is_some())
            .field("observer", &self.observer.is_some())
            .finish()
    }
}

impl WsServerConfig {
    /// Serves `wss` with this TLS configuration.
    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Sets the origin policy.
    pub fn origin_policy(mut self, policy: OriginPolicy) -> Self {
        self.origin_policy = policy;
        self
    }

    /// Sets the subprotocols this server speaks, in order of preference.
    pub fn subprotocols<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.subprotocols = names.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the hook that inspects every upgrade request; see
    /// [`UpgradeHook`].
    pub fn upgrade_hook(mut self, hook: impl UpgradeHook + 'static) -> Self {
        self.upgrade_hook = Some(Arc::new(hook));
        self
    }

    /// Sets the upgrade hook from a closure. Same as
    /// [`upgrade_hook`](Self::upgrade_hook), but lets the compiler infer the
    /// closure's argument type.
    pub fn on_upgrade<F>(self, hook: F) -> Self
    where
        F: Fn(&UpgradeRequest) -> Result<UpgradeAccept, UpgradeReject> + Send + Sync + 'static,
    {
        self.upgrade_hook(hook)
    }

    /// Sets the observer that receives connection and handshake events.
    pub fn observer(mut self, observer: Arc<dyn WsObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub(crate) fn head_limits(&self) -> HeadLimits {
        HeadLimits {
            max_line_bytes: self.max_line_bytes,
            max_headers: self.max_headers,
            max_header_bytes: self.max_header_bytes,
            allow_bare_lf: false,
        }
    }

    pub(crate) fn conn_params(&self) -> ConnParams {
        ConnParams {
            server: true,
            max_frame_size: self.max_frame_size,
            max_message_size: self.max_message_size,
            idle_timeout: self.idle_timeout,
            write_timeout: self.write_timeout,
            ping_interval: self.ping_interval,
            pong_timeout: self.pong_timeout,
            close_timeout: self.close_timeout,
            send_timeout: self.send_timeout,
            send_queue: self.send_queue_capacity,
            recv_queue: self.recv_queue_capacity,
            observer: self.observer.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_finite() {
        let o = WsOptions::default();
        assert_eq!(o.max_frame_size, Some(DEFAULT_MAX_FRAME_SIZE));
        assert_eq!(o.max_message_size, Some(DEFAULT_MAX_MESSAGE_SIZE));
        assert!(o.connect_timeout > Duration::ZERO);
        assert!(o.handshake_timeout > Duration::ZERO);
        let s = WsServerConfig::default();
        assert!(s.max_frame_size < usize::MAX && s.max_message_size < usize::MAX);
        assert!(s.idle_timeout.is_some() && s.write_timeout.is_some());
    }

    #[test]
    fn debug_never_prints_header_values() {
        let o = WsOptions::default().header("Authorization", "Bearer s3cret");
        let debug = format!("{o:?}");
        assert!(debug.contains("Authorization"));
        assert!(!debug.contains("s3cret"));
    }
}
