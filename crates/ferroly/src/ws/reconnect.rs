//! A client wrapper that reconnects with exponential backoff and jitter.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use ferroly::lifecycle::BoxFuture;

use super::conn::WsSender;
use super::{rand, Message, WsClient, WsError, WsOptions};

/// Backoff settings for a [`ReconnectingClient`].
///
/// The delay before attempt *n* (counting consecutive failures from 0) is
/// `initial_backoff * 2^n`, capped at `max_backoff`, then reduced by a random
/// fraction of up to `jitter` so that many clients do not reconnect in step.
#[derive(Debug, Clone)]
pub struct ReconnectOptions {
    /// The delay before the first reconnect attempt. Default: 500 ms.
    pub initial_backoff: Duration,
    /// The largest delay between attempts. Default: 30 s.
    pub max_backoff: Duration,
    /// The fraction of each delay that is randomised, `0.0..=1.0` (values
    /// outside are clamped). `0.0` disables jitter. Default: `0.5`.
    pub jitter: f64,
    /// Give up after this many consecutive failures (a failed dial, a failed
    /// connect hook, or a connection that dropped before `stable_after`).
    /// `None` retries forever. Default: `None`.
    pub max_attempts: Option<u32>,
    /// A connection that stays up at least this long counts as a success
    /// and resets the backoff. Default: 10 s.
    pub stable_after: Duration,
}

impl Default for ReconnectOptions {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            jitter: 0.5,
            max_attempts: None,
            stable_after: Duration::from_secs(10),
        }
    }
}

impl ReconnectOptions {
    /// The delay before the attempt that follows `failures` consecutive
    /// failures, given a random `unit` in `[0, 1)`.
    fn delay(&self, failures: u32, unit: f64) -> Duration {
        let factor = 1u32.checked_shl(failures.min(31)).unwrap_or(u32::MAX);
        let base = self
            .initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff);
        let jitter = self.jitter.clamp(0.0, 1.0);
        base.mul_f64(1.0 - jitter * unit.clamp(0.0, 1.0))
    }
}

type ConnectHook =
    Arc<dyn Fn(WsSender, bool) -> BoxFuture<'static, Result<(), WsError>> + Send + Sync>;

/// A WebSocket client that re-dials when its connection is lost.
///
/// It connects lazily on first use. When the connection ends for any reason
/// other than [`close`](Self::close), [`recv`](Self::recv) first returns the
/// final [`Message::Close`] (so the application knows there was a gap), and
/// the next call dials again after a backoff delay. After every successful
/// dial — the first one included — the [`on_connect`](Self::on_connect) hook
/// runs before any message is delivered, which is where subscriptions are
/// (re-)established.
///
/// Messages are never replayed: anything queued on a connection that drops
/// is lost, and delivery is at most once.
///
/// ```no_run
/// use ferroly::ws::{Message, ReconnectOptions, ReconnectingClient, WsOptions};
///
/// # async fn ex() -> Result<(), ferroly::ws::WsError> {
/// let mut client = ReconnectingClient::new("wss://example.com/feed", WsOptions::default())
///     .with_reconnect(ReconnectOptions {
///         max_attempts: Some(10),
///         ..ReconnectOptions::default()
///     })
///     .on_connect(|sender, _reconnected| async move {
///         sender.send(Message::text(r#"{"subscribe":"prices"}"#)).await
///     });
/// loop {
///     match client.recv().await? {
///         Message::Close { code, .. } => eprintln!("connection lost ({code}), reconnecting"),
///         msg => println!("{msg:?}"),
///     }
/// }
/// # }
/// ```
pub struct ReconnectingClient {
    url: String,
    options: WsOptions,
    reconnect: ReconnectOptions,
    hook: Option<ConnectHook>,
    current: Option<(WsClient, Instant)>,
    /// Consecutive failures since the last stable connection.
    failures: u32,
    /// Whether a connection has ever been established.
    connected_once: bool,
    /// Whether a backoff delay is owed before the next dial.
    delay_next: bool,
    reconnects: u64,
}

impl std::fmt::Debug for ReconnectingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReconnectingClient")
            .field("url", &self.url)
            .field("connected", &self.is_connected())
            .field("reconnects", &self.reconnects)
            .finish()
    }
}

impl ReconnectingClient {
    /// Creates a client for `url`. Nothing is dialled until the first
    /// [`connect`](Self::connect), [`send`](Self::send) or
    /// [`recv`](Self::recv).
    pub fn new(url: impl Into<String>, options: WsOptions) -> Self {
        Self {
            url: url.into(),
            options,
            reconnect: ReconnectOptions::default(),
            hook: None,
            current: None,
            failures: 0,
            connected_once: false,
            delay_next: false,
            reconnects: 0,
        }
    }

    /// Sets the backoff settings.
    pub fn with_reconnect(mut self, reconnect: ReconnectOptions) -> Self {
        self.reconnect = reconnect;
        self
    }

