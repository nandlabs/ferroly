//! API-key authentication: a hashed key store, constant-time comparison and
//! scopes. Requires the `hash` feature.

use std::sync::{Arc, RwLock};

use ferroly::hash::{ct_eq, random, sha256};
use ferroly::http::{BoxFuture, HttpResponse, StatusCode};

use super::ctx::Ctx;
use super::router::{Group, Router};

/// The header [`ApiKeyAuth`] reads by default.
pub const API_KEY_HEADER: &str = "x-api-key";

/// The SHA-256 digest under which an API key is stored.
///
/// Keys are high-entropy random strings ([`ApiKey::generate`]), so a plain
/// SHA-256 is the right hash: it cannot be inverted, and unlike a password
/// hash it is cheap enough to compute on every request. Do **not** use
/// human-chosen API keys; they would need a slow password hash.
pub fn hash_api_key(key: &str) -> [u8; 32] {
    sha256(key.as_bytes()).into_bytes()
}

/// A stored API key: an identifier, the SHA-256 digest of the key (never the
/// key itself) and the scopes it grants.
///
/// A successful [`ApiKeyAuth`] stores the matching record in the request's
/// extensions; read it with [`Ctx::api_key`].
///
/// ```
/// use ferroly::turbo::{hash_api_key, ApiKey};
///
/// // Issue a key: show `key` to the client once, store `record`.
/// let (key, record) = ApiKey::generate("client-42").unwrap();
/// let record = record.with_scopes(["orders:read"]);
/// assert_eq!(record.digest(), &hash_api_key(&key));
/// assert!(record.has_scope("orders:read"));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey {
    id: String,
    digest: [u8; 32],
    scopes: Vec<String>,
}

impl ApiKey {
    /// A record for the key whose SHA-256 is `digest` (see [`hash_api_key`]),
    /// identified by `id` (a client or key name, for logs and handlers).
    pub fn new(id: impl Into<String>, digest: [u8; 32]) -> Self {
        ApiKey {
            id: id.into(),
            digest,
            scopes: Vec::new(),
        }
    }

    /// A record for the plaintext `key`, which is hashed here and not kept.
    pub fn from_key(id: impl Into<String>, key: &str) -> Self {
        ApiKey::new(id, hash_api_key(key))
    }

    /// Generates a new key from the OS CSPRNG — 32 random bytes as 43
    /// URL-safe base64 characters — and returns it together with its record.
    /// Hand the key to the client once; store only the record.
    pub fn generate(id: impl Into<String>) -> Result<(String, ApiKey), random::RandomError> {
        let key = random::token_base64url(32)?;
        let record = ApiKey::from_key(id, &key);
        Ok((key, record))
    }

    /// Adds scopes this key grants.
    pub fn with_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }

    /// The key's identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The SHA-256 digest of the key.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// The scopes this key grants.
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// Whether this key grants `scope` (exact, case-sensitive match).
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Whether this key grants every one of `scopes`.
    pub fn has_all(&self, scopes: &[&str]) -> bool {
        scopes.iter().all(|s| self.has_scope(s))
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey")
            .field("id", &self.id)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// Why a key store could not answer (not "no such key": that is `Ok(None)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyStoreError(pub String);

impl std::fmt::Display for ApiKeyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "API key store unavailable: {}", self.0)
    }
}

impl std::error::Error for ApiKeyStoreError {}

/// Where [`ApiKeyAuth`] looks keys up, by the SHA-256 digest of the key the
/// client presented. Only digests are stored and compared.
///
/// Implement it over a database (an indexed `digest` column) or a cache. A
/// lookup by digest leaks nothing useful through timing — the digest of an
/// unknown key says nothing about any stored key — and [`ApiKeyAuth`]
/// re-checks the returned record's digest in constant time, so a store that
/// matches loosely (a prefix, a case-insensitive collation) cannot admit the
/// wrong key. Return `Err` only when the store itself failed (answered `503`).
pub trait ApiKeyStore: Send + Sync + 'static {
    /// The record whose digest is `digest`, if any.
    fn find<'a>(
        &'a self,
        digest: &'a [u8; 32],
    ) -> BoxFuture<'a, Result<Option<ApiKey>, ApiKeyStoreError>>;
}

