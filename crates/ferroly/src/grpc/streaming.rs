//! [`Streaming`]: a typed stream of messages, read from the wire or fed by a
//! [`StreamSender`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::frame::{decode_message, encode_frame, Encoding, FrameDecoder};
use super::metadata::Metadata;
use super::status::Status;
use crate::http::{BodySender, BodyStream, BoxFuture};
use crate::proto::{Limits, Message};

/// A stream of messages of type `M`.
///
/// It is both what a call **receives** — the request stream of a client- or
/// bidirectional-streaming handler, the response stream a client reads — and
/// what a side **sends**: create one with [`Streaming::channel`] (and feed it
/// from a [`StreamSender`]) or [`Streaming::from_messages`], and hand it to
/// the client call or return it from the handler.
///
/// Read with [`message`](Self::message) until it returns `Ok(None)` (or with
/// [`next`](Self::next), which has the shape of `Stream::next`); collect a
/// whole stream with [`collect`](Self::collect). [`poll_message`](Self::poll_message)
/// is the poll-based form, for adapting a `Streaming` to any `Stream` trait
/// without this crate depending on one:
///
/// ```
/// use std::pin::Pin;
/// use std::task::{Context, Poll};
/// use ferroly::grpc::{Status, Streaming};
///
/// // A local stand-in for `futures::Stream`.
/// trait Stream {
///     type Item;
///     fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>;
/// }
///
/// struct Adapter<M>(Streaming<M>);
///
/// impl<M: ferroly::proto::Message + Send + 'static> Stream for Adapter<M> {
///     type Item = Result<M, Status>;
///     fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
///         self.0.poll_message(cx).map(Result::transpose)
///     }
/// }
/// ```
///
/// A full example:
///
/// ```
/// use ferroly::grpc::{Status, Streaming};
///
/// #[derive(ferroly::proto::Message, Default)]
/// struct Tick {
///     #[proto(tag = 1)]
///     n: u32,
/// }
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<(), Status> {
/// let (tx, mut stream) = Streaming::channel(4);
/// tokio::spawn(async move {
///     for n in 0..3 {
///         tx.send(Tick { n }).await?; // fails once the reader is gone
///     }
///     tx.finish().await // the clean end
/// });
/// let mut got = Vec::new();
/// while let Some(tick) = stream.message().await? {
///     got.push(tick.n);
/// }
/// assert_eq!(got, [0, 1, 2]);
/// # Ok(())
/// # }
/// ```
pub struct Streaming<M> {
    inner: Inner<M>,
    trailers: Option<Metadata>,
    /// A read started by [`poll_message`](Self::poll_message) and not yet
    /// finished: it owns the source until it completes.
    polling: Option<Polling<M>>,
}

/// What a read in progress gives back: the source, the trailers and the
/// result.
type Polled<M> = (Inner<M>, Option<Metadata>, Result<Option<M>, Status>);
type Polling<M> = Pin<Box<dyn Future<Output = Polled<M>> + Send>>;

/// What travels from a [`StreamSender`] to its [`Streaming`].
enum Item<M> {
    Message(M),
    End,
    Fail(Status),
}

enum Inner<M> {
    Channel(mpsc::Receiver<Item<M>>),
    Messages(Box<dyn Iterator<Item = M> + Send>),
    Wire(Box<Wire>),
    /// Ended; a status still to report once, if any.
    Done(Option<Status>),
}

impl<M> fmt::Debug for Streaming<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match &self.inner {
            Inner::Channel(_) => "channel",
            Inner::Messages(_) => "messages",
            Inner::Wire(_) => "wire",
            Inner::Done(_) => "done",
        };
        f.debug_struct("Streaming")
            .field("source", &source)
            .field("trailers", &self.trailers)
            .finish()
    }
}

