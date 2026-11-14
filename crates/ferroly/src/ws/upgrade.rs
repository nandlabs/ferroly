//! The server side of the opening handshake: request validation, the origin
//! policy, and the hook that lets an application authenticate an upgrade
//! request before the connection becomes a WebSocket.

use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;

use ferroly::http::io::{RequestHead, Version};
use ferroly::http::{split_target, HeaderMap, Method, StatusCode};
use ferroly::lifecycle::BoxFuture;

use super::handshake::{has_token, is_token, list_items, single, valid_client_key};

/// The upgrade request as seen by an [`UpgradeHook`] and, after the upgrade,
/// through [`WsConn::request`](super::WsConn::request).
#[derive(Debug, Clone)]
pub struct UpgradeRequest {
    target: String,
    path: String,
    query: Option<String>,
    host: String,
    headers: HeaderMap,
    peer_addr: Option<SocketAddr>,
    local_addr: Option<SocketAddr>,
    tls: bool,
    peer_certificates: Vec<Vec<u8>>,
    subprotocols: Vec<String>,
}

impl UpgradeRequest {
    /// The request target exactly as sent, in origin form (`/path?query`).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The path part of the target (not percent-decoded).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The raw query string, without the leading `?`.
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// The host the client addressed (the `Host` header, or the authority of
    /// an absolute-form target).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// All request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The first value of a request header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// The `Origin` header, if the client sent one (browsers always do).
    pub fn origin(&self) -> Option<&str> {
        self.headers.get("origin")
    }

    /// The remote address of the connection.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    /// The local address the connection was accepted on.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Whether the connection is `wss` (TLS terminated by this server).
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// The client's verified certificate chain (DER, leaf first) when mutual
    /// TLS is configured and the client presented one; empty otherwise.
    pub fn peer_certificates(&self) -> &[Vec<u8>] {
        &self.peer_certificates
    }

    /// The subprotocols the client offered, in its order of preference.
    pub fn subprotocols(&self) -> &[String] {
        &self.subprotocols
    }
}

/// Facts about the transport an upgrade request arrived on.
pub(crate) struct ConnInfo {
    pub peer_addr: Option<SocketAddr>,
    pub local_addr: Option<SocketAddr>,
    pub tls: bool,
    pub peer_certificates: Vec<Vec<u8>>,
}

/// Accepts an upgrade from an [`UpgradeHook`], optionally attaching
/// per-connection context, choosing the subprotocol, and adding response
/// headers.
#[derive(Default)]
pub struct UpgradeAccept {
    pub(crate) context: Option<Arc<dyn Any + Send + Sync>>,
    pub(crate) subprotocol: Option<String>,
    pub(crate) headers: HeaderMap,
}

impl std::fmt::Debug for UpgradeAccept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeAccept")
            .field("context", &self.context.is_some())
            .field("subprotocol", &self.subprotocol)
            .finish()
    }
}

impl UpgradeAccept {
    /// Accepts the upgrade with no context and the server's default
    /// subprotocol choice.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches a value (for example the authenticated user) that the
    /// connection handler reads back with
    /// [`WsConn::context`](super::WsConn::context).
    pub fn context<T: Any + Send + Sync>(mut self, value: T) -> Self {
        self.context = Some(Arc::new(value));
        self
    }

    /// Selects the subprotocol, overriding the server's configured
    /// preference. It must be one the client offered; otherwise the upgrade
    /// is answered with `500`, because selecting a protocol the client did
    /// not ask for would make it fail the connection.
    pub fn subprotocol(mut self, name: impl Into<String>) -> Self {
        self.subprotocol = Some(name.into());
        self
    }

    /// Adds a header to the `101` response (for example `Set-Cookie`).
    /// Headers the handshake manages itself are ignored.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.append(name, value);
        self
    }
}

/// Rejects an upgrade: the client receives this HTTP response instead of
/// `101 Switching Protocols`, and the connection is closed.
#[derive(Debug, Clone)]
pub struct UpgradeReject {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: String,
}