/// An in-memory [`ApiKeyStore`]. Lookups compare the digest with **every**
/// record in constant time (no early exit), so timing does not depend on
/// which record matched or how much of a digest did. Keys can be added and
/// revoked while serving.
///
/// ```
/// use ferroly::turbo::{ApiKey, MemoryApiKeyStore};
///
/// let store = MemoryApiKeyStore::new()
///     .with_key(ApiKey::from_key("ci", "k-ci-0123456789abcdef0123456789abcdef").with_scopes(["deploy"]));
/// store.insert(ApiKey::from_key("ops", "k-ops-0123456789abcdef0123456789abcde"));
/// assert_eq!(store.len(), 2);
/// assert!(store.remove("ci"));
/// ```
#[derive(Debug, Default)]
pub struct MemoryApiKeyStore {
    keys: RwLock<Vec<ApiKey>>,
}

impl MemoryApiKeyStore {
    /// An empty store.
    pub fn new() -> Self {
        MemoryApiKeyStore::default()
    }

    /// Adds a key (builder form of [`insert`](Self::insert)).
    pub fn with_key(self, key: ApiKey) -> Self {
        self.insert(key);
        self
    }

    /// Adds a key. A record with the same id is replaced.
    pub fn insert(&self, key: ApiKey) {
        let mut keys = self.keys.write().unwrap_or_else(|e| e.into_inner());
        keys.retain(|k| k.id != key.id);
        keys.push(key);
    }

    /// Revokes the key with this id; `false` if there was none.
    pub fn remove(&self, id: &str) -> bool {
        let mut keys = self.keys.write().unwrap_or_else(|e| e.into_inner());
        let before = keys.len();
        keys.retain(|k| k.id != id);
        keys.len() != before
    }

    /// The number of stored keys.
    pub fn len(&self) -> usize {
        self.keys.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lookup(&self, digest: &[u8; 32]) -> Option<ApiKey> {
        let keys = self.keys.read().unwrap_or_else(|e| e.into_inner());
        let mut found = None;
        for key in keys.iter() {
            // Every record is compared; the result is only selected.
            let matches = ct_eq(&key.digest, digest);
            if matches && found.is_none() {
                found = Some(key.clone());
            }
        }
        found
    }
}

impl ApiKeyStore for MemoryApiKeyStore {
    fn find<'a>(
        &'a self,
        digest: &'a [u8; 32],
    ) -> BoxFuture<'a, Result<Option<ApiKey>, ApiKeyStoreError>> {
        let found = self.lookup(digest);
        Box::pin(async move { Ok(found) })
    }
}

impl<S: ApiKeyStore> ApiKeyStore for Arc<S> {
    fn find<'a>(
        &'a self,
        digest: &'a [u8; 32],
    ) -> BoxFuture<'a, Result<Option<ApiKey>, ApiKeyStoreError>> {
        (**self).find(digest)
    }
}

/// Why [`ApiKeyAuth::authenticate`] refused a request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiKeyError {
    /// No key was presented (`401`).
    Missing,
    /// The presented key is unknown or revoked (`401`).
    Invalid,
    /// The key is valid but lacks a required scope (`403`).
    InsufficientScope,
    /// The store failed (`503`).
    Unavailable(String),
}

impl std::fmt::Display for ApiKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiKeyError::Missing => f.write_str("missing API key"),
            ApiKeyError::Invalid => f.write_str("invalid API key"),
            ApiKeyError::InsufficientScope => f.write_str("API key lacks a required scope"),
            ApiKeyError::Unavailable(why) => write!(f, "API key store unavailable: {why}"),
        }
    }
}

impl std::error::Error for ApiKeyError {}

/// An API-key authenticator: reads the key from a header, looks its SHA-256
/// digest up in an [`ApiKeyStore`], compares in constant time and checks
/// required scopes. Install it router-wide with [`Router::api_key`] or for a
/// group with [`Group::api_key`].
///
/// | Outcome | Response |
/// |---|---|
/// | no key | `401`, `WWW-Authenticate: ApiKey header="x-api-key"` |
/// | unknown or revoked key | `401` (the same body: never says why) |
/// | the key header repeated (or `Authorization`, with [`allow_bearer`](Self::allow_bearer)) | `401`, even when [`optional`](Self::optional) |
/// | key without a required scope | `403` |
/// | the store failed | `503` |
/// | accepted | the [`ApiKey`] record is stored for [`Ctx::api_key`] |
///
/// ```
/// use std::sync::Arc;
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{ApiKey, ApiKeyAuth, MemoryApiKeyStore, Router};
///
/// let store = Arc::new(MemoryApiKeyStore::new());
/// store.insert(ApiKey::from_key("ci", "k-ci-0123456789abcdef0123456789abcdef").with_scopes(["deploy"]));
///
/// let _router = Router::new().group("/deploy", |g| {
///     g.api_key(ApiKeyAuth::new(store.clone()).require_scopes(&["deploy"]))
///         .post("/", |ctx| async move {
///             let who = ctx.api_key().map(|k| k.id().to_string()).unwrap_or_default();
///             HttpResponse::text(StatusCode::OK, format!("deploying for {who}"))
///         })
/// });
/// ```
#[derive(Clone)]
#[must_use]
pub struct ApiKeyAuth {
    store: Arc<dyn ApiKeyStore>,
    header: String,
    allow_bearer: bool,
    required: Vec<String>,
    optional: bool,
}

