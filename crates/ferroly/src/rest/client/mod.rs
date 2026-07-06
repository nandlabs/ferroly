//! HTTP client built on the in-house `ferroly::http` client, with codec-aware
//! bodies, pluggable auth, and retry reused from `ferroly::clients`.

mod request;
mod response;

pub use request::RequestBuilder;
pub use response::Response;

use std::sync::Arc;
use std::time::Duration;

use ferroly::clients::{AuthProvider, RetryPolicy};
use ferroly::http::Method;

pub(crate) struct ClientInner {
    pub(crate) http: ferroly::http::Client,
    pub(crate) auth: Option<Arc<dyn AuthProvider>>,
    pub(crate) retry: Option<RetryPolicy>,
    pub(crate) default_content_type: String,
}

/// Configuration for a [`Client`].
#[derive(Default)]
pub struct ClientOptions {
    request_timeout: Option<Duration>,
    auth: Option<Arc<dyn AuthProvider>>,
    retry: Option<RetryPolicy>,
    default_content_type: Option<String>,
}

impl ClientOptions {
    /// Starts a builder.
    pub fn builder() -> ClientOptionsBuilder {
        ClientOptionsBuilder {
            options: ClientOptions::default(),
        }
    }
}

/// Fluent builder for [`ClientOptions`].
#[derive(Default)]
#[must_use]
pub struct ClientOptionsBuilder {
    options: ClientOptions,
}

impl ClientOptionsBuilder {
    /// Sets the per-request timeout.
    pub fn request_timeout(mut self, d: Duration) -> Self {
        self.options.request_timeout = Some(d);
        self
    }

    /// Sets the authentication provider applied to every request.
    pub fn auth(mut self, auth: Arc<dyn AuthProvider>) -> Self {
        self.options.auth = Some(auth);
        self
    }

    /// Sets the retry policy applied to transport errors and 5xx responses.
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.options.retry = Some(policy);
        self
    }

    /// Sets the default request content type (default `application/json`).
    pub fn default_content_type(mut self, ct: impl Into<String>) -> Self {
        self.options.default_content_type = Some(ct.into());
        self
    }

    /// Finalizes the options.
    pub fn build(self) -> ClientOptions {
        self.options
    }
}

/// An HTTP client with codec-aware request/response bodies.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Creates a client with default options.
    pub fn new() -> Self {
        Self::with_options(ClientOptions::default())
    }

    /// Creates a client from explicit options.
    pub fn with_options(options: ClientOptions) -> Self {
        let http = ferroly::http::Client::new().with_timeout(options.request_timeout);
        Self {
            inner: Arc::new(ClientInner {
                http,
                auth: options.auth,
                retry: options.retry,
                default_content_type: options
                    .default_content_type
                    .unwrap_or_else(|| "application/json".to_string()),
            }),
        }
    }

    /// Starts building a request for `url` with the given method.
    pub fn request(&self, url: impl Into<String>, method: Method) -> RequestBuilder {
        RequestBuilder::new(self.inner.clone(), method, url.into())
    }

    /// Shorthand for a `GET` request.
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Get)
    }

    /// Shorthand for a `POST` request.
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Post)
    }

    /// Shorthand for a `PUT` request.
    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Put)
    }

    /// Shorthand for a `DELETE` request.
    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Delete)
    }
}