impl<M> Streaming<M> {
    /// A connected [`StreamSender`] / `Streaming` pair; `capacity` messages
    /// (at least 1) may be queued before [`StreamSender::send`] waits.
    ///
    /// The stream ends cleanly only when the sender says so with
    /// [`finish`](StreamSender::finish), or with an error through
    /// [`fail`](StreamSender::fail). A sender dropped without either — its
    /// task panicked, or returned early with `?` — ends the stream with
    /// `INTERNAL`, so a crashed producer never looks like a complete stream.
    pub fn channel(capacity: usize) -> (StreamSender<M>, Streaming<M>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (StreamSender { tx }, Streaming::new(Inner::Channel(rx)))
    }

    /// A stream yielding `messages`, then ending cleanly. The iterator is
    /// consumed lazily, one message per read — so it may be long or
    /// unbounded (an iterator that generates its items), and sending stops
    /// pulling from it as soon as the call ends.
    pub fn from_messages<I>(messages: I) -> Streaming<M>
    where
        I: IntoIterator<Item = M>,
        I::IntoIter: Send + 'static,
    {
        Streaming::new(Inner::Messages(Box::new(messages.into_iter())))
    }

    /// A stream that has already ended — cleanly, or with `status` when it
    /// is not OK.
    pub fn ended(status: Status) -> Streaming<M> {
        let trailers = status.is_ok().then(|| status.metadata().clone());
        let pending = (!status.is_ok()).then_some(status);
        Streaming {
            inner: Inner::Done(pending),
            trailers,
            polling: None,
        }
    }

    fn new(inner: Inner<M>) -> Self {
        Streaming {
            inner,
            trailers: None,
            polling: None,
        }
    }

    /// The trailing metadata of a response stream a client read, once
    /// [`message`](Self::message) has returned `Ok(None)` (for a stream that
    /// failed, it is on the error [`Status`]).
    pub fn trailers(&self) -> Option<&Metadata> {
        self.trailers.as_ref()
    }

    pub(crate) fn wire(wire: Wire) -> Self {
        Streaming::new(Inner::Wire(Box::new(wire)))
    }
}

impl<M: Message> Streaming<M> {
    /// The next message: `Ok(None)` at the clean end of the stream, `Err`
    /// when the stream failed — the call's error status on a client (for
    /// example `DEADLINE_EXCEEDED` or the handler's error), `CANCELLED` on a
    /// server when the client went away. After the end or an error it keeps
    /// returning `Ok(None)`.
    pub async fn message(&mut self) -> Result<Option<M>, Status> {
        if let Some(polling) = self.polling.as_mut() {
            // Finish the read `poll_message` started, in place: if this
            // future is dropped (a `select!` branch, a timeout), the read and
            // the source it owns stay for the next call.
            let (inner, trailers, result) = polling.as_mut().await;
            self.polling = None;
            self.inner = inner;
            self.trailers = trailers;
            return result;
        }
        self.read().await
    }

    /// The next message, in the shape of `Stream::next`: `None` at the clean
    /// end, `Some(Err(_))` when the stream failed (and `None` after that).
    pub async fn next(&mut self) -> Option<Result<M, Status>> {
        self.message().await.transpose()
    }

    /// Reads the rest of the stream into a `Vec`, failing with the stream's
    /// error. Mind the memory: every message is kept.
    pub async fn collect(mut self) -> Result<Vec<M>, Status> {
        let mut out = Vec::new();
        while let Some(m) = self.message().await? {
            out.push(m);
        }
        Ok(out)
    }