impl std::fmt::Debug for ApiKeyAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuth")
            .field("header", &self.header)
            .field("allow_bearer", &self.allow_bearer)
            .field("required", &self.required)
            .field("optional", &self.optional)
            .finish_non_exhaustive()
    }
}

impl ApiKeyAuth {
    /// Keys from the `X-API-Key` header, looked up in `store`.
    pub fn new(store: impl ApiKeyStore) -> Self {
        ApiKeyAuth {
            store: Arc::new(store),
            header: API_KEY_HEADER.to_string(),
            allow_bearer: false,
            required: Vec::new(),
            optional: false,
        }
    }

    /// Reads the key from `name` instead of `X-API-Key` (case-insensitive).
    pub fn header(mut self, name: impl Into<String>) -> Self {
        self.header = name.into().to_ascii_lowercase();
        self
    }

    /// Also accepts the key as `Authorization: Bearer <key>` when the header
    /// is absent. Default `false` (keep bearer tokens for JWTs).
    pub fn allow_bearer(mut self, allow: bool) -> Self {
        self.allow_bearer = allow;
        self
    }

    /// Requires the key to grant **all** of `scopes` (`403` otherwise).
    pub fn require_scopes(mut self, scopes: &[&str]) -> Self {
        self.required
            .extend(scopes.iter().map(|scope| (*scope).to_string()));
        self
    }

    /// Whether a request **without** a key is let through (no
    /// [`Ctx::api_key`]) instead of answered `401`. A presented key is always
    /// verified. Default `false`.
    pub fn optional(mut self, optional: bool) -> Self {
        self.optional = optional;
        self
    }

    /// The key the request presents, if any; `Err` when the request is
    /// ambiguous — the key header, or the `Authorization` header it may be
    /// read from, occurs more than once. A gateway in front may have checked
    /// one instance while this would read another, so neither is trusted.
    fn presented<'a>(&self, ctx: &'a Ctx) -> Result<Option<&'a str>, ()> {
        let repeated = |name: &str| ctx.header_all(name).nth(1).is_some();
        if repeated(&self.header) || (self.allow_bearer && repeated("authorization")) {
            return Err(());
        }
        let from_header = ctx
            .header(&self.header)
            .map(str::trim)
            .filter(|k| !k.is_empty());
        Ok(from_header.or_else(|| {
            if !self.allow_bearer {
                return None;
            }
            let (scheme, key) = ctx.header("authorization")?.split_once(' ')?;
            let key = key.trim();
            (scheme.eq_ignore_ascii_case("bearer") && !key.is_empty()).then_some(key)
        }))
    }

    /// Checks a presented key: digest, store lookup, constant-time
    /// comparison of the digests, then the required scopes.
    pub async fn authenticate(&self, key: &str) -> Result<ApiKey, ApiKeyError> {
        let digest = hash_api_key(key);
        let record = self
            .store
            .find(&digest)
            .await
            .map_err(|e| ApiKeyError::Unavailable(e.0))?
            .ok_or(ApiKeyError::Invalid)?;
        // Never trust the store's notion of "equal".
        if !ct_eq(record.digest(), &digest) {
            return Err(ApiKeyError::Invalid);
        }
        if !self.required.iter().all(|scope| record.has_scope(scope)) {
            return Err(ApiKeyError::InsufficientScope);
        }
        Ok(record)
    }

    fn into_filter(
        self,
    ) -> impl Fn(&Ctx) -> BoxFuture<'static, Option<HttpResponse>> + Send + Sync + 'static {
        move |ctx: &Ctx| {
            let key = self.presented(ctx).map(|key| key.map(str::to_string));
            let extensions = ctx.extensions();
            let auth = self.clone();
            Box::pin(async move {
                let Ok(key) = key else {
                    return Some(auth.challenge("401 invalid API key"));
                };
                let Some(key) = key else {
                    return if auth.optional {
                        None
                    } else {
                        Some(auth.challenge("401 missing API key"))
                    };
                };
                match auth.authenticate(&key).await {
                    Ok(record) => {
                        extensions.insert(record);
                        None
                    }
                    Err(ApiKeyError::InsufficientScope) => Some(HttpResponse::text(
                        StatusCode::FORBIDDEN,
                        "403 insufficient permissions",
                    )),
                    Err(ApiKeyError::Unavailable(_)) => Some(HttpResponse::text(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "503 authentication unavailable",
                    )),
                    Err(_) => Some(auth.challenge("401 invalid API key")),
                }
            })
        }
    }

    fn challenge(&self, body: &'static str) -> HttpResponse {
        let header: String = self
            .header
            .chars()
            .filter(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\\'))
            .collect();
        HttpResponse::text(StatusCode::UNAUTHORIZED, body)
            .header("WWW-Authenticate", format!("ApiKey header=\"{header}\""))
    }
}

