//! Streamed message bodies with optional trailers: [`BodyStream`] (the
//! reading side) and [`BodySender`] (the writing side), created as a pair by
//! [`body_channel`].
//!
//! They carry a body whose length is not known up front, in either direction:
//!
//! - a server handler reads a streamed **request** body with
//!   [`Request::take_body_stream`](super::Request::take_body_stream) (after
//!   opting in with
//!   [`HttpHandler::wants_body_stream`](super::HttpHandler::wants_body_stream)),
//!   and streams a **response** body with
//!   [`HttpResponse::body_stream`](super::HttpResponse::body_stream);
//! - the client streams a **request** body with
//!   [`Client::send_body_stream`](super::Client::send_body_stream).
//!
//! A body ends cleanly only when the sender says so — with
//! [`finish`](BodySender::finish) or [`send_trailers`](BodySender::send_trailers).
//! Dropping a [`BodySender`] without either aborts the body: the reader gets an
//! error, and on the wire the message is cut off (HTTP/2 resets the stream,
//! HTTP/1.1 closes the connection without the final chunk), so a crashed
//! producer can never look like a complete message.

use tokio::sync::mpsc;

use super::{HeaderMap, HttpError};

/// One item travelling from a [`BodySender`] to its [`BodyStream`].
#[derive(Debug)]
pub(crate) enum Piece {
    Data(Vec<u8>),
    /// A clean end, optionally with trailers.
    End(Option<HeaderMap>),
    /// The body failed with this error (the server reading a streamed
    /// HTTP/1.1 request body: too large, timed out, malformed or cut off).
    Fail(HttpError),
}

fn receiver_gone() -> HttpError {
    HttpError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "the body's reader is gone",
    ))
}

fn aborted() -> HttpError {
    HttpError::Protocol("the body was aborted before it ended".into())
}

/// Creates a connected [`BodySender`] / [`BodyStream`] pair. `capacity` is
/// how many chunks may be queued before [`BodySender::send`] waits for the
/// reader (at least 1).
///
/// ```
/// use ferroly::http::{body_channel, HeaderMap};
///
/// # async fn ex() -> Result<(), ferroly::http::HttpError> {
/// let (mut tx, mut rx) = body_channel(4);
/// tokio::spawn(async move {
///     tx.send(b"hello ".to_vec()).await?;
///     tx.send(b"world".to_vec()).await?;
///     let mut trailers = HeaderMap::new();
///     trailers.set("x-checksum", "abc");
///     tx.send_trailers(trailers).await
/// });
/// let mut body = Vec::new();
/// while let Some(chunk) = rx.chunk().await? {
///     body.extend_from_slice(&chunk);
/// }
/// assert_eq!(body, b"hello world");
/// assert_eq!(rx.trailers().and_then(|t| t.get("x-checksum")), Some("abc"));
/// # Ok(())
/// # }
/// ```
pub fn body_channel(capacity: usize) -> (BodySender, BodyStream) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (BodySender { tx }, BodyStream::new(Inner::Channel(rx)))
}

/// The writing half of a streamed body; see [`body_channel`].
pub struct BodySender {
    tx: mpsc::Sender<Piece>,
}

impl std::fmt::Debug for BodySender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodySender")
            .field("closed", &self.tx.is_closed())
            .finish()
    }
}

impl BodySender {
    /// Sends one chunk, waiting while the queue is full. Empty chunks are
    /// skipped. Fails when the reading side is gone (the peer reset the
    /// stream, the connection closed, or the [`BodyStream`] was dropped).
    pub async fn send(&mut self, chunk: impl Into<Vec<u8>>) -> Result<(), HttpError> {
        let chunk = chunk.into();
        if chunk.is_empty() {
            return if self.tx.is_closed() {
                Err(receiver_gone())
            } else {
                Ok(())
            };
        }
        self.tx
            .send(Piece::Data(chunk))
            .await
            .map_err(|_| receiver_gone())
    }

    /// Ends the body cleanly, without trailers.
    pub async fn finish(self) -> Result<(), HttpError> {
        self.tx
            .send(Piece::End(None))
            .await
            .map_err(|_| receiver_gone())
    }

    /// Ends the body cleanly with a trailer section (sent as trailing
    /// HEADERS on HTTP/2 and as a chunked trailer section on HTTP/1.1).
    pub async fn send_trailers(self, trailers: HeaderMap) -> Result<(), HttpError> {
        self.tx
            .send(Piece::End(Some(trailers)))
            .await
            .map_err(|_| receiver_gone())
    }

    /// Ends the body with `error`, which the reader gets from
    /// [`BodyStream::chunk`] instead of the generic "aborted" error a dropped
    /// sender produces.
    pub(crate) async fn fail(self, error: HttpError) {
        let _ = self.tx.send(Piece::Fail(error)).await;
    }

    /// Whether the reading side is gone, so further sends would fail.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Waits until the reading side is gone (the peer reset the stream, the
    /// connection closed, or the [`BodyStream`] was dropped). Lets a producer
    /// that is idle — waiting for its own input — notice that nobody reads
    /// any more, typically in a `tokio::select!` with that input.
    pub async fn closed(&self) {
        self.tx.closed().await
    }
}

/// Where a [`BodyStream`] reads from.
pub(crate) enum Inner {
    /// A [`body_channel`].
    Channel(mpsc::Receiver<Piece>),
    /// A body that is already in memory (yielded as one chunk).
    Buffered(Option<Vec<u8>>),
    /// The receiving side of an HTTP/2 stream.
    #[cfg(feature = "http2")]
    H2(super::h2::RecvStream),
}

