//! [`Request`] and [`Response`]: a message (or a [`Streaming`] of
//! messages) with its metadata.
//!
//! [`Streaming`]: super::Streaming

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::metadata::Metadata;

/// What the server knows about the connection a request arrived on.
#[derive(Debug, Clone, Default)]
pub(crate) struct Peer {
    pub(crate) addr: Option<SocketAddr>,
    pub(crate) tls: bool,
    pub(crate) certs: Arc<Vec<Vec<u8>>>,
}

/// A gRPC request: a message (`T` is the request message, or a
/// [`Streaming`](super::Streaming) of them) with its metadata.
///
/// A client builds one with [`Request::new`] (and optionally metadata and a
/// timeout); a server handler receives one with the metadata the client
/// sent, the remaining time from its `grpc-timeout`, and the connection's
/// peer address and TLS state.
///
/// ```
/// use std::time::Duration;
/// use ferroly::grpc::Request;
///
/// let mut req = Request::new("payload").with_timeout(Duration::from_secs(2));
/// req.metadata_mut().insert("x-request-id", "r-1")?;
/// assert_eq!(req.timeout(), Some(Duration::from_secs(2)));
/// assert_eq!(*req.get_ref(), "payload");
/// # Ok::<(), ferroly::grpc::Status>(())
/// ```
#[derive(Debug)]
pub struct Request<T> {
    metadata: Metadata,
    message: T,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    peer: Option<Peer>,
}

impl<T> Request<T> {
    /// A request carrying `message`, without metadata or timeout.
    pub fn new(message: T) -> Self {
        Request {
            metadata: Metadata::new(),
            message,
            timeout: None,
            deadline: None,
            peer: None,
        }
    }

    pub(crate) fn from_server(
        message: T,
        metadata: Metadata,
        timeout: Option<Duration>,
        deadline: Option<Instant>,
        peer: Peer,
    ) -> Self {
        Request {
            metadata,
            message,
            timeout,
            deadline,
            peer: Some(peer),
        }
    }

    /// The request metadata (headers).
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Mutable access to the metadata.
    pub fn metadata_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }

    /// The message.
    pub fn get_ref(&self) -> &T {
        &self.message
    }

    /// Mutable access to the message.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.message
    }

    /// The message, dropping the metadata.
    pub fn into_inner(self) -> T {
        self.message
    }

    /// The metadata and the message.
    pub fn into_parts(self) -> (Metadata, T) {
        (self.metadata, self.message)
    }

    /// The call's timeout. On a client, what [`set_timeout`](Self::set_timeout)
    /// set (sent as `grpc-timeout` and enforced locally); on a server, the
    /// timeout the client sent.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Sets the call's timeout (client side): the call fails with
    /// `DEADLINE_EXCEEDED` once it has run this long, and the server is told
    /// with `grpc-timeout` so it can stop too.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = Some(timeout);
    }

    /// Returns the request with a timeout (see [`set_timeout`](Self::set_timeout)).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// When the call's deadline passes. On a server, from the client's
    /// `grpc-timeout`: the server stops the handler and answers
    /// `DEADLINE_EXCEEDED` then, and long-running work can check this to
    /// give up early. On a client, what [`set_deadline`](Self::set_deadline)
    /// set.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Sets the call's deadline (client side): like
    /// [`set_timeout`](Self::set_timeout), as a point in time. When both are
    /// set the earlier one applies. A deadline already in the past fails the
    /// call with `DEADLINE_EXCEEDED` without sending it.
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    /// Returns the request with a deadline (see [`set_deadline`](Self::set_deadline)).
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Propagates the deadline of an incoming call to this outgoing one: a
    /// handler that calls other services passes its own request here, so
    /// the downstream calls give up when the caller would (no-op when
    /// `incoming` has no deadline; an earlier deadline of this request is
    /// kept).
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use ferroly::grpc::Request;
    ///
    /// // In a handler, `incoming` is the request it received.
    /// let incoming = Request::new(()).with_deadline(Instant::now() + Duration::from_secs(1));
    /// let outgoing = Request::new("downstream").inherit_deadline(&incoming);
    /// assert_eq!(outgoing.deadline(), incoming.deadline());
    /// ```
    #[must_use]
    pub fn inherit_deadline<U>(mut self, incoming: &Request<U>) -> Self {
        if let Some(theirs) = incoming.deadline {
            self.deadline = Some(match self.deadline {
                Some(ours) => ours.min(theirs),
                None => theirs,
            });
        }
        self
    }

    /// The client's address (server side).
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer.as_ref().and_then(|p| p.addr)
    }

    /// Whether the request arrived over TLS (server side).
    pub fn is_tls(&self) -> bool {
        self.peer.as_ref().is_some_and(|p| p.tls)
    }

    /// The client's verified certificate chain (DER, leaf first) on a
    /// mutual-TLS connection (server side); otherwise empty.
    pub fn peer_certificates(&self) -> &[Vec<u8>] {
        match &self.peer {
            Some(p) => p.certs.as_slice(),
            None => &[],
        }
    }

    /// The head of the call (everything but the message) and the message,
    /// for interceptors.
    pub(crate) fn split(self) -> (Request<()>, T) {
        let Request {
            metadata,
            message,
            timeout,
            deadline,
            peer,
        } = self;
        let head = Request {
            metadata,
            message: (),
            timeout,
            deadline,
            peer,
        };
        (head, message)
    }

    /// A request with the same metadata, timeout and peer, carrying `f`
    /// applied to the message.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Request<U> {
        Request {
            metadata: self.metadata,
            message: f(self.message),
            timeout: self.timeout,
            deadline: self.deadline,
            peer: self.peer,
        }
    }
}