impl Ctx {
    /// The API key record an [`ApiKeyAuth`] accepted for this request.
    pub fn api_key(&self) -> Option<Arc<ApiKey>> {
        self.get::<ApiKey>()
    }
}

impl Router {
    /// Sets an [`ApiKeyAuth`] as the router's authenticator (it runs before
    /// all filters, for every request). It replaces any other authenticator
    /// (`Router::jwt` included); use [`Group::api_key`] to combine them on
    /// different prefixes.
    pub fn api_key(self, auth: ApiKeyAuth) -> Self {
        self.auth_async(auth.into_filter())
    }
}

impl Group {
    /// Requires a valid API key for requests under this group's prefix (as a
    /// group filter; see the [scoping guarantee](Group#scoping-guarantee)).
    pub fn api_key(self, auth: ApiKeyAuth) -> Self {
        self.filter_async(auth.into_filter())
    }
}

/// A filter that requires the accepted API key to grant **all** of
/// `required` scopes: `401` when no [`ApiKeyAuth`] accepted a key, `403`
/// when a scope is missing. For per-route checks on top of an authenticator.
pub fn require_api_key_scopes(
    required: &[&str],
) -> impl Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static {
    let required: Vec<String> = required.iter().map(|s| (*s).to_string()).collect();
    move |ctx: &Ctx| {
        let Some(key) = ctx.api_key() else {
            return Some(
                HttpResponse::text(StatusCode::UNAUTHORIZED, "401 authentication required")
                    .header("WWW-Authenticate", "ApiKey"),
            );
        };
        if required.iter().all(|scope| key.has_scope(scope)) {
            None
        } else {
            Some(HttpResponse::text(
                StatusCode::FORBIDDEN,
                "403 insufficient permissions",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_hold_only_the_digest() {
        let record = ApiKey::from_key("a", "secret-key").with_scopes(["x", "y"]);
        assert_eq!(record.digest(), &hash_api_key("secret-key"));
        assert!(record.has_all(&["x", "y"]) && !record.has_scope("z"));
        let debug = format!("{record:?}");
        assert!(!debug.contains("secret-key"), "{debug}");
        let (key, generated) = ApiKey::generate("g").unwrap();
        assert_eq!(key.len(), 43);
        assert_eq!(generated.digest(), &hash_api_key(&key));
    }

    #[test]
    fn the_memory_store_finds_by_digest_and_revokes() {
        let store = MemoryApiKeyStore::new()
            .with_key(ApiKey::from_key("a", "key-a"))
            .with_key(ApiKey::from_key("b", "key-b"));
        assert_eq!(store.lookup(&hash_api_key("key-b")).unwrap().id(), "b");
        assert!(store.lookup(&hash_api_key("key-c")).is_none());
        // Replacing by id, then revoking.
        store.insert(ApiKey::from_key("b", "key-b2"));
        assert_eq!(store.len(), 2);
        assert!(store.lookup(&hash_api_key("key-b")).is_none());
        assert!(store.remove("b"));
        assert!(!store.remove("b"));
        assert!(store.lookup(&hash_api_key("key-b2")).is_none());
    }

    /// A store that answers loosely (returns a record for any digest) cannot
    /// admit a wrong key: the digests are re-compared.
    #[tokio::test]
    async fn a_loose_store_cannot_admit_the_wrong_key() {
        struct Loose;
        impl ApiKeyStore for Loose {
            fn find<'a>(
                &'a self,
                _digest: &'a [u8; 32],
            ) -> BoxFuture<'a, Result<Option<ApiKey>, ApiKeyStoreError>> {
                Box::pin(async { Ok(Some(ApiKey::from_key("x", "the-real-key"))) })
            }
        }
        let auth = ApiKeyAuth::new(Loose);
        assert_eq!(
            auth.authenticate("a-guess").await,
            Err(ApiKeyError::Invalid)
        );
        assert_eq!(auth.authenticate("the-real-key").await.unwrap().id(), "x");
    }
}