/// The reading half of a streamed body: a sequence of chunks, then (once
/// [`chunk`](Self::chunk) has returned `None`) optional
/// [`trailers`](Self::trailers).
pub struct BodyStream {
    inner: Inner,
    trailers: Option<HeaderMap>,
    /// Trailers already known (an HTTP/1.1 chunked body read in full),
    /// handed out once the body has ended.
    end_trailers: Option<HeaderMap>,
    done: bool,
}

impl std::fmt::Debug for BodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match &self.inner {
            Inner::Channel(_) => "channel",
            Inner::Buffered(_) => "buffered",
            #[cfg(feature = "http2")]
            Inner::H2(_) => "http2",
        };
        f.debug_struct("BodyStream")
            .field("source", &source)
            .field("done", &self.done)
            .field("trailers", &self.trailers)
            .finish()
    }
}

impl BodyStream {
    pub(crate) fn new(inner: Inner) -> Self {
        Self {
            inner,
            trailers: None,
            end_trailers: None,
            done: false,
        }
    }

    /// A stream that yields `body` as a single chunk, then ends.
    pub fn from_bytes(body: impl Into<Vec<u8>>) -> Self {
        let body = body.into();
        Self::new(Inner::Buffered(if body.is_empty() {
            None
        } else {
            Some(body)
        }))
    }

    /// An empty stream.
    pub fn empty() -> Self {
        Self::new(Inner::Buffered(None))
    }

    /// Reads the next chunk, or `None` at the clean end of the body.
    ///
    /// Fails when the body was aborted (the sender was dropped without
    /// finishing, or the peer reset the stream) or the connection failed;
    /// after an error or the end, further calls return the same outcome
    /// (`None` after a clean end).
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        if self.done {
            return Ok(None);
        }
        let result = match &mut self.inner {
            Inner::Buffered(body) => Ok(body.take()),
            Inner::Channel(rx) => match rx.recv().await {
                Some(Piece::Data(chunk)) => Ok(Some(chunk)),
                Some(Piece::End(trailers)) => {
                    self.trailers = trailers;
                    Ok(None)
                }
                Some(Piece::Fail(error)) => Err(error),
                None => Err(aborted()),
            },
            #[cfg(feature = "http2")]
            Inner::H2(rx) => match rx.next().await {
                Ok(super::h2::Chunk::Data(chunk)) => Ok(Some(chunk)),
                Ok(super::h2::Chunk::End(trailers)) => {
                    self.trailers = trailers;
                    Ok(None)
                }
                Err(e) => Err(e),
            },
        };
        match &result {
            Ok(Some(_)) => {}
            Ok(None) => {
                self.done = true;
                if self.trailers.is_none() {
                    self.trailers = self.end_trailers.take();
                }
            }
            Err(_) => {
                self.done = true;
                self.inner = Inner::Buffered(None);
            }
        }
        result
    }

    /// Reads the rest of the body into memory, failing with
    /// [`HttpError::BodyTooLarge`] past `max` bytes.
    pub async fn collect(&mut self, max: usize) -> Result<Vec<u8>, HttpError> {
        let mut out = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            if out.len() + chunk.len() > max {
                return Err(HttpError::BodyTooLarge(max as u64));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// The trailers that ended the body, once [`chunk`](Self::chunk) has
    /// returned `None`; `None` before that or when the body had none.
    pub fn trailers(&self) -> Option<&HeaderMap> {
        self.trailers.as_ref()
    }

    /// Takes the trailers (see [`trailers`](Self::trailers)).
    pub fn take_trailers(&mut self) -> Option<HeaderMap> {
        self.trailers.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clean_end_abort_and_closed_reader() {
        let (mut tx, mut rx) = body_channel(2);
        tx.send(b"a".to_vec()).await.unwrap();
        tx.send(Vec::new()).await.unwrap();
        tx.finish().await.unwrap();
        assert_eq!(rx.chunk().await.unwrap(), Some(b"a".to_vec()));
        assert_eq!(rx.chunk().await.unwrap(), None);
        assert_eq!(rx.chunk().await.unwrap(), None);
        assert!(rx.trailers().is_none());

        // Dropping the sender without finishing is an abort.
        let (mut tx, mut rx) = body_channel(2);
        tx.send(b"partial".to_vec()).await.unwrap();
        drop(tx);
        assert_eq!(rx.chunk().await.unwrap(), Some(b"partial".to_vec()));
        assert!(rx.chunk().await.is_err());
        assert_eq!(rx.chunk().await.unwrap(), None);

        // A dropped reader makes sends fail.
        let (mut tx, rx) = body_channel(0);
        drop(rx);
        assert!(tx.is_closed());
        assert!(tx.send(b"x".to_vec()).await.is_err());
        assert!(tx.send(Vec::new()).await.is_err());
        // `closed` resolves once the reader is gone.
        tx.closed().await;
    }

    #[tokio::test]
    async fn closed_waits_for_the_reader() {
        let (tx, rx) = body_channel(1);
        let waiter = tokio::spawn(async move { tx.closed().await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(rx);
        waiter.await.unwrap();
    }

    #[tokio::test]
    async fn buffered_and_collect() {
        let mut s = BodyStream::from_bytes(b"xyz".to_vec());
        assert_eq!(s.collect(10).await.unwrap(), b"xyz");
        let mut s = BodyStream::from_bytes(b"xyz".to_vec());
        assert!(matches!(
            s.collect(2).await,
            Err(HttpError::BodyTooLarge(2))
        ));
        let mut s = BodyStream::empty();
        assert_eq!(s.chunk().await.unwrap(), None);
        assert!(format!("{s:?}").contains("buffered"));
    }
}