impl UpgradeReject {
    /// A rejection with `status` and the status text as body. A status below
    /// `300` (which would not be a refusal) is replaced by `403`.
    pub fn new(status: StatusCode) -> Self {
        let status = if status.as_u16() < 300 {
            StatusCode::FORBIDDEN
        } else {
            status
        };
        Self {
            status,
            headers: HeaderMap::new(),
            body: status.reason().to_string(),
        }
    }

    /// `401 Unauthorized`.
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED)
    }

    /// `403 Forbidden`.
    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN)
    }

    /// Adds a response header (for example `WWW-Authenticate`).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.append(name, value);
        self
    }

    /// Replaces the plain-text response body.
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    /// The HTTP status of the rejection.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    fn because(status: StatusCode, why: &str) -> Self {
        Self::new(status).body(why)
    }
}

/// Inspects an upgrade request before the server answers it.
///
/// The hook runs after the request has passed the protocol checks and the
/// [`OriginPolicy`], and before the `101` response is written, so nothing a
/// rejected client sends is ever treated as a WebSocket frame. It is the
/// place to authenticate (a bearer token, a cookie, a client certificate, a
/// ticket in the query string), authorise the path, and attach
/// per-connection context. It runs inside the handshake timeout.
///
/// Closures of the form `Fn(&UpgradeRequest) -> Result<UpgradeAccept,
/// UpgradeReject>` implement this trait; implement it on a type of your own
/// when the decision needs to `await` something.
///
/// ```
/// use ferroly::ws::{UpgradeAccept, UpgradeReject, UpgradeRequest, WsServerConfig};
///
/// struct User(String);
///
/// let config = WsServerConfig::default().on_upgrade(|req: &UpgradeRequest| {
///     match req.header("authorization") {
///         Some("Bearer let-me-in") => Ok(UpgradeAccept::new().context(User("ada".into()))),
///         _ => Err(UpgradeReject::unauthorized().header("WWW-Authenticate", "Bearer")),
///     }
/// });
/// # let _ = config;
/// ```
pub trait UpgradeHook: Send + Sync {
    /// Decides whether `request` may upgrade.
    fn on_upgrade<'a>(
        &'a self,
        request: &'a UpgradeRequest,
    ) -> BoxFuture<'a, Result<UpgradeAccept, UpgradeReject>>;
}

impl<F> UpgradeHook for F
where
    F: Fn(&UpgradeRequest) -> Result<UpgradeAccept, UpgradeReject> + Send + Sync,
{
    fn on_upgrade<'a>(
        &'a self,
        request: &'a UpgradeRequest,
    ) -> BoxFuture<'a, Result<UpgradeAccept, UpgradeReject>> {
        let decision = self(request);
        Box::pin(async move { decision })
    }
}

/// Which `Origin` header values a server accepts.
///
/// Browsers attach the page's origin to every WebSocket handshake and do
/// **not** apply the same-origin policy to WebSockets, so a server that
/// authenticates with cookies (or any other ambient credential) must check
/// the origin itself or any web page can open an authenticated connection
/// (cross-site WebSocket hijacking).
///
/// A request **without** an `Origin` header is always let through by this
/// check: it does not come from a browser, and a non-browser client can put
/// anything in the header anyway. Origin checking is therefore not
/// authentication — use an [`UpgradeHook`] for that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginPolicy {
    /// No origin check at all. Only appropriate when the endpoint does not
    /// rely on browser-held credentials (for example it requires a token in
    /// a header or the query string) or is not reachable from browsers.
    Any,
    /// The default: an `Origin`, when present, must name the same host and
    /// port the request was addressed to (its `Host`). Cross-origin browser
    /// connections are refused with `403`.
    ///
    /// Behind a reverse proxy that rewrites `Host`, same-origin requests
    /// look cross-origin; configure the proxy to pass `Host` through or use
    /// an allow-list.
    SameOrigin,
    /// An `Origin`, when present, must be one of these (for example
    /// `https://app.example.com`), compared case-insensitively and ignoring
    /// a trailing slash. Everything else is refused with `403`.
    AllowList(Vec<String>),
}

impl OriginPolicy {
    /// An allow-list of origins such as `https://app.example.com`.
    pub fn allow_list<I, S>(origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        OriginPolicy::AllowList(origins.into_iter().map(Into::into).collect())
    }

