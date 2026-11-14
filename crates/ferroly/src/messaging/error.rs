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

/// One message of a batch that was not accepted.
#[derive(Debug)]
#[non_exhaustive]
pub struct BatchEntryFailure {
    /// The position of the message in the batch that was passed in (0-based).
    pub index: usize,
    /// Why it failed.
    pub error: MessagingError,
    /// The message, handed back when the failing operation still had it.
    pub message: Option<Message>,
}

impl BatchEntryFailure {
    /// A failure for the message at `index`. If `error` carries the rejected
    /// message, it is moved into [`message`](Self::message).
    pub fn new(index: usize, mut error: MessagingError) -> Self {
        let message = error.take_message();
        Self {
            index,
            error,
            message,
        }
    }

    /// A failure for the message at `index`, with the message supplied.
    pub fn with_message(index: usize, error: MessagingError, message: Message) -> Self {
        Self {
            index,
            error,
            message: Some(message),
        }
    }
}

/// The detail of a [`MessagingError::PartialBatch`].
#[derive(Debug)]
#[non_exhaustive]
pub struct BatchFailure {
    /// How many messages were accepted.
    pub sent: usize,
    /// The messages that were tried and failed, in ascending `index` order.
    pub failures: Vec<BatchEntryFailure>,
    /// The messages that were never tried (a sequential send stops at the
    /// first failure), in their original order.
    pub unsent: Vec<Message>,
}

impl BatchFailure {
    /// A batch result. `failures` are sorted by index.
    pub fn new(sent: usize, mut failures: Vec<BatchEntryFailure>, unsent: Vec<Message>) -> Self {
        failures.sort_by_key(|f| f.index);
        Self {
            sent,
            failures,
            unsent,
        }
    }

    /// The error of the first failed entry.
    pub fn first_error(&self) -> Option<&MessagingError> {
        self.failures.first().map(|f| &f.error)
    }

    /// Every message that was not accepted: the failed ones that were handed
    /// back (in index order) followed by the ones never tried.
    pub fn into_messages(self) -> Vec<Message> {
        self.failures
            .into_iter()
            .filter_map(|f| f.message)
            .chain(self.unsent)
            .collect()
    }

    /// How many messages were not accepted (`failures.len() + unsent.len()`).
    pub fn failed(&self) -> usize {
        self.failures.len() + self.unsent.len()
    }
}

impl fmt::Display for BatchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} sent, {} failed", self.sent, self.failed())?;
        if let Some(error) = self.first_error() {
            write!(f, ": {error}")?;
        }
        Ok(())
    }
}

/// Errors raised by messaging operations.
///
/// The send-side variants ([`NoListeners`](Self::NoListeners),
/// [`BufferFull`](Self::BufferFull), [`Closed`](Self::Closed),
/// [`SendTimeout`](Self::SendTimeout)) and a backend's
/// [`Refused`](Self::Refused) hand the rejected message back: recover it with
/// [`into_message`](Self::into_message). Whether trying again may help is
/// [`is_retryable`](Self::is_retryable).
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
    /// The supplied options are not valid (for example `buffer_size: 0`), or
    /// ask for something the provider cannot honour.
    #[error("invalid options: {0}")]
    InvalidOptions(String),
    /// The provider does not implement the operation.
    #[error("not supported by this provider: {0}")]
    Unsupported(String),
    /// Shutdown finished with messages that could not be delivered; the count
    /// is the number of such messages.
    #[error("{0} message(s) could not be delivered before shutdown")]
    Undelivered(usize),
    /// Some messages of a batch were not accepted; see [`BatchFailure`].
    #[error("batch partly failed: {0}")]
    PartialBatch(BatchFailure),
    /// A failure reported by an external backend (connection, protocol,
    /// authorization, …). Build one with [`MessagingError::backend`] or
    /// [`MessagingError::backend_transient`].
    #[error("backend error: {source}")]
    Backend {
        /// The backend's own error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
        /// Whether trying again later may succeed (throttling, an outage, a
        /// timeout) as opposed to a failure that will repeat.
        transient: bool,
    },
    /// A backend refused one message — it was throttled, too large, or not
    /// acceptable — and hands it back. Build one with
    /// [`MessagingError::refused`] or [`MessagingError::refused_transient`].
    #[error("backend refused the message for {rejected}: {source}")]
    Refused {
        /// Why the backend refused it.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
        /// The destination and the message, handed back.
        rejected: Rejected,
        /// Whether sending the same message again later may succeed.
        transient: bool,
    },
}

impl MessagingError {
    /// Wraps a backend's error as a permanent failure. This is the variant an
    /// adapter for an external broker returns for its transport and protocol
    /// failures.
    pub fn backend<E>(source: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Self::Backend {
            source: source.into(),
            transient: false,
        }
    }

    /// Wraps a backend's error as a transient failure (throttling, an outage,
    /// a timeout): trying again later may succeed.
    pub fn backend_transient<E>(source: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Self::Backend {
            source: source.into(),
            transient: true,
        }
    }

    /// A backend refused `message` for `destination` and will refuse it again
    /// (too large, not acceptable). The message is handed back.
    pub fn refused<E>(source: E, destination: impl Into<String>, message: Message) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Self::Refused {
            source: source.into(),
            rejected: Rejected::new(destination, message),
            transient: false,
        }
    }

    /// A backend refused `message` for `destination` for now (throttled);
    /// sending it again later may succeed. The message is handed back.
    pub fn refused_transient<E>(source: E, destination: impl Into<String>, message: Message) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Self::Refused {
            source: source.into(),
            rejected: Rejected::new(destination, message),
            transient: true,
        }
    }

    /// Whether the same operation may succeed if it is tried again later.
    ///
    /// `true` for [`BufferFull`](Self::BufferFull), [`SendTimeout`](Self::SendTimeout),
    /// and for [`Backend`](Self::Backend) / [`Refused`](Self::Refused) marked
    /// transient. `true` for [`PartialBatch`](Self::PartialBatch) when at least
    /// one of its failures is retryable. `false` for everything else.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::BufferFull(_) | Self::SendTimeout(_) => true,
            Self::Backend { transient, .. } | Self::Refused { transient, .. } => *transient,
            Self::PartialBatch(batch) => batch.failures.iter().any(|f| f.error.is_retryable()),
            _ => false,
        }
    }

    fn rejected(&self) -> Option<&Rejected> {
        match self {
            Self::NoListeners(r) | Self::BufferFull(r) | Self::Closed(r) | Self::SendTimeout(r) => {
                Some(r)
            }
            Self::Refused { rejected, .. } => Some(rejected),
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
    pub fn take_message(&mut self) -> Option<Message> {
        match self {
            Self::NoListeners(r) | Self::BufferFull(r) | Self::Closed(r) | Self::SendTimeout(r) => {
                r.message.take().map(|m| *m)
            }
            Self::Refused { rejected, .. } => rejected.message.take().map(|m| *m),
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
