//! WebSocket client: the opening handshake and a connection handle.

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use ferroly::http::io::read_response_head;
use ferroly::http::transport::{connect_tcp, tls_config, tls_handshake};
use ferroly::http::{Conn, HeaderMap, HttpError, StatusCode, Uri};

use super::conn::{self, Incoming, WsSender};
use super::handshake::{accept_key, encode_key, has_token, is_token, list_items, single};
use super::message::close_code;
use super::{rand, Message, WsError, WsOptions};

/// Request headers the handshake writes itself; a caller may not set them.
const MANAGED_REQUEST_HEADERS: [&str; 10] = [
    "host",
    "upgrade",
    "connection",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
    "sec-websocket-accept",
    "content-length",
    "transfer-encoding",
];

/// A connected WebSocket client.
///
/// A background task drives the connection (reading, writing, answering
/// pings, keepalive, the close handshake), so [`send`](Self::send) and
/// [`recv`](Self::recv) can be used independently; use
/// [`sender`](Self::sender) for a clonable handle to send from other tasks.
///
/// Dropping the client aborts the background task and closes the socket
/// without a close handshake; call [`close`](Self::close) for a clean close.
///
/// ```no_run
/// use std::time::Duration;
/// use ferroly::ws::{Message, WsClient, WsOptions};
///
/// # async fn ex() -> Result<(), ferroly::ws::WsError> {
/// let options = WsOptions::default()
///     .header("Authorization", "Bearer token")
///     .subprotocol("chat.v2")
///     .keepalive(Duration::from_secs(20), Duration::from_secs(10));
/// let mut client = WsClient::dial("wss://example.com/ws", options).await?;
/// client.send(Message::text("hello")).await?;
/// while let Some(msg) = client.recv().await {
///     match msg {
///         Message::Text(text) => println!("{text}"),
///         Message::Binary(bytes) => println!("{} bytes", bytes.len()),
///         Message::Close { code, reason } => println!("closed: {code} {reason}"),
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct WsClient {
    sender: WsSender,
    incoming: Incoming,
    task: JoinHandle<()>,
    subprotocol: Option<String>,
    response_headers: HeaderMap,
}

impl std::fmt::Debug for WsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsClient")
            .field("id", &self.sender.id())
            .field("subprotocol", &self.subprotocol)
            .field("closed", &self.sender.is_closed())
            .finish()
    }
}

impl WsClient {
    /// Connects to `url` (`ws://…` or `wss://…`) and performs the RFC 6455
    /// opening handshake.
    ///
    /// The TCP connect plus TLS handshake is bounded by
    /// [`connect_timeout`](WsOptions::connect_timeout) and the upgrade
    /// exchange by [`handshake_timeout`](WsOptions::handshake_timeout); both
    /// fail with [`WsError::Timeout`]. A response other than `101` fails
    /// with [`WsError::Rejected`], and a `101` that does not carry the
    /// right `Upgrade`, `Connection` and `Sec-WebSocket-Accept` headers —
    /// or that selects a subprotocol or extension that was not offered —
    /// fails with [`WsError::Connect`].
    pub async fn dial(url: &str, opts: WsOptions) -> Result<WsClient, WsError> {
        let uri = Uri::parse(url).map_err(|e| WsError::Connect(e.to_string()))?;
        // A fresh nonce per handshake, independent of any masking key.
        let key = encode_key(&rand::nonce());
        // Built first so that invalid options fail before any I/O.
        let request = build_request(&uri, &key, &opts)?;

        let connect = async {
            let tcp = connect_tcp(&uri.host, uri.port).await?;
            if !uri.is_tls() {
                return Ok::<Conn, HttpError>(Box::new(tcp));
            }
            let tls = match &opts.tls {
                Some(custom) => custom.build()?,
                None => tls_config(),
            };
            tls_handshake(tcp, &uri.host, &tls).await
        };
        let conn = timeout(opts.connect_timeout, connect)
            .await
            .map_err(|_| WsError::Timeout("connect".into()))?
            .map_err(|e| WsError::Connect(e.to_string()))?;

        let mut stream = BufReader::new(conn);
        let exchange = async {
            stream.write_all(request.as_bytes()).await?;
            stream.flush().await?;
            // Strict status line and headers, under the client's default
            // head limits.
            read_response_head(&mut stream).await
        };
        let (status, headers) = timeout(opts.handshake_timeout, exchange)
            .await
            .map_err(|_| WsError::Timeout("handshake".into()))?
            .map_err(|e| WsError::Connect(e.to_string()))?;
        let subprotocol = check_response(status, &headers, &key, &opts.subprotocols)?;

        // Any bytes buffered past the handshake begin the frame stream.
        let leftover = stream.buffer().to_vec();
        let (sender, incoming, driver) =
            conn::establish(stream.into_inner(), leftover, opts.conn_params());
        Ok(WsClient {
            sender,
            incoming,
            task: tokio::spawn(driver),
            subprotocol,
            response_headers: headers,
        })
    }

