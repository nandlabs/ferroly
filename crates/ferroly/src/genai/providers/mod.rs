//! Concrete provider implementations, each gated behind a cargo feature.

#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
pub(crate) mod common;
#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
pub(crate) mod sse;

#[cfg(feature = "claude")]
mod claude;
#[cfg(feature = "ollama")]
mod ollama;
#[cfg(feature = "openai")]
mod openai;

#[cfg(feature = "claude")]
pub use claude::{ClaudeProvider, ClaudeProviderConfig};
#[cfg(feature = "ollama")]
pub use ollama::OllamaProvider;
#[cfg(feature = "openai")]
pub use openai::{AuthStyle, OpenAiConfig, OpenAiProvider};

#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
pub use options::{ProviderOptions, Timeouts};

#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
mod options {
    use std::fmt;
    use std::sync::Arc;
    use std::time::Duration;

    use ferroly::clients::{AuthProvider, Retry};
    use ferroly::genai::GenAiObserver;
    use ferroly::http::{Client, ClientTlsConfig, Proxy};

    /// The timeouts of a provider call. `None` disables a timeout.
    ///
    /// ```text
    /// call ──► [ connect ]──►[ send request ]──►[ wait for response ]──►[ read body / stream … ]
    ///           connect
    ///          └────────────────────── response ────────────────────┘   stream_idle per read
    ///          └──────────────────────────────── total ───────────────────────────────────────┘
    /// ```
    ///
    /// The defaults suit model calls, which legitimately take minutes: a
    /// non-streaming completion sends nothing until the whole answer is
    /// generated, so `response` is long; a stream is watched by `stream_idle`
    /// instead.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Timeouts {
        /// DNS + TCP connect + TLS handshake (+ proxy tunnel). Default 10 s.
        /// Expiry is [`TimeoutPhase::Connect`](crate::genai::TimeoutPhase):
        /// nothing was sent, so the call is safe to repeat.
        pub connect: Option<Duration>,
        /// From the start of the call until the response starts (status line
        /// and headers) — the "first byte" timeout. Default 10 minutes.
        pub response: Option<Duration>,
        /// The longest silence tolerated while reading a response body or a
        /// stream. Default 5 minutes. Providers send keep-alive events during
        /// long generations; a model that "thinks" silently for longer than
        /// this needs a larger value.
        pub stream_idle: Option<Duration>,
        /// A deadline for the whole exchange, body included. Default none. It
        /// also bounds a stream, so leave it unset (or generous) for streamed
        /// calls.
        pub total: Option<Duration>,
    }

    impl Default for Timeouts {
        fn default() -> Self {
            Timeouts {
                connect: Some(Duration::from_secs(10)),
                response: Some(Duration::from_secs(600)),
                stream_idle: Some(Duration::from_secs(300)),
                total: None,
            }
        }
    }

    /// Common construction options for the built-in HTTP providers.
    ///
    /// Every field has a working default; set fields directly, use the chained
    /// setters, or a struct literal with `..Default::default()`.
    ///
    /// ```
    /// use std::time::Duration;
    /// use ferroly::genai::ProviderOptions;
    ///
    /// let opts = ProviderOptions::with_base_url("https://llm-gateway.internal")
    ///     .header("x-team", "search")
    ///     .response_timeout(Duration::from_secs(120));
    /// assert_eq!(opts.timeouts.response, Some(Duration::from_secs(120)));
    /// ```
    #[derive(Clone)]
    pub struct ProviderOptions {
        /// Overrides the provider's default API base URL (no trailing slash).
        pub base_url: Option<String>,
        /// Replaces the provider's built-in credential handling (API key
        /// header) with a custom [`AuthProvider`] — for example an OAuth 2.0
        /// client-credentials source or a gateway token. The provider runs
        /// `clients::authorize`, so refreshing providers work.
        pub auth: Option<Arc<dyn AuthProvider>>,
        /// Extra headers sent with every request (organisation ids, gateway
        /// routing, beta flags). A provider's own required headers win.
        pub headers: Vec<(String, String)>,
        /// The call's timeouts.
        pub timeouts: Timeouts,
        /// An outbound proxy, passed to the HTTP client.
        pub proxy: Option<Proxy>,
        /// Custom TLS settings (private CA, client certificate), passed to the
        /// HTTP client.
        pub tls: Option<ClientTlsConfig>,
        /// A ready-made HTTP client to use instead of building one. Its own
        /// timeouts, proxy and TLS settings apply; of [`timeouts`](Self::timeouts)
        /// only `stream_idle` is still enforced (by the stream reader).
        pub http_client: Option<Client>,
        /// Retry failed calls inside the provider, honouring `Retry-After`.
        /// Off by default. Only failures that are safe and useful to repeat
        /// are retried (see
        /// [`GenAiError::error_class`](crate::genai::GenAiError::error_class));
        /// a response timeout is not. A streamed call is retried only until
        /// the stream is established.
        pub retry: Option<Retry>,
        /// An observer notified of every call.
        pub observer: Option<Arc<dyn GenAiObserver>>,
        /// How much of a provider's error body is kept in an error message.
        /// Default 2048 bytes.
        pub max_error_body_bytes: usize,
        /// Cap on a non-streaming response body. Default 64 MiB.
        pub max_response_bytes: Option<u64>,
        /// Cap on the total size of a streamed response. Default none: a
        /// stream is bounded by `stream_idle` (and `total`) and by a per-line
        /// limit instead, because a long generation has no natural size.
        pub max_stream_bytes: Option<u64>,
        /// The largest number of inputs sent in one embeddings request; larger
        /// requests are split and the vectors re-joined in order. `None` uses
        /// the provider's default (OpenAI 2048, Ollama 128).
        pub embed_batch_size: Option<usize>,
    }

    impl Default for ProviderOptions {
        fn default() -> Self {
            ProviderOptions {
                base_url: None,
                auth: None,
                headers: Vec::new(),
                timeouts: Timeouts::default(),
                proxy: None,
                tls: None,
                http_client: None,
                retry: None,
                observer: None,
                max_error_body_bytes: 2048,
                max_response_bytes: Some(64 * 1024 * 1024),
                max_stream_bytes: None,
                embed_batch_size: None,
            }
        }
    }

    impl fmt::Debug for ProviderOptions {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // Header values may hold credentials: print the names only.
            let header_names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
            f.debug_struct("ProviderOptions")
                .field("base_url", &self.base_url)
                .field("auth", &self.auth.as_ref().map(|_| "<custom>"))
                .field("headers", &header_names)
                .field("timeouts", &self.timeouts)
                .field("proxy", &self.proxy.is_some())
                .field("tls", &self.tls.is_some())
                .field("http_client", &self.http_client.is_some())
                .field("retry", &self.retry)
                .field("observer", &self.observer.is_some())
                .field("max_error_body_bytes", &self.max_error_body_bytes)
                .field("max_response_bytes", &self.max_response_bytes)
                .field("max_stream_bytes", &self.max_stream_bytes)
                .field("embed_batch_size", &self.embed_batch_size)
                .finish()
        }
    }

    impl ProviderOptions {
        /// Default options.
        pub fn new() -> Self {
            ProviderOptions::default()
        }

        /// Creates options with a custom base URL.
        pub fn with_base_url(base_url: impl Into<String>) -> Self {
            ProviderOptions {
                base_url: Some(base_url.into()),
                ..ProviderOptions::default()
            }
        }

        /// Sets the base URL.
        pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
            self.base_url = Some(base_url.into());
            self
        }

        /// Uses a custom auth provider instead of the built-in API-key header.
        pub fn auth(mut self, auth: Arc<dyn AuthProvider>) -> Self {
            self.auth = Some(auth);
            self
        }

        /// Adds a header sent with every request.
        pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
            self.headers.push((name.into(), value.into()));
            self
        }

        /// Replaces all four timeouts.
        pub fn timeouts(mut self, timeouts: Timeouts) -> Self {
            self.timeouts = timeouts;
            self
        }

        /// Sets the connect timeout.
        pub fn connect_timeout(mut self, timeout: Duration) -> Self {
            self.timeouts.connect = Some(timeout);
            self
        }

        /// Sets the response (first-byte) timeout.
        pub fn response_timeout(mut self, timeout: Duration) -> Self {
            self.timeouts.response = Some(timeout);
            self
        }

        /// Sets the stream idle timeout.
        pub fn stream_idle_timeout(mut self, timeout: Duration) -> Self {
            self.timeouts.stream_idle = Some(timeout);
            self
        }

        /// Sets the total deadline of one exchange.
        pub fn total_timeout(mut self, timeout: Duration) -> Self {
            self.timeouts.total = Some(timeout);
            self
        }

        /// Routes requests through a proxy.
        pub fn proxy(mut self, proxy: Proxy) -> Self {
            self.proxy = Some(proxy);
            self
        }

        /// Uses custom TLS settings.
        pub fn tls(mut self, tls: ClientTlsConfig) -> Self {
            self.tls = Some(tls);
            self
        }

        /// Uses a ready-made HTTP client.
        pub fn http_client(mut self, client: Client) -> Self {
            self.http_client = Some(client);
            self
        }

        /// Retries failed calls inside the provider.
        pub fn retry(mut self, retry: impl Into<Retry>) -> Self {
            self.retry = Some(retry.into());
            self
        }

        /// Installs an observer.
        pub fn observer(mut self, observer: Arc<dyn GenAiObserver>) -> Self {
            self.observer = Some(observer);
            self
        }

        /// Sets the embeddings batch size.
        pub fn embed_batch_size(mut self, size: usize) -> Self {
            self.embed_batch_size = Some(size);
            self
        }
    }
}