/// Converts a message, or a ready [`Request`], into a `Request` — so the
/// generated client methods accept either.
pub trait IntoRequest<T> {
    /// The request.
    fn into_request(self) -> Request<T>;
}

impl<T> IntoRequest<T> for T {
    fn into_request(self) -> Request<T> {
        Request::new(self)
    }
}

impl<T> IntoRequest<T> for Request<T> {
    fn into_request(self) -> Request<T> {
        self
    }
}

/// A gRPC response: a message (or a [`Streaming`](super::Streaming) of
/// them), the response headers' metadata, and — for a single message — the
/// trailing metadata.
///
/// A handler returns `Response::new(message)`, optionally with metadata to
/// send in the response headers ([`metadata_mut`](Self::metadata_mut)) and
/// trailers ([`trailers_mut`](Self::trailers_mut)). A client receives the
/// headers' and trailers' metadata here; for a streamed response the
/// trailers arrive at the end of the stream, through
/// [`Streaming::trailers`](super::Streaming::trailers).
#[derive(Debug)]
pub struct Response<T> {
    metadata: Metadata,
    message: T,
    trailers: Metadata,
}

impl<T> Response<T> {
    /// A response carrying `message`, without metadata.
    pub fn new(message: T) -> Self {
        Response {
            metadata: Metadata::new(),
            message,
            trailers: Metadata::new(),
        }
    }

    pub(crate) fn from_parts(metadata: Metadata, message: T, trailers: Metadata) -> Self {
        Response {
            metadata,
            message,
            trailers,
        }
    }

    /// The response-header metadata.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Mutable access to the response-header metadata.
    pub fn metadata_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }

    /// The trailing metadata (sent with the final status).
    pub fn trailers(&self) -> &Metadata {
        &self.trailers
    }

    /// Mutable access to the trailing metadata.
    pub fn trailers_mut(&mut self) -> &mut Metadata {
        &mut self.trailers
    }

    /// The message.
    pub fn get_ref(&self) -> &T {
        &self.message
    }

    /// Mutable access to the message.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.message
    }

    /// The message, dropping the metadata.
    pub fn into_inner(self) -> T {
        self.message
    }

    /// The header metadata, the message and the trailing metadata.
    pub fn into_parts(self) -> (Metadata, T, Metadata) {
        (self.metadata, self.message, self.trailers)
    }

    /// A response with the same metadata carrying `f` applied to the
    /// message.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Response<U> {
        Response {
            metadata: self.metadata,
            message: f(self.message),
            trailers: self.trailers,
        }
    }
}