    /// The connection id (unique in the process).
    pub fn id(&self) -> u64 {
        self.sender.id()
    }

    /// The subprotocol the server selected, if any. It is always one of
    /// [`WsOptions::subprotocols`].
    pub fn subprotocol(&self) -> Option<&str> {
        self.subprotocol.as_deref()
    }

    /// The headers of the server's `101` response.
    pub fn response_headers(&self) -> &HeaderMap {
        &self.response_headers
    }

    /// A clonable handle for sending on this connection from other tasks.
    pub fn sender(&self) -> WsSender {
        self.sender.clone()
    }

    /// Queues a message, waiting while the outgoing queue is full; see
    /// [`WsSender::send`] for the backpressure rules.
    pub async fn send(&self, msg: Message) -> Result<(), WsError> {
        self.sender.send(msg).await
    }

    /// Queues a message without waiting; see [`WsSender::try_send`].
    pub fn try_send(&self, msg: Message) -> Result<(), WsError> {
        self.sender.try_send(msg)
    }

    /// Receives the next message.
    ///
    /// The last message of every connection is a [`Message::Close`] saying
    /// why it ended: the server's close code and reason, or a locally
    /// generated code (`1002`/`1007`/`1009` for a protocol violation by the
    /// server, `1011` for a missed keepalive pong, `1006` when the
    /// transport dropped without a close frame). After it this returns
    /// `None`.
    pub async fn recv(&mut self) -> Option<Message> {
        self.incoming.recv().await
    }

    /// Whether the connection is closed or closing.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// Closes the connection with code `1000` and waits for the close
    /// handshake to finish (bounded by
    /// [`close_timeout`](WsOptions::close_timeout)).
    pub async fn close(self) -> Result<(), WsError> {
        self.close_with(close_code::NORMAL, "").await
    }

    /// Closes the connection with `code` and `reason`, writing the messages
    /// already queued first, and waits for the close handshake to finish.
    /// See [`WsSender::close`] for the accepted codes.
    pub async fn close_with(mut self, code: u16, reason: &str) -> Result<(), WsError> {
        self.sender.close(code, reason)?;
        // Borrow rather than move the handle so `Drop` can still run.
        let _ = (&mut self.task).await;
        Ok(())
    }
}

impl Drop for WsClient {
    /// Aborts the background task so that a socket cannot leak when the
    /// client is dropped without [`close`](WsClient::close).
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Builds the upgrade request, refusing headers and subprotocol names that
/// would corrupt or contradict the handshake.
fn build_request(uri: &Uri, key: &str, opts: &WsOptions) -> Result<String, WsError> {
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        uri.request_target(),
        uri.authority(),
        key,
    );
    if !opts.subprotocols.is_empty() {
        if let Some(bad) = opts.subprotocols.iter().find(|name| !is_token(name)) {
            return Err(WsError::Connect(format!(
                "invalid subprotocol name: {bad:?}"
            )));
        }
        request.push_str("Sec-WebSocket-Protocol: ");
        request.push_str(&opts.subprotocols.join(", "));
        request.push_str("\r\n");
    }
    for (name, value) in opts.headers.iter() {
        if MANAGED_REQUEST_HEADERS
            .iter()
            .any(|managed| name.eq_ignore_ascii_case(managed))
        {
            return Err(WsError::Connect(format!(
                "the {name} header is set by the handshake and cannot be overridden"
            )));
        }
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    Ok(request)
}

/// Validates the server's handshake response (RFC 6455 §4.1) and returns
/// the selected subprotocol.
fn check_response(
    status: StatusCode,
    headers: &HeaderMap,
    key: &str,
    offered: &[String],
) -> Result<Option<String>, WsError> {
    let invalid = |why: &str| WsError::Connect(format!("invalid handshake response: {why}"));
    if status.as_u16() != 101 {
        return Err(WsError::Rejected {
            status: status.as_u16(),
        });
    }
    if !has_token(headers, "upgrade", "websocket") {
        return Err(invalid("missing `Upgrade: websocket`"));
    }
    if !has_token(headers, "connection", "upgrade") {
        return Err(invalid("missing `Connection: Upgrade`"));
    }
    if single(headers, "sec-websocket-accept") != Ok(Some(accept_key(key).as_str())) {
        return Err(invalid("wrong Sec-WebSocket-Accept"));
    }
    // No extension is ever offered, so the server may not select one.
    if list_items(headers, "sec-websocket-extensions")
        .next()
        .is_some()
    {
        return Err(invalid(
            "the server selected an extension that was not offered",
        ));
    }
    match single(headers, "sec-websocket-protocol") {
        Ok(None) => Ok(None),
        Ok(Some(selected)) if offered.iter().any(|name| name == selected) => {
            Ok(Some(selected.to_string()))
        }
        _ => Err(invalid(
            "the server selected a subprotocol that was not offered",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

    fn check(
        status: u16,
        headers: &[(&str, &str)],
        offered: &[String],
    ) -> Result<Option<String>, WsError> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(*name, *value);
        }
        check_response(StatusCode(status), &map, KEY, offered)
    }

    fn good() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Upgrade", "WebSocket"),
            ("Connection", "upgrade"),
            ("Sec-WebSocket-Accept", ACCEPT),
        ]
    }