    /// Whether a request with this `Origin` header, addressed to `host`, is
    /// allowed.
    pub fn allows(&self, origin: Option<&str>, host: &str) -> bool {
        let Some(origin) = origin else {
            return true;
        };
        let origin = origin.trim().trim_end_matches('/');
        match self {
            OriginPolicy::Any => true,
            OriginPolicy::SameOrigin => same_origin(origin, host),
            OriginPolicy::AllowList(allowed) => allowed.iter().any(|entry| {
                entry
                    .trim()
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(origin)
            }),
        }
    }
}

/// Whether `origin` (`scheme://host[:port]`) names the authority `host`.
fn same_origin(origin: &str, host: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        // `null` and anything unparseable is never same-origin.
        return false;
    };
    if authority.is_empty() {
        return false;
    }
    if authority.eq_ignore_ascii_case(host) {
        return true;
    }
    // One side may spell out the scheme's default port.
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => ":80",
        "https" | "wss" => ":443",
        _ => return false,
    };
    let strip = |s: &str| {
        s.strip_suffix(default_port)
            .unwrap_or(s)
            .to_ascii_lowercase()
    };
    strip(authority) == strip(host)
}

/// A request that passed every protocol check.
pub(crate) struct Validated {
    pub request: UpgradeRequest,
    /// The client's `Sec-WebSocket-Key`.
    pub key: String,
}

/// Validates an upgrade request (RFC 6455 §4.2.1) and the origin policy.
/// `Err` is the HTTP response to send instead of `101`.
pub(crate) fn validate(
    head: RequestHead,
    info: ConnInfo,
    policy: &OriginPolicy,
) -> Result<Validated, UpgradeReject> {
    let bad = |why: &str| UpgradeReject::because(StatusCode::BAD_REQUEST, why);

    if head.method != Method::Get {
        return Err(UpgradeReject::because(
            StatusCode::METHOD_NOT_ALLOWED,
            "WebSocket upgrade must be a GET",
        )
        .header("Allow", "GET"));
    }
    if head.version != Version::Http11 {
        return Err(bad("WebSocket upgrade requires HTTP/1.1"));
    }
    let headers = &head.headers;
    let host = match (&head.authority, single(headers, "host")) {
        (Some(authority), Ok(_)) => authority.clone(),
        (None, Ok(Some(host))) if !host.is_empty() => host.to_string(),
        _ => return Err(bad("missing or repeated Host header")),
    };
    if !has_token(headers, "upgrade", "websocket") {
        return Err(
            UpgradeReject::because(StatusCode(426), "this endpoint only speaks WebSocket")
                .header("Upgrade", "websocket")
                .header("Connection", "Upgrade"),
        );
    }
    if !has_token(headers, "connection", "upgrade") {
        return Err(bad("Connection header must contain the upgrade token"));
    }
    if single(headers, "sec-websocket-version") != Ok(Some("13")) {
        return Err(
            UpgradeReject::because(StatusCode(426), "unsupported WebSocket version")
                .header("Sec-WebSocket-Version", "13"),
        );
    }
    let key = match single(headers, "sec-websocket-key") {
        Ok(Some(key)) if valid_client_key(key) => key.to_string(),
        _ => return Err(bad("missing or malformed Sec-WebSocket-Key")),
    };
    // An upgrade request has no body; bytes after the head would otherwise
    // be read as frames.
    if headers.contains("transfer-encoding")
        || !matches!(headers.content_length_checked(), Ok(None | Some(0)))
    {
        return Err(bad("WebSocket upgrade request must not have a body"));
    }
    let mut subprotocols = Vec::new();
    for name in list_items(headers, "sec-websocket-protocol") {
        if !is_token(name) {
            return Err(bad("malformed Sec-WebSocket-Protocol"));
        }
        if !subprotocols.iter().any(|seen: &String| seen == name) {
            subprotocols.push(name.to_string());
        }
    }
    let origin = match single(headers, "origin") {
        Ok(origin) => origin,
        Err(()) => {
            return Err(UpgradeReject::because(
                StatusCode::FORBIDDEN,
                "repeated Origin header",
            ))
        }
    };
    if !policy.allows(origin, &host) {
        return Err(UpgradeReject::because(
            StatusCode::FORBIDDEN,
            "origin not allowed",
        ));
    }

    let (path, query) = split_target(&head.target);
    Ok(Validated {
        request: UpgradeRequest {
            target: head.target,
            path,
            query,
            host,
            headers: head.headers,
            peer_addr: info.peer_addr,
            local_addr: info.local_addr,
            tls: info.tls,
            peer_certificates: info.peer_certificates,
            subprotocols,
        },
        key,
    })
}

