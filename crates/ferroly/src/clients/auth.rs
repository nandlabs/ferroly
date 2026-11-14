//! Authentication providers applied to outbound requests.

use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::time::Instant;

use ferroly::clients::error::AuthError;
use ferroly::http::{BoxFuture, Request};

use super::{base64, Secret};

/// Applies authentication to an outbound [`ferroly::http::Request`].
///
/// Providers are shared as `Arc<dyn AuthProvider>` so a client (or a GenAI
/// provider) can be configured with any scheme without changing its type.
///
/// # Static and refreshing credentials
///
/// [`apply`](AuthProvider::apply) is synchronous: it copies credentials the
/// provider already holds into headers. That is all a static scheme
/// ([`BearerAuth`], [`ApiKeyAuth`], [`BasicAuth`]) needs.
///
/// A provider whose credentials expire ([`RefreshingAuth`], OAuth 2.0) needs
/// to do I/O first. That happens in [`prepare`](AuthProvider::prepare), an
/// async hook with a no-op default. **Callers should `prepare().await` before
/// every `apply`** — or use [`authorize`], which does both — so any provider
/// works:
///
/// ```
/// use ferroly::clients::{authorize, AuthProvider, BearerAuth};
/// use ferroly::http::{Method, Request};
///
/// # async fn send(provider: &dyn AuthProvider) -> Result<(), ferroly::clients::AuthError> {
/// let mut req = Request::builder(Method::Get, "https://api.example/v1/items")
///     .expect("valid url")
///     .build();
/// authorize(provider, &mut req).await?; // prepare(), then apply()
/// # Ok(())
/// # }
/// ```
pub trait AuthProvider: Send + Sync {
    /// Applies credentials to the request by setting the appropriate headers.
    fn apply(&self, req: &mut Request);

    /// Makes sure the credentials [`apply`](AuthProvider::apply) will use are
    /// present and current, fetching or refreshing them if necessary.
    ///
    /// The default does nothing, which is right for static credentials.
    fn prepare(&self) -> BoxFuture<'_, Result<(), AuthError>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Prepares `provider` (refreshing its credentials if needed) and applies it
/// to `req`. The async-aware replacement for calling
/// [`AuthProvider::apply`] directly.
pub async fn authorize(provider: &dyn AuthProvider, req: &mut Request) -> Result<(), AuthError> {
    provider.prepare().await?;
    provider.apply(req);
    Ok(())
}

/// Bearer-token authentication (`Authorization: Bearer <token>`).
///
/// The token is held in a [`Secret`]: it is not shown by `Debug` and its
/// buffer is wiped on drop.
#[derive(Clone)]
pub struct BearerAuth {
    token: Secret,
}

impl BearerAuth {
    /// Creates a bearer-token provider.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: Secret::from(token.into()),
        }
    }

    /// Creates a bearer-token provider from a [`Secret`]. A token that is
    /// not valid UTF-8 results in an empty `Bearer` value.
    pub fn from_secret(token: Secret) -> Self {
        Self { token }
    }
}

impl fmt::Debug for BearerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BearerAuth")
            .field("token", &self.token)
            .finish()
    }
}

impl AuthProvider for BearerAuth {
    fn apply(&self, req: &mut Request) {
        req.headers.set(
            "Authorization",
            format!("Bearer {}", self.token.expose_str().unwrap_or_default()),
        );
    }
}

/// API-key authentication placed in a caller-specified header.
///
/// `Debug` shows the header name and redacts the key.
#[derive(Clone)]
pub struct ApiKeyAuth {
    header: String,
    key: Secret,
}

impl ApiKeyAuth {
    /// Creates an API-key provider that sets `header: key`.
    pub fn new(header: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            key: Secret::from(key.into()),
        }
    }
}

impl fmt::Debug for ApiKeyAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyAuth")
            .field("header", &self.header)
            .field("key", &self.key)
            .finish()
    }
}