    /// Polls for the next message: the poll-based form of
    /// [`message`](Self::message), with the same results, for implementing a
    /// `Stream` trait over a `Streaming` (see the type's docs). Once it has
    /// returned `Pending`, keep polling it (or call `message`, which finishes
    /// the same read) — the read in progress owns the stream.
    pub fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<M>, Status>>
    where
        M: Send + 'static,
    {
        let polling = self.polling.get_or_insert_with(|| {
            let mut stream = Streaming {
                inner: std::mem::replace(&mut self.inner, Inner::Done(None)),
                trailers: self.trailers.take(),
                polling: None,
            };
            Box::pin(async move {
                let result = stream.read().await;
                (stream.inner, stream.trailers, result)
            })
        });
        match polling.as_mut().poll(cx) {
            Poll::Ready((inner, trailers, result)) => {
                self.polling = None;
                self.inner = inner;
                self.trailers = trailers;
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    async fn read(&mut self) -> Result<Option<M>, Status> {
        let result = match &mut self.inner {
            Inner::Channel(rx) => match rx.recv().await {
                Some(Item::Message(m)) => return Ok(Some(m)),
                Some(Item::End) => Ok(None),
                Some(Item::Fail(status)) => Err(status),
                None => Err(Status::internal(
                    "the stream's sender was dropped without finishing the stream",
                )),
            },
            Inner::Messages(it) => return Ok(it.next()),
            Inner::Wire(wire) => match wire.next_frame().await {
                Ok(Some(bytes)) => match decode_message(&bytes, wire.limits) {
                    Ok(m) => return Ok(Some(m)),
                    Err(status) => Err(status),
                },
                Ok(None) => {
                    self.trailers = wire.trailers.take();
                    Ok(None)
                }
                Err(status) => Err(status),
            },
            Inner::Done(pending) => return pending.take().map_or(Ok(None), Err),
        };
        // Ended (cleanly or not): drop the source — on a client that resets
        // the stream if it is still open.
        self.inner = Inner::Done(None);
        result
    }
}

/// The sending half of a [`Streaming::channel`].
///
/// End the stream with [`finish`](Self::finish) (or [`fail`](Self::fail));
/// dropping the sender without either ends it with `INTERNAL`.
pub struct StreamSender<M> {
    tx: mpsc::Sender<Item<M>>,
}

impl<M> fmt::Debug for StreamSender<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamSender")
            .field("closed", &self.tx.is_closed())
            .finish()
    }
}

fn reader_gone() -> Status {
    Status::cancelled("the stream's reader is gone (the call ended or was cancelled)")
}

impl<M> StreamSender<M> {
    /// Sends a message, waiting while the queue is full. Fails with
    /// `CANCELLED` when the reading side is gone: the call finished, failed,
    /// passed its deadline or was cancelled by the peer — stop producing
    /// then.
    pub async fn send(&self, message: M) -> Result<(), Status> {
        self.tx
            .send(Item::Message(message))
            .await
            .map_err(|_| reader_gone())
    }

    /// Ends the stream cleanly (on a server: the call ends with `OK`; on a
    /// client: the request stream is half-closed).
    pub async fn finish(self) -> Result<(), Status> {
        self.tx.send(Item::End).await.map_err(|_| reader_gone())
    }

    /// Ends the stream with an error: on a server, the call ends with this
    /// status; on a client, the call is cancelled and fails with it.
    pub async fn fail(self, status: Status) -> Result<(), Status> {
        self.tx
            .send(Item::Fail(status))
            .await
            .map_err(|_| reader_gone())
    }

    /// Whether the reading side is gone, so sends would fail.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Waits until the reading side is gone — lets an idle producer notice
    /// that the call ended (for example in a `tokio::select!`).
    pub async fn closed(&self) {
        self.tx.closed().await
    }
}

/// Where a wire stream's bytes come from.
pub(crate) enum Source {
    /// A request body, on the server.
    Server(BodyStream),
    /// A response body, on the client, with the slot where a failed upload
    /// leaves its status.
    Client {
        resp: Box<crate::http::Response>,
        upload_error: Option<Arc<Mutex<Option<Status>>>>,
    },
}

/// Frames read from an HTTP/2 body.
pub(crate) struct Wire {
    source: Source,
    decoder: FrameDecoder,
    limits: Limits,
    deadline: Option<Instant>,
    trailers: Option<Metadata>,
}

/// How a wire stream's messages are received.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Receive {
    /// The largest message, in bytes (after decompression).
    pub(crate) max_size: usize,
    /// The decode budget of each message.
    pub(crate) limits: Limits,
    /// The stream's `grpc-encoding`.
    pub(crate) encoding: Encoding,
}

impl Wire {
    pub(crate) fn new(source: Source, receive: Receive, deadline: Option<Instant>) -> Self {
        Wire {
            source,
            decoder: FrameDecoder::new(receive.max_size).with_encoding(receive.encoding),
            limits: receive.limits,
            deadline,
            trailers: None,
        }
    }

