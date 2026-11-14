//! Provider-agnostic messaging with a full-featured in-process provider:
//! queues and publish/subscribe topics, ack/redelivery with backoff, routing
//! keys, dead-lettering, TTL and delayed delivery, request/reply,
//! de-duplication, backpressure policies, observability hooks, and a draining
//! shutdown integrated with [`ferroly::lifecycle`].
//!
//! Design choices:
//! - A handler **returns an [`Ack`]** rather than mutating the message in
//!   place — the disposition is explicit, with no interior mutability.
//! - A message is one [`Message`] struct plus a codec, not a split
//!   header/body/message interface trio. Metadata (message id, correlation id,
//!   timestamp, reply address, trace context) lives in conventional
//!   [`headers`].
//! - Destinations are addressed by a `&str`: `chan://name` is a queue
//!   (competing consumers), `topic://name` is publish/subscribe (every
//!   subscription gets a copy; `*` and `>` wildcards).
//! - Async work is cancelled by dropping the future, so there are no separate
//!   context-carrying method variants.
//! - Backends plug in through the [`Provider`] trait; [`LocalProvider`] is the
//!   only one shipped.
//!
//! # Delivery guarantee
//!
//! [`LocalProvider`] is **in-process and in-memory**. Within the life of the
//! process it delivers **at least once**: an accepted message is handed to a
//! handler until it is acked, rejected or dead-lettered, and a handler error or
//! panic causes a redelivery, never a loss. **Nothing survives a restart** and
//! nothing crosses a process boundary — for durability, implement [`Provider`]
//! over a broker.
//!
//! ```
//! # use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver};
//! # #[tokio::main]
//! # async fn main() {
//! let bus = LocalProvider::new("bus");
//! bus.add_listener("chan://orders", handler(|msg| async move {
//!     println!("got {}", msg.body_str());
//!     Ack::Ok
//! }), Default::default()).await.unwrap();
//!
//! bus.send("chan://orders", Message::text("order-1"), Default::default()).await.unwrap();
//!
//! // Let the consumer finish what is buffered before the process exits.
//! let report = bus.shutdown(std::time::Duration::from_secs(5)).await;
//! assert!(report.is_clean());
//! # }
//! ```

#![deny(missing_docs)]

use std::future::Future;
use std::pin::Pin;

mod error;
mod local;
mod message;
mod observer;
mod options;
mod provider;
mod queue;

/// A boxed, `Send` future — the object-safe async desugaring.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub use error::{BatchFailure, MessagingError, Rejected};
pub use local::{DrainReport, LocalProvider, Undelivered, WeakLocalProvider};
pub use message::{fallible, handler, headers, new_message_id, Ack, Handler, Message};
#[cfg(feature = "log")]
pub use observer::LogObserver;
pub use observer::{DropReason, Observer};
pub use options::{Backoff, ExpiryAction, ListenerOptions, LocalOptions, OnFull, SendOptions};
pub use provider::{split_address, topic_matches, Producer, Provider, ProviderRegistry, Receiver};