/// Picks the subprotocol to answer with: the hook's choice if it made one
/// (which must have been offered), otherwise the first server-supported
/// protocol the client offered. `Err(())` means the hook chose a protocol
/// the client did not offer.
pub(crate) fn select_subprotocol(
    offered: &[String],
    supported: &[String],
    chosen: Option<String>,
) -> Result<Option<String>, ()> {
    match chosen {
        Some(choice) => {
            if offered.contains(&choice) {
                Ok(Some(choice))
            } else {
                Err(())
            }
        }
        None => Ok(supported
            .iter()
            .find(|s| offered.iter().any(|o| o == *s))
            .cloned()),
    }
}

/// Response headers a hook may not set on the `101` (or on a rejection):
/// the handshake and the framing own them.
pub(crate) fn is_managed_response_header(name: &str) -> bool {
    const MANAGED: [&str; 8] = [
        "upgrade",
        "connection",
        "sec-websocket-accept",
        "sec-websocket-protocol",
        "sec-websocket-extensions",
        "content-length",
        "transfer-encoding",
        "content-type",
    ];
    MANAGED.iter().any(|m| name.eq_ignore_ascii_case(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(method: Method, version: Version, headers: &[(&str, &str)]) -> RequestHead {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(*name, *value);
        }
        RequestHead {
            method,
            target: "/chat?room=1".into(),
            authority: None,
            version,
            headers: map,
        }
    }

    fn info() -> ConnInfo {
        ConnInfo {
            peer_addr: None,
            local_addr: None,
            tls: false,
            peer_certificates: Vec::new(),
        }
    }

    const GOOD: [(&str, &str); 5] = [
        ("Host", "example.com"),
        ("Upgrade", "websocket"),
        ("Connection", "keep-alive, Upgrade"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];

    fn status(headers: &[(&str, &str)], method: Method, version: Version) -> u16 {
        match validate(
            head(method, version, headers),
            info(),
            &OriginPolicy::SameOrigin,
        ) {
            Ok(_) => 101,
            Err(reject) => reject.status.as_u16(),
        }
    }

    fn without(name: &str) -> Vec<(&'static str, &'static str)> {
        GOOD.iter()
            .copied()
            .filter(|(n, _)| !n.eq_ignore_ascii_case(name))
            .collect()
    }

    #[test]
    fn a_well_formed_request_is_accepted() {
        let v = validate(
            head(Method::Get, Version::Http11, &GOOD),
            info(),
            &OriginPolicy::SameOrigin,
        )
        .unwrap();
        assert_eq!(v.request.path(), "/chat");
        assert_eq!(v.request.query(), Some("room=1"));
        assert_eq!(v.request.host(), "example.com");
        assert_eq!(v.key, "dGhlIHNhbXBsZSBub25jZQ==");
    }

    #[test]
    fn each_handshake_rule_has_its_status() {
        assert_eq!(status(&GOOD, Method::Post, Version::Http11), 405);
        assert_eq!(status(&GOOD, Method::Get, Version::Http10), 400);
        assert_eq!(status(&without("host"), Method::Get, Version::Http11), 400);
        assert_eq!(
            status(&without("upgrade"), Method::Get, Version::Http11),
            426
        );
        assert_eq!(
            status(&without("connection"), Method::Get, Version::Http11),
            400
        );
        assert_eq!(
            status(
                &without("sec-websocket-version"),
                Method::Get,
                Version::Http11
            ),
            426
        );
        assert_eq!(
            status(&without("sec-websocket-key"), Method::Get, Version::Http11),
            400
        );
        let mut v8 = without("sec-websocket-version");
        v8.push(("Sec-WebSocket-Version", "8"));
        assert_eq!(status(&v8, Method::Get, Version::Http11), 426);
        let mut short_key = without("sec-websocket-key");
        short_key.push(("Sec-WebSocket-Key", "c2hvcnQ="));
        assert_eq!(status(&short_key, Method::Get, Version::Http11), 400);
        let mut body = GOOD.to_vec();
        body.push(("Content-Length", "5"));
        assert_eq!(status(&body, Method::Get, Version::Http11), 400);
        let mut bad_proto = GOOD.to_vec();
        bad_proto.push(("Sec-WebSocket-Protocol", "a b"));
        assert_eq!(status(&bad_proto, Method::Get, Version::Http11), 400);
    }

    #[test]
    fn version_mismatch_advertises_the_supported_version() {
        let mut v8 = without("sec-websocket-version");
        v8.push(("Sec-WebSocket-Version", "8"));
        let reject = validate(
            head(Method::Get, Version::Http11, &v8),
            info(),
            &OriginPolicy::Any,
        )
        .err()
        .unwrap();
        assert_eq!(reject.headers.get("sec-websocket-version"), Some("13"));
    }

    #[test]
    fn origin_policies() {
        let same = OriginPolicy::SameOrigin;
        assert!(same.allows(None, "example.com"));
        assert!(same.allows(Some("https://example.com"), "example.com"));
        assert!(same.allows(Some("https://EXAMPLE.com"), "example.com"));
        assert!(same.allows(Some("https://example.com"), "example.com:443"));
        assert!(same.allows(Some("http://example.com:80"), "example.com"));
        assert!(same.allows(Some("http://localhost:8080"), "localhost:8080"));
        assert!(!same.allows(Some("https://evil.example"), "example.com"));
        assert!(!same.allows(Some("https://example.com.evil.example"), "example.com"));
        assert!(!same.allows(Some("http://example.com:8080"), "example.com"));
        assert!(!same.allows(Some("null"), "example.com"));

        let list = OriginPolicy::allow_list(["https://app.example.com", "http://localhost:3000/"]);
        assert!(list.allows(None, "api.example.com"));
        assert!(list.allows(Some("https://app.example.com"), "api.example.com"));
        assert!(list.allows(Some("https://APP.example.com/"), "api.example.com"));
        assert!(list.allows(Some("http://localhost:3000"), "api.example.com"));
        assert!(!list.allows(Some("https://api.example.com"), "api.example.com"));
        assert!(!list.allows(Some("https://evil.example"), "api.example.com"));

        assert!(OriginPolicy::Any.allows(Some("https://evil.example"), "example.com"));
    }

    #[test]
    fn cross_origin_is_rejected_with_403() {
        let mut cross = GOOD.to_vec();
        cross.push(("Origin", "https://evil.example"));
        assert_eq!(status(&cross, Method::Get, Version::Http11), 403);
        let mut own = GOOD.to_vec();
        own.push(("Origin", "https://example.com"));
        assert_eq!(status(&own, Method::Get, Version::Http11), 101);
    }

    #[test]
    fn subprotocol_selection() {
        let offered = vec!["v1".to_string(), "v2".to_string()];
        let supported = vec!["v2".to_string(), "v1".to_string()];
        // Server preference order wins.
        assert_eq!(
            select_subprotocol(&offered, &supported, None),
            Ok(Some("v2".into()))
        );
        assert_eq!(select_subprotocol(&offered, &[], None), Ok(None));
        assert_eq!(select_subprotocol(&[], &supported, None), Ok(None));
        assert_eq!(
            select_subprotocol(&offered, &supported, Some("v1".into())),
            Ok(Some("v1".into()))
        );
        assert_eq!(
            select_subprotocol(&offered, &supported, Some("v9".into())),
            Err(())
        );
    }

    #[test]
    fn a_reject_is_never_a_success_status() {
        assert_eq!(UpgradeReject::new(StatusCode(101)).status().as_u16(), 403);
        assert_eq!(UpgradeReject::new(StatusCode(200)).status().as_u16(), 403);
        assert_eq!(UpgradeReject::unauthorized().status().as_u16(), 401);
    }
}