impl AuthProvider for ApiKeyAuth {
    fn apply(&self, req: &mut Request) {
        req.headers.set(
            self.header.clone(),
            self.key.expose_str().unwrap_or_default(),
        );
    }
}

/// HTTP Basic authentication.
///
/// `Debug` shows the user name and redacts the password.
#[derive(Clone)]
pub struct BasicAuth {
    user: String,
    pass: Secret,
}

impl BasicAuth {
    /// Creates a basic-auth provider.
    pub fn new(user: impl Into<String>, pass: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            pass: Secret::from(pass.into()),
        }
    }
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasicAuth")
            .field("user", &self.user)
            .field("pass", &self.pass)
            .finish()
    }
}

/// `Basic base64(user ":" pass)`, with the intermediate `user:pass` buffer
/// wiped after encoding.
pub(crate) fn basic_header_value(user: &str, pass: &[u8]) -> String {
    let mut pair = Vec::with_capacity(user.len() + 1 + pass.len());
    pair.extend_from_slice(user.as_bytes());
    pair.push(b':');
    pair.extend_from_slice(pass);
    let pair = Secret::new(pair);
    format!("Basic {}", base64::encode(pair.expose()))
}

impl AuthProvider for BasicAuth {
    fn apply(&self, req: &mut Request) {
        req.headers.set(
            "Authorization",
            basic_header_value(&self.user, self.pass.expose()),
        );
    }
}

// ---- refreshing tokens -------------------------------------------------------

/// An access token together with how long it stays valid.
///
/// `Debug` redacts the token.
#[derive(Debug, Clone)]
pub struct AccessToken {
    token: Secret,
    expires_in: Option<Duration>,
}

impl AccessToken {
    /// A token with no known expiry (it is used until
    /// [`RefreshingAuth::invalidate`] is called).
    pub fn new(token: impl Into<Secret>) -> Self {
        AccessToken {
            token: token.into(),
            expires_in: None,
        }
    }

    /// Sets the lifetime, counted from the moment the token was obtained
    /// (the OAuth 2.0 `expires_in`).
    pub fn expires_in(mut self, lifetime: Duration) -> Self {
        self.expires_in = Some(lifetime);
        self
    }

    /// The token.
    pub fn token(&self) -> &Secret {
        &self.token
    }

    /// The lifetime, if the issuer stated one.
    pub fn lifetime(&self) -> Option<Duration> {
        self.expires_in
    }
}

/// Something that can obtain a fresh access token — an OAuth 2.0 grant, a
/// cloud metadata endpoint, a secrets manager, a local command.
///
/// Implement it for your own source and wrap it in a [`RefreshingAuth`],
/// which adds caching, expiry tracking and single-flight refresh:
///
/// ```
/// use std::time::Duration;
/// use ferroly::clients::{AccessToken, AuthError, RefreshingAuth, TokenSource};
/// use ferroly::http::BoxFuture;
///
/// struct Vault;
///
/// impl TokenSource for Vault {
///     fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>> {
///         Box::pin(async {
///             // ... call the secrets manager ...
///             Ok(AccessToken::new("token-from-vault").expires_in(Duration::from_secs(900)))
///         })
///     }
/// }
///
/// let auth = RefreshingAuth::new(Vault);
/// # let _ = auth;
/// ```
///
/// A source should not cache: every call should produce a token that is
/// valid for its full lifetime.
pub trait TokenSource: Send + Sync {
    /// Obtains a new access token.
    fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>>;
}

struct CachedToken {
    token: Secret,
    /// The ready-to-send `Bearer <token>` header value.
    header: Secret,
    expires_at: Option<Instant>,
}