    /// The next frame's bytes, `None` at the clean end.
    pub(crate) async fn next_frame(&mut self) -> Result<Option<Vec<u8>>, Status> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            let read = async {
                match &mut self.source {
                    Source::Server(body) => body.chunk().await,
                    Source::Client { resp, .. } => resp.chunk().await,
                }
            };
            let chunk = match self.deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, read).await {
                    Ok(chunk) => chunk,
                    Err(_) => return Err(Status::deadline_exceeded("the deadline passed")),
                },
                None => read.await,
            };
            match chunk {
                Ok(Some(chunk)) => self.decoder.push(&chunk),
                Ok(None) => {
                    self.decoder.finish()?;
                    return self.end();
                }
                Err(e) => return Err(self.failure(&e)),
            }
        }
    }

    fn failure(&self, error: &crate::http::HttpError) -> Status {
        match &self.source {
            Source::Server(_) => Status::cancelled(format!("the request stream failed: {error}")),
            Source::Client { upload_error, .. } => upload_error
                .as_ref()
                .and_then(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).take())
                .unwrap_or_else(|| Status::from_http_error(error, self.deadline)),
        }
    }

    /// The clean end of the body: on the client, the status in the trailers.
    fn end(&mut self) -> Result<Option<Vec<u8>>, Status> {
        let Source::Client { resp, .. } = &self.source else {
            return Ok(None);
        };
        let status = resp
            .trailers()
            .and_then(Status::from_header_map)
            .unwrap_or_else(|| {
                Status::internal("the server ended the response without a grpc-status")
            });
        if status.is_ok() {
            self.trailers = Some(status.metadata().clone());
            Ok(None)
        } else {
            Err(status)
        }
    }
}

/// A [`Streaming`] whose messages are encoded into frames, type-erased so
/// the server and client pumps need not be generic.
pub(crate) trait FrameSource: Send {
    /// The next encoded frame; `None` at the clean end.
    fn next_frame(&mut self) -> BoxFuture<'_, Option<Result<Vec<u8>, Status>>>;
}

pub(crate) struct Encoder<M> {
    pub(crate) stream: Streaming<M>,
    pub(crate) max_size: usize,
}

impl<M: Message + Send + 'static> FrameSource for Encoder<M> {
    fn next_frame(&mut self) -> BoxFuture<'_, Option<Result<Vec<u8>, Status>>> {
        Box::pin(async move {
            match self.stream.message().await {
                Ok(Some(m)) => Some(encode_frame(&m, self.max_size)),
                Ok(None) => None,
                Err(status) => Some(Err(status)),
            }
        })
    }
}

/// How a [`pump`] ended.
pub(crate) enum PumpEnd {
    /// The source ended cleanly.
    Done,
    /// The source failed, or the deadline passed.
    Failed(Status),
    /// Nobody reads the body any more.
    ReaderGone,
}

/// Moves frames from `source` into `tx` until one side ends or `deadline`
/// passes. An idle source is abandoned as soon as the reader goes away.
pub(crate) async fn pump(
    source: &mut dyn FrameSource,
    tx: &mut BodySender,
    deadline: Option<Instant>,
) -> PumpEnd {
    let expired = || async move {
        match deadline {
            Some(d) => tokio::time::sleep_until(d).await,
            None => std::future::pending().await,
        }
    };
    let late = || PumpEnd::Failed(Status::deadline_exceeded("the deadline passed"));
    loop {
        let next = tokio::select! {
            next = source.next_frame() => next,
            () = tx.closed() => return PumpEnd::ReaderGone,
            () = expired() => return late(),
        };
        let frame = match next {
            None => return PumpEnd::Done,
            Some(Err(status)) => return PumpEnd::Failed(status),
            Some(Ok(frame)) => frame,
        };
        tokio::select! {
            sent = tx.send(frame) => if sent.is_err() {
                return PumpEnd::ReaderGone;
            },
            () = expired() => return late(),
        }
    }
}
