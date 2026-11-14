//! Error types for the messaging module.

use std::fmt;

use ferroly::messaging::Message;
use ferroly_derive::FerrolyError;

/// A message that could not be accepted, handed back to the caller together
/// with the destination it was meant for.
///
/// Displays as the destination. Take the message back with
/// [`into_message`](Self::into_message) (or
/// [`MessagingError::into_message`]) to retry, park or log it.
#[derive(Debug)]
pub struct Rejected {
    /// The destination name or address.
    pub destination: String,
    /// The message that was not accepted. `None` when the failing operation
    /// did not carry one (for example adding a listener to a stopped provider).
    pub message: Option<Box<Message>>,
}

impl Rejected {
    /// A rejection that hands `message` back.
    pub fn new(destination: impl Into<String>, message: Message) -> Self {
        Self {
            destination: destination.into(),
            message: Some(Box::new(message)),
        }
    }

    /// A rejection with no message attached.
    pub fn without_message(destination: impl Into<String>) -> Self {
        Self {
            destination: destination.into(),
            message: None,
        }
    }

    /// Takes the rejected message back.
    pub fn into_message(self) -> Option<Message> {
        self.message.map(|m| *m)
    }
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.destination)
    }
}

/// The detail of a [`MessagingError::Batch`]: how far a batch got, why it
/// stopped, and the messages that were not sent.
#[derive(Debug)]
pub struct BatchFailure {
    /// How many messages were accepted before the failure.
    pub sent: usize,
    /// The error that stopped the batch. Its own rejected message (if any) has
    /// been moved to the front of [`unsent`](Self::unsent).
    pub error: Box<MessagingError>,
    /// The messages that were not sent, in their original order, starting with
    /// the one that failed.
    pub unsent: Vec<Message>,
}

impl fmt::Display for BatchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} sent, {} unsent: {}",
            self.sent,
            self.unsent.len(),
            self.error
        )
    }
}

/// Errors raised by messaging operations.
///
/// The send-side variants ([`NoListeners`](Self::NoListeners),
/// [`BufferFull`](Self::BufferFull), [`Closed`](Self::Closed),
/// [`SendTimeout`](Self::SendTimeout)) hand the rejected message back: recover
/// it with [`into_message`](Self::into_message).
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum MessagingError {
    /// No listener is registered for the destination.
    #[error("no listeners for destination: {0}")]
    NoListeners(Rejected),
    /// The listener's buffer was full ([`OnFull::Reject`](crate::messaging::OnFull::Reject)).
    #[error("destination buffer full: {0}")]
    BufferFull(Rejected),
    /// The provider is draining or stopped, or the destination's listener went
    /// away.
    #[error("destination closed: {0}")]
    Closed(Rejected),
    /// The buffer stayed full for the whole
    /// [`OnFull::BlockFor`](crate::messaging::OnFull::BlockFor) wait.
    #[error("timed out waiting for buffer room: {0}")]
    SendTimeout(Rejected),
    /// A request received no reply within its timeout.
    #[error("no reply within the timeout from: {0}")]
    RequestTimeout(String),
    /// The address is malformed, uses a scheme the provider does not handle, or
    /// is not valid for the operation.
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    /// The supplied options are not valid (for example `buffer_size: 0`).
    #[error("invalid options: {0}")]
    InvalidOptions(String),
    /// The provider does not implement the operation.
    #[error("not supported by this provider: {0}")]
    Unsupported(String),
    /// Shutdown finished with messages that could not be delivered; the count
    /// is the number of such messages.
    #[error("{0} message(s) could not be delivered before shutdown")]
    Undelivered(usize),
    /// A batch stopped part-way; see [`BatchFailure`].
    #[error("batch send failed: {0}")]
    Batch(BatchFailure),
    /// A failure reported by an external backend (connection, protocol,
    /// authorization, …). Build one with [`MessagingError::backend`].
    #[error("backend error: {source}")]
    Backend {
        /// The backend's own error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl MessagingError {
    /// Wraps a backend's error. This is the variant an adapter for an external
    /// broker returns for its transport and protocol failures.
    pub fn backend<E>(source: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Self::Backend {
            source: source.into(),
        }
    }

    fn rejected(&self) -> Option<&Rejected> {
        match self {
            Self::NoListeners(r) | Self::BufferFull(r) | Self::Closed(r) | Self::SendTimeout(r) => {
                Some(r)
            }
            _ => None,
        }
    }

    /// The message the failed operation handed back, if any.
    pub fn message(&self) -> Option<&Message> {
        self.rejected().and_then(|r| r.message.as_deref())
    }

    /// Takes back the message the failed operation could not accept, if any.
    pub fn into_message(mut self) -> Option<Message> {
        self.take_message()
    }

    /// Detaches the rejected message (if any), leaving the error in place.
    pub(crate) fn take_message(&mut self) -> Option<Message> {
        match self {
            Self::NoListeners(r) | Self::BufferFull(r) | Self::Closed(r) | Self::SendTimeout(r) => {
                r.message.take().map(|m| *m)
            }
            _ => None,
        }
    }

    /// The destination the failed operation was addressed to, if known.
    pub fn destination(&self) -> Option<&str> {
        match self {
            Self::RequestTimeout(destination) => Some(destination),
            other => other.rejected().map(|r| r.destination.as_str()),
        }
    }
}