    /// Sets a hook that runs after every successful dial, before any message
    /// is delivered. It receives a sender for the new connection and `true`
    /// when this is a reconnect (not the first connection). If it fails, the
    /// connection is dropped and counted as a failed attempt.
    pub fn on_connect<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(WsSender, bool) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), WsError>> + Send + 'static,
    {
        self.hook = Some(Arc::new(move |sender, reconnected| {
            Box::pin(hook(sender, reconnected))
        }));
        self
    }

    /// Whether a live connection is currently held.
    pub fn is_connected(&self) -> bool {
        matches!(&self.current, Some((client, _)) if !client.is_closed())
    }

    /// How many times the connection has been re-established.
    pub fn reconnects(&self) -> u64 {
        self.reconnects
    }

    /// A sender for the current connection, if there is one. It stops
    /// working when that connection is lost; fetch a new one after a
    /// reconnect (the `on_connect` hook is handed one automatically).
    pub fn sender(&self) -> Option<WsSender> {
        self.current.as_ref().map(|(client, _)| client.sender())
    }

    /// Ensures there is a live connection, dialling (with backoff between
    /// attempts) if necessary. Fails with the last error once
    /// [`max_attempts`](ReconnectOptions::max_attempts) consecutive
    /// attempts have failed; calling it again starts a fresh round.
    pub async fn connect(&mut self) -> Result<(), WsError> {
        if self.is_connected() {
            return Ok(());
        }
        self.drop_current();
        loop {
            if self.delay_next {
                let delay = self
                    .reconnect
                    .delay(self.failures.saturating_sub(1), rand::unit());
                tokio::time::sleep(delay).await;
            }
            self.delay_next = true;
            let attempt = dial_once(
                self.url.clone(),
                self.options.clone(),
                self.hook.clone(),
                self.connected_once,
            );
            match attempt.await {
                Ok(client) => {
                    if self.connected_once {
                        self.reconnects += 1;
                    }
                    self.connected_once = true;
                    self.current = Some((client, Instant::now()));
                    return Ok(());
                }
                Err(error) => {
                    self.failures = self.failures.saturating_add(1);
                    if self.exhausted() {
                        self.failures = 0;
                        return Err(error);
                    }
                }
            }
        }
    }

    fn exhausted(&self) -> bool {
        matches!(self.reconnect.max_attempts, Some(max) if self.failures >= max)
    }

    /// Forgets the current connection, updating the failure streak: a
    /// connection that lived long enough resets it, a short-lived one
    /// extends it (so a server that accepts and immediately closes is not
    /// hammered).
    fn drop_current(&mut self) {
        if let Some((_, since)) = self.current.take() {
            if since.elapsed() >= self.reconnect.stable_after {
                self.failures = 0;
            } else {
                self.failures = self.failures.saturating_add(1);
            }
            self.delay_next = true;
        }
    }

    /// Receives the next message, reconnecting first if the connection was
    /// lost.
    ///
    /// A [`Message::Close`] means the current connection just ended; the
    /// next call reconnects. An error means reconnecting failed
    /// [`max_attempts`](ReconnectOptions::max_attempts) times in a row.
    pub async fn recv(&mut self) -> Result<Message, WsError> {
        loop {
            // A connection that is closing may still hold unread messages
            // and its final close message: drain it before dialling again.
            if self.current.is_none() {
                self.connect().await?;
            }
            let Some((client, _)) = self.current.as_mut() else {
                continue;
            };
            match client.recv().await {
                Some(msg) => {
                    if msg.is_close() {
                        self.drop_current();
                        if self.exhausted() {
                            self.failures = 0;
                            return Err(WsError::Connect(
                                "connection keeps dropping; giving up".into(),
                            ));
                        }
                    }
                    return Ok(msg);
                }
                None => self.drop_current(),
            }
        }
    }

    /// Sends a message on the current connection, connecting first if
    /// necessary. A message that fails to send is **not** retried.
    ///
    /// If the current connection has ended and its remaining messages have
    /// not been read with [`recv`](Self::recv) yet, they are discarded when
    /// this dials the replacement.
    pub async fn send(&mut self, msg: Message) -> Result<(), WsError> {
        self.connect().await?;
        match &self.current {
            Some((client, _)) => client.send(msg).await,
            None => Err(WsError::Closed),
        }
    }

    /// Closes the current connection (code `1000`) and stops reconnecting.
    pub async fn close(mut self) -> Result<(), WsError> {
        match self.current.take() {
            Some((client, _)) => client.close().await,
            None => Ok(()),
        }
    }
}

/// One dial plus the connect hook. Takes owned values so the future does not
/// borrow the client.
async fn dial_once(
    url: String,
    options: WsOptions,
    hook: Option<ConnectHook>,
    reconnected: bool,
) -> Result<WsClient, WsError> {
    let client = WsClient::dial(&url, options).await?;
    if let Some(hook) = hook {
        hook(client.sender(), reconnected).await?;
    }
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_caps_and_jitters_downwards() {
        let r = ReconnectOptions {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            jitter: 0.0,
            ..ReconnectOptions::default()
        };
        assert_eq!(r.delay(0, 0.9), Duration::from_millis(100));
        assert_eq!(r.delay(1, 0.9), Duration::from_millis(200));
        assert_eq!(r.delay(3, 0.9), Duration::from_millis(800));
        assert_eq!(r.delay(4, 0.9), Duration::from_secs(1));
        assert_eq!(r.delay(200, 0.9), Duration::from_secs(1)); // no overflow

        let j = ReconnectOptions { jitter: 0.5, ..r };
        assert_eq!(j.delay(0, 0.0), Duration::from_millis(100));
        assert_eq!(j.delay(0, 1.0), Duration::from_millis(50));
        let d = j.delay(1, 0.5);
        assert!(d >= Duration::from_millis(100) && d <= Duration::from_millis(200));
        // Out-of-range jitter is clamped rather than producing a negative delay.
        let wild = ReconnectOptions { jitter: 7.0, ..j };
        assert_eq!(wild.delay(0, 0.5), Duration::from_millis(50));
    }
}