    #[test]
    fn accepts_a_correct_101() {
        assert_eq!(check(101, &good(), &[]).unwrap(), None);
    }

    #[test]
    fn rejects_incomplete_or_wrong_101_responses() {
        for missing in ["upgrade", "connection", "sec-websocket-accept"] {
            let headers: Vec<_> = good()
                .into_iter()
                .filter(|(name, _)| !name.eq_ignore_ascii_case(missing))
                .collect();
            assert!(
                matches!(check(101, &headers, &[]), Err(WsError::Connect(_))),
                "missing {missing}"
            );
        }
        let mut wrong = good();
        wrong[2] = ("Sec-WebSocket-Accept", "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert!(check(101, &wrong, &[]).is_err());
        assert!(matches!(
            check(200, &good(), &[]),
            Err(WsError::Rejected { status: 200 })
        ));
    }

    #[test]
    fn subprotocol_and_extension_must_have_been_offered() {
        let offered = vec!["chat".to_string()];
        let mut chat = good();
        chat.push(("Sec-WebSocket-Protocol", "chat"));
        assert_eq!(
            check(101, &chat, &offered).unwrap(),
            Some("chat".to_string())
        );
        // Selected although nothing (or something else) was offered.
        assert!(check(101, &chat, &[]).is_err());
        let mut other = good();
        other.push(("Sec-WebSocket-Protocol", "mqtt"));
        assert!(check(101, &other, &offered).is_err());

        let mut ext = good();
        ext.push(("Sec-WebSocket-Extensions", "permessage-deflate"));
        assert!(check(101, &ext, &offered).is_err());
    }

    #[test]
    fn request_carries_custom_headers_and_refuses_managed_ones() {
        let uri = Uri::parse("ws://example.com:8080/chat?x=1").unwrap();
        let opts = WsOptions::default()
            .header("Authorization", "Bearer abc")
            .subprotocol("v2")
            .subprotocol("v1");
        let request = build_request(&uri, KEY, &opts).unwrap();
        assert!(request.starts_with("GET /chat?x=1 HTTP/1.1\r\nHost: example.com:8080\r\n"));
        assert!(request.contains("\r\nAuthorization: Bearer abc\r\n"));
        assert!(request.contains("\r\nSec-WebSocket-Protocol: v2, v1\r\n"));
        assert!(request.ends_with("\r\n\r\n"));

        for managed in ["Host", "Sec-WebSocket-Key", "Connection", "Upgrade"] {
            let opts = WsOptions::default().header(managed, "x");
            assert!(build_request(&uri, KEY, &opts).is_err(), "{managed}");
        }
        let opts = WsOptions::default().subprotocol("not a token");
        assert!(build_request(&uri, KEY, &opts).is_err());
        // A header value can never split the request.
        let opts = WsOptions::default().header("X-Test", "a\r\nInjected: 1");
        let request = build_request(&uri, KEY, &opts).unwrap();
        assert!(!request.contains("\r\nInjected"));
    }
}