/// An [`AuthProvider`] that sends `Authorization: Bearer <token>` with a
/// token obtained from a [`TokenSource`], cached and refreshed before it
/// expires.
///
/// - **Caching** — the token is reused until `skew` (default 30 s) before its
///   expiry; a token without an expiry is reused until
///   [`invalidate`](Self::invalidate).
/// - **Single flight** — when a refresh is due, concurrent callers share one
///   fetch instead of each requesting a token.
/// - **Graceful refresh** — if a refresh fails while the current token has
///   not actually expired yet, the current token keeps being used and the
///   error is swallowed; the next call tries again.
///
/// The refresh happens in [`prepare`](AuthProvider::prepare) (or
/// [`token`](Self::token)). [`apply`](AuthProvider::apply) only copies the
/// cached token; if nothing has been fetched yet it sets no header, so
/// callers must `prepare().await` first — [`authorize`] does both.
pub struct RefreshingAuth {
    source: Arc<dyn TokenSource>,
    skew: Duration,
    cached: RwLock<Option<Arc<CachedToken>>>,
    refresh: tokio::sync::Mutex<()>,
}

impl fmt::Debug for RefreshingAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshingAuth")
            .field("skew", &self.skew)
            .field("has_token", &self.current().is_some())
            .finish_non_exhaustive()
    }
}

impl RefreshingAuth {
    /// Wraps a token source.
    pub fn new(source: impl TokenSource + 'static) -> Self {
        Self::from_arc(Arc::new(source))
    }

    /// Wraps a shared token source.
    pub fn from_arc(source: Arc<dyn TokenSource>) -> Self {
        RefreshingAuth {
            source,
            skew: Duration::from_secs(30),
            cached: RwLock::new(None),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    /// How long before its expiry a token is considered due for refresh, so
    /// a request never leaves with a token that expires in flight. Default
    /// 30 seconds.
    pub fn skew(mut self, skew: Duration) -> Self {
        self.skew = skew;
        self
    }

    fn current(&self) -> Option<Arc<CachedToken>> {
        self.cached
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether `token` can still be used without refreshing.
    fn is_fresh(&self, token: &CachedToken) -> bool {
        match token.expires_at {
            None => true,
            Some(at) => at.saturating_duration_since(Instant::now()) > self.skew,
        }
    }

    /// Whether `token` is past its actual expiry.
    fn is_expired(token: &CachedToken) -> bool {
        token.expires_at.is_some_and(|at| Instant::now() >= at)
    }

    async fn ensure(&self) -> Result<Arc<CachedToken>, AuthError> {
        if let Some(token) = self.current() {
            if self.is_fresh(&token) {
                return Ok(token);
            }
        }
        // One refresh at a time; whoever waited here re-checks the cache.
        let _refreshing = self.refresh.lock().await;
        let current = self.current();
        if let Some(token) = &current {
            if self.is_fresh(token) {
                return Ok(token.clone());
            }
        }
        let obtained_at = Instant::now();
        match self.source.fetch_token().await {
            Ok(fresh) => {
                let Some(text) = fresh.token.expose_str() else {
                    return Err(AuthError::InvalidResponse(
                        "access token is not valid UTF-8".into(),
                    ));
                };
                let cached = Arc::new(CachedToken {
                    header: Secret::from(format!("Bearer {text}")),
                    expires_at: fresh.expires_in.and_then(|d| obtained_at.checked_add(d)),
                    token: fresh.token,
                });
                *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(cached.clone());
                Ok(cached)
            }
            Err(error) => match current {
                // Still inside the skew window: the old token works.
                Some(token) if !Self::is_expired(&token) => Ok(token),
                _ => Err(error),
            },
        }
    }

    /// A currently valid access token, fetching or refreshing it if needed.
    pub async fn token(&self) -> Result<Secret, AuthError> {
        Ok(self.ensure().await?.token.clone())
    }

    /// Discards the cached token, so the next request fetches a new one. Call
    /// it when the server rejects the token (a `401`) before its stated
    /// expiry — it may have been revoked.
    pub fn invalidate(&self) {
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl AuthProvider for RefreshingAuth {
    fn apply(&self, req: &mut Request) {
        if let Some(token) = self.current() {
            req.headers.set(
                "Authorization",
                token.header.expose_str().unwrap_or_default(),
            );
        }
    }

    fn prepare(&self) -> BoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async move { self.ensure().await.map(|_| ()) })
    }
}
