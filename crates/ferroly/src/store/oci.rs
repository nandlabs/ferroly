//! OCI / Docker registries (the OCI Distribution Spec v2, pull side):
//! manifests by tag or digest, blobs by digest into the cache, with the
//! registry token flow.
//!
//! Authentication follows the registry's challenge: a request answered
//! `401` with `WWW-Authenticate: Bearer realm="…",service="…",scope="…"` is
//! retried with a token fetched from the realm (anonymously, or with the
//! configured basic credentials); a `Basic` challenge is answered with the
//! credentials directly. Only a challenge from the registry's own origin is
//! answered: a `401` from the storage backend a blob redirected to is an
//! error, so the credentials never go to a realm the storage names. Tokens
//! are cached per registry and scope until they expire. Blob downloads
//! follow the registry's redirect to its storage backend without forwarding
//! the credentials there.
//!
//! ```no_run
//! use ferroly::store::oci::{OciClient, Reference};
//! use ferroly::store::Cache;
//!
//! # async fn demo() -> Result<(), ferroly::store::StoreError> {
//! let oci = OciClient::new();
//! let cache = Cache::open("/var/cache/oci")?;
//! let reference = Reference::parse("ghcr.io/org/model-weights:v1")?;
//! let pulled = oci.pull(&cache, &reference).await?;
//! for blob in &pulled.blobs {
//!     println!("{} -> {}", blob.digest, blob.path.display());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Not included: pushing, the OAuth2 `POST` token flow (the `GET` token
//! endpoint is used, which every registry supports), credential helpers and
//! `~/.docker/config.json`, picking a platform from an index automatically,
//! signature verification (cosign / notation) and the referrers API.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroly::codec::{json, Limits, Value};
use ferroly::hash::{base64, Secret};
use ferroly::http::{percent_encode, Client, Method, Request, Response, Uri};

use super::fetch::{adapter_client, has_content, ranged, read_capped, truncate, ERROR_BODY_LIMIT};
use super::redirect::{send_following, CrossOrigin};
use super::{Cache, Digest, Fetched, StoreError, Stored};

/// The `Accept` header sent for manifests: OCI image manifest and index,
/// and the Docker v2 manifest and manifest list.
pub const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.docker.distribution.manifest.list.v2+json";

/// OCI image manifest media type.
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// OCI image index media type.
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
/// Docker v2 schema 2 manifest media type.
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
/// Docker v2 manifest list media type.
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// Largest manifest accepted (the size registries are required to accept).
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
/// Largest token response accepted.
const MAX_TOKEN_BYTES: u64 = 64 * 1024;
/// Most descriptors read from one manifest.
const MAX_DESCRIPTORS: usize = 10_000;
/// Tokens kept before the cache is emptied.
const MAX_CACHED_TOKENS: usize = 256;
/// Token lifetime when the token response does not say (the spec's default).
const DEFAULT_TOKEN_SECS: u64 = 60;

/// A reference to a repository, optionally at a tag or digest:
/// `[registry/]repository[:tag][@sha256:…]`.
///
/// Without a registry the reference is a Docker Hub one (`ubuntu` is
/// `docker.io/library/ubuntu`). Without a tag or digest the tag is
/// `latest`.
///
/// ```
/// use ferroly::store::oci::Reference;
///
/// let r = Reference::parse("ghcr.io/org/app:1.2").unwrap();
/// assert_eq!((r.registry(), r.repository(), r.tag()), ("ghcr.io", "org/app", Some("1.2")));
/// let r = Reference::parse("ubuntu").unwrap();
/// assert_eq!(r.to_string(), "docker.io/library/ubuntu:latest");
/// let r = Reference::parse("localhost:5000/x@sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").unwrap();
/// assert_eq!(r.registry(), "localhost:5000");
/// assert!(r.digest().is_some());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<Digest>,
}

impl Reference {
    /// A reference to `repository` on `registry` (`host[:port]`), at no
    /// tag or digest yet.
    pub fn new(
        registry: impl Into<String>,
        repository: impl Into<String>,
    ) -> Result<Reference, StoreError> {
        let registry = registry.into();
        let repository = repository.into();
        validate_registry(&registry)?;
        validate_repository(&repository)?;
        Ok(Reference {
            registry,
            repository,
            tag: None,
            digest: None,
        })
    }

    /// Parses `[registry/]repository[:tag][@sha256:…]`.
    pub fn parse(text: &str) -> Result<Reference, StoreError> {
        if text.len() > 2048 {
            return Err(StoreError::Config("reference too long".into()));
        }
        let (rest, digest) = match text.split_once('@') {
            Some((rest, d)) => (rest, Some(Digest::parse(d)?)),
            None => (text, None),
        };
        // A tag is after the last ':' that follows the last '/'.
        let last_slash = rest.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match rest[last_slash..].rfind(':') {
            Some(i) => (
                &rest[..last_slash + i],
                Some(rest[last_slash + i + 1..].to_string()),
            ),
            None => (rest, None),
        };
        let (registry, repository) = match name.split_once('/') {
            Some((first, path))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), path.to_string())
            }
            _ => ("docker.io".to_string(), name.to_string()),
        };
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        let mut reference = Reference::new(registry, repository)?;
        if let Some(tag) = tag {
            reference = reference.with_tag(tag)?;
        } else if digest.is_none() {
            reference.tag = Some("latest".into());
        }
        reference.digest = digest;
        Ok(reference)
    }

    /// The same repository at `tag`.
    pub fn with_tag(mut self, tag: impl Into<String>) -> Result<Reference, StoreError> {
        let tag = tag.into();
        let ok = (1..=128).contains(&tag.len())
            && tag
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !ok {
            return Err(StoreError::Config(format!(
                "invalid tag {:?}",
                truncate(&tag, 80)
            )));
        }
        self.tag = Some(tag);
        Ok(self)
    }

    /// The same repository at `digest` (which takes precedence over a tag).
    pub fn with_digest(mut self, digest: Digest) -> Reference {
        self.digest = Some(digest);
        self
    }

    /// The registry (`host[:port]`), as written.
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// The repository path (`org/name`).
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// The tag, if any.
    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// The digest, if any.
    pub fn digest(&self) -> Option<&Digest> {
        self.digest.as_ref()
    }

    /// The host the registry API is served from (`docker.io` is
    /// `registry-1.docker.io`).
    pub fn api_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    /// The digest if set, otherwise the tag: what goes in a manifest URL.
    fn manifest_ref(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(d), _) => d.to_string(),
            (None, Some(t)) => t.clone(),
            (None, None) => "latest".into(),
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

/// A platform an index entry is for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Platform {
    /// CPU architecture (`amd64`, `arm64`, …).
    pub architecture: String,
    /// Operating system (`linux`, …).
    pub os: String,
    /// CPU variant (`v8`, …), if any.
    pub variant: Option<String>,
}

/// A content descriptor: what a manifest says about a blob or a child
/// manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Descriptor {
    /// The content's media type.
    pub media_type: String,
    /// The content digest.
    pub digest: Digest,
    /// The content size in bytes.
    pub size: u64,
    /// The platform (index entries only).
    pub platform: Option<Platform>,
    /// The `org.opencontainers.image.title` annotation (ORAS file name), if
    /// any.
    pub title: Option<String>,
}

/// A manifest or index, verified against its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Manifest {
    /// The media type (from the document, or the `Content-Type` header).
    pub media_type: String,
    /// The SHA-256 of [`bytes`](Manifest::bytes).
    pub digest: Digest,
    /// The manifest exactly as served.
    pub bytes: Vec<u8>,
    /// An image manifest's config blob.
    pub config: Option<Descriptor>,
    /// An image manifest's layers.
    pub layers: Vec<Descriptor>,
    /// An index's child manifests.
    pub manifests: Vec<Descriptor>,
}

impl Manifest {
    /// Whether this is an index / manifest list (it has child manifests
    /// rather than layers).
    pub fn is_index(&self) -> bool {
        self.media_type == OCI_INDEX
            || self.media_type == DOCKER_MANIFEST_LIST
            || (!self.manifests.is_empty() && self.layers.is_empty())
    }

    /// Whether this is an image manifest [`OciClient::pull`] can pull: an
    /// OCI image manifest or a Docker v2 schema 2 manifest (by media type,
    /// or, when neither the document nor the response names one, by its
    /// shape) with a config blob. Docker schema 1, OCI artifact manifests
    /// and unknown media types are not.
    pub fn is_image(&self) -> bool {
        let known = self.media_type == OCI_MANIFEST
            || self.media_type == DOCKER_MANIFEST
            || (self.media_type.is_empty() && self.manifests.is_empty());
        known && self.config.is_some()
    }
}

/// The result of [`OciClient::pull`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Pulled {
    /// The image manifest.
    pub manifest: Manifest,
    /// The manifest itself, stored in the cache under its digest.
    pub manifest_path: std::path::PathBuf,
    /// The config blob (first) and the layers, in manifest order.
    pub blobs: Vec<Stored>,
}

#[derive(Clone)]
struct CachedToken {
    token: Secret,
    expires: Instant,
}

/// A registry client. Cheap to clone (clones share the token cache, until
/// [`with_basic_auth`](OciClient::with_basic_auth) gives one its own);
/// `Debug` redacts the credentials.
#[derive(Clone)]
pub struct OciClient {
    http: Client,
    credentials: Option<(String, Secret)>,
    plain_http: bool,
    tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
}

impl fmt::Debug for OciClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OciClient")
            .field("username", &self.credentials.as_ref().map(|c| &c.0))
            .field("password", &self.credentials.as_ref().map(|c| &c.1))
            .field("plain_http", &self.plain_http)
            .finish_non_exhaustive()
    }
}

impl Default for OciClient {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed `WWW-Authenticate` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Challenge {
    scheme: String,
    params: Vec<(String, String)>,
}

impl Challenge {
    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl OciClient {
    /// An anonymous client over HTTPS.
    pub fn new() -> Self {
        OciClient {
            http: Client::new(),
            credentials: None,
            plain_http: false,
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Credentials for basic authentication and for token requests.
    ///
    /// The client gets a token cache of its own, so tokens obtained with
    /// these credentials are never shared with the client it was cloned
    /// from, nor the other way round.
    pub fn with_basic_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.credentials = Some((username.into(), Secret::from(password.into())));
        self.tokens = Arc::new(Mutex::new(HashMap::new()));
        self
    }

    /// The HTTP client to use (timeouts, proxy, TLS roots). Its own redirect
    /// following and response decompression are turned off for registry
    /// requests: this client follows redirects itself (never forwarding
    /// credentials to another origin, and refusing `https` → `http`), and
    /// verifies the bytes exactly as served.
    pub fn with_client(mut self, client: Client) -> Self {
        self.http = adapter_client(client);
        self
    }

    /// Talks plain `http://` to registries (a local development registry).
    /// Off by default. Credentials and tokens are then sent unencrypted.
    pub fn plain_http(mut self, enabled: bool) -> Self {
        self.plain_http = enabled;
        self
    }

    fn base_url(&self, reference: &Reference) -> String {
        let scheme = if self.plain_http { "http" } else { "https" };
        format!(
            "{scheme}://{}/v2/{}",
            reference.api_host(),
            reference.repository
        )
    }

    /// The `Basic` credentials header. The joined `user:password` is wiped;
    /// the header value itself is not, by design: it goes into a request's
    /// header map, its clones and the connection's write buffer, none of
    /// which can be wiped, so wiping only this copy would promise nothing.
    /// The same holds for every `Bearer` value sent here and in the Hub
    /// adapter.
    fn basic_header(&self) -> Option<String> {
        self.credentials.as_ref().map(|(user, pass)| {
            let mut raw = Vec::with_capacity(user.len() + 1 + pass.len());
            raw.extend_from_slice(user.as_bytes());
            raw.push(b':');
            raw.extend_from_slice(pass.expose());
            let raw = Secret::from(raw);
            format!("Basic {}", base64::encode(raw.expose()))
        })
    }

    fn token_key(reference: &Reference) -> String {
        format!(
            "{}|repository:{}:pull",
            reference.api_host(),
            reference.repository
        )
    }

    fn cached_token(&self, key: &str) -> Option<Secret> {
        let tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        tokens
            .get(key)
            .filter(|t| t.expires > Instant::now())
            .map(|t| t.token.clone())
    }

    /// Sends `req` for `reference`'s repository, answering an auth
    /// challenge once.
    async fn send_authed(
        &self,
        reference: &Reference,
        req: Request,
    ) -> Result<Response, StoreError> {
        let key = Self::token_key(reference);
        let mut first = req.clone();
        if let Some(token) = self.cached_token(&key) {
            first.headers.set(
                "authorization",
                format!("Bearer {}", token.expose_str().unwrap_or_default()),
            );
        }
        let resp = send_following(&self.http, first, CrossOrigin::Follow).await?;
        if resp.status().as_u16() != 401 {
            return Ok(resp);
        }
        if !resp.uri().same_origin(&req.uri) {
            // The storage backend a redirect led to wants credentials of its
            // own. Answering its challenge would send ours to whatever realm
            // it names, and cache the token it hands back for the registry.
            return Err(oci_error(resp).await);
        }
        let challenge = resp
            .headers()
            .get_all("www-authenticate")
            .flat_map(parse_challenges)
            .min_by_key(|c| !c.scheme.eq_ignore_ascii_case("bearer"));
        drop(resp);
        let authorization = match challenge {
            Some(c) if c.scheme.eq_ignore_ascii_case("bearer") => {
                let (token, lifetime) = self.fetch_token(reference, &c).await?;
                let header = format!("Bearer {}", token.expose_str().unwrap_or_default());
                let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
                if tokens.len() >= MAX_CACHED_TOKENS {
                    tokens.clear();
                }
                tokens.insert(
                    key,
                    CachedToken {
                        token,
                        expires: Instant::now() + lifetime,
                    },
                );
                header
            }
            Some(c) if c.scheme.eq_ignore_ascii_case("basic") => {
                self.basic_header().ok_or_else(|| {
                    StoreError::Auth(format!("{} requires credentials", reference.registry()))
                })?
            }
            _ => {
                return Err(StoreError::Auth(format!(
                    "{} answered 401 without a usable challenge",
                    reference.registry()
                )))
            }
        };
        let registry = req.uri.clone();
        let mut retry = req;
        retry.headers.set("authorization", authorization);
        let resp = send_following(&self.http, retry, CrossOrigin::Follow).await?;
        if resp.status().as_u16() == 401 {
            if !resp.uri().same_origin(&registry) {
                return Err(oci_error(resp).await);
            }
            return Err(StoreError::Auth(format!(
                "{} rejected the credentials for {}",
                reference.registry(),
                reference.repository()
            )));
        }
        Ok(resp)
    }

    /// Fetches a bearer token from the challenge's realm; returns it with
    /// how long to keep it.
    async fn fetch_token(
        &self,
        reference: &Reference,
        challenge: &Challenge,
    ) -> Result<(Secret, Duration), StoreError> {
        let realm = challenge
            .param("realm")
            .ok_or_else(|| StoreError::Auth("bearer challenge without a realm".into()))?;
        let realm_uri = Uri::parse(realm)
            .map_err(|_| StoreError::Auth("bearer challenge with an invalid realm".into()))?;
        if !self.plain_http && !realm_uri.is_tls() {
            return Err(StoreError::Auth(
                "refusing a plain-http token realm for an https registry".into(),
            ));
        }
        let own_scope = format!("repository:{}:pull", reference.repository);
        let scope = challenge.param("scope").unwrap_or(&own_scope);
        let mut url = realm.to_string();
        let mut sep = if realm_uri.query.is_some() { '&' } else { '?' };
        if let Some(service) = challenge.param("service") {
            url.push_str(&format!("{sep}service={}", percent_encode(service)));
            sep = '&';
        }
        url.push_str(&format!("{sep}scope={}", percent_encode(scope)));
        let mut builder = Request::builder(Method::Get, &url)?;
        if let Some(basic) = self.basic_header() {
            builder = builder.header("authorization", basic);
        }
        let resp = send_following(&self.http, builder.build(), CrossOrigin::Follow).await?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(StoreError::Auth(format!(
                "token request to {} failed with status {status}",
                realm_uri.host
            )));
        }
        let body = Secret::from(read_capped(resp, MAX_TOKEN_BYTES).await?);
        let value = json::from_slice_with(body.expose(), json_limits())
            .map_err(|_| StoreError::Auth("malformed token response".into()))?;
        let token = value
            .get("token")
            .or_else(|| value.get("access_token"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| StoreError::Auth("token response without a token".into()))?;
        let lifetime = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TOKEN_SECS)
            .clamp(1, 24 * 3600);
        // Renew a little early.
        let lifetime = Duration::from_secs(lifetime).saturating_sub(Duration::from_secs(5));
        Ok((Secret::from(token.to_string()), lifetime))
    }

    /// Fetches and verifies the manifest `reference` names (its digest if it
    /// has one, otherwise its tag).
    ///
    /// The SHA-256 of the bytes served is checked against the reference's
    /// digest and against `Docker-Content-Digest` when the registry sends
    /// it.
    pub async fn manifest(&self, reference: &Reference) -> Result<Manifest, StoreError> {
        let url = format!(
            "{}/manifests/{}",
            self.base_url(reference),
            reference.manifest_ref()
        );
        let req = Request::builder(Method::Get, &url)?
            .header("accept", MANIFEST_ACCEPT)
            .build();
        let resp = self.send_authed(reference, req).await?;
        if !resp.status().is_success() {
            return Err(oci_error(resp).await);
        }
        let header_digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|d| Digest::parse(d.trim()).ok());
        let content_type = resp
            .headers()
            .get("content-type")
            .map(|t| t.split(';').next().unwrap_or("").trim().to_string());
        let bytes = read_capped(resp, MAX_MANIFEST_BYTES).await?;
        let digest = Digest::of(&bytes);
        for expected in [reference.digest().copied(), header_digest]
            .into_iter()
            .flatten()
        {
            if !expected.ct_eq(&digest) {
                return Err(StoreError::DigestMismatch {
                    expected: expected.to_string(),
                    actual: digest.to_string(),
                });
            }
        }
        parse_manifest(bytes, digest, content_type)
    }

    fn blob_request(&self, reference: &Reference, digest: &Digest) -> Result<Request, StoreError> {
        let url = format!("{}/blobs/{digest}", self.base_url(reference));
        Ok(Request::builder(Method::Get, &url)?.build())
    }

    /// `GET`s a blob of `reference`'s repository, following the registry's
    /// redirect, with a streaming body. Not verified: use
    /// [`blob_to_cache`](OciClient::blob_to_cache) for that.
    pub async fn blob(
        &self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<Response, StoreError> {
        let req = self.blob_request(reference, digest)?;
        let resp = self.send_authed(reference, req).await?;
        if !resp.status().is_success() {
            return Err(oci_error(resp).await);
        }
        Ok(resp)
    }

    /// A [`Fetched`] for a blob from byte `offset` — the building block for
    /// [`Cache::get_or_fetch`].
    pub async fn fetch_blob(
        &self,
        reference: &Reference,
        digest: &Digest,
        offset: u64,
    ) -> Result<Fetched, StoreError> {
        let req = ranged(&self.blob_request(reference, digest)?, offset);
        let resp = self.send_authed(reference, req).await?;
        if !has_content(resp.status().as_u16(), offset) {
            return Err(oci_error(resp).await);
        }
        Fetched::from_response(resp, offset).await
    }

    /// Downloads the blob `descriptor` names into `cache` (verified against
    /// its digest and size; resumed when interrupted; no request on a hit).
    pub async fn blob_to_cache(
        &self,
        cache: &Cache,
        reference: &Reference,
        descriptor: &Descriptor,
    ) -> Result<std::path::PathBuf, StoreError> {
        let digest = descriptor.digest;
        cache
            .get_or_fetch_sized(&digest, descriptor.size, |offset| {
                self.fetch_blob(reference, &digest, offset)
            })
            .await
    }

    /// Pulls an image manifest and all its blobs (config first, then the
    /// layers) into `cache`. The manifest is stored too, under its digest.
    ///
    /// An index (multi-platform image) is a [`StoreError::Config`] error:
    /// pick an entry from [`Manifest::manifests`] and pull
    /// `reference.with_digest(entry.digest)`. Any other manifest that is not
    /// an image ([`Manifest::is_image`]: Docker schema 1, an OCI artifact
    /// manifest, an unknown media type, no config) is a
    /// [`StoreError::Protocol`] error rather than an empty pull.
    pub async fn pull(&self, cache: &Cache, reference: &Reference) -> Result<Pulled, StoreError> {
        let manifest = self.manifest(reference).await?;
        if manifest.is_index() {
            return Err(StoreError::Config(format!(
                "{reference} is an index of {} manifests; pull one of them by digest",
                manifest.manifests.len()
            )));
        }
        if !manifest.is_image() {
            let kind = if manifest.media_type.is_empty() {
                "manifest without a media type or config".to_string()
            } else {
                format!("{:?} manifest", truncate(&manifest.media_type, 120))
            };
            return Err(StoreError::Protocol(format!(
                "{reference} is a {kind}, not an image manifest"
            )));
        }
        let bytes = manifest.bytes.clone();
        let manifest_path = cache
            .get_or_fetch(&manifest.digest, move |_| {
                std::future::ready(Ok(Fetched::from_bytes(bytes.clone())))
            })
            .await?;
        let mut blobs = Vec::new();
        for descriptor in manifest.config.iter().chain(manifest.layers.iter()) {
            let path = self.blob_to_cache(cache, reference, descriptor).await?;
            blobs.push(Stored {
                digest: descriptor.digest,
                path,
            });
        }
        Ok(Pulled {
            manifest,
            manifest_path,
            blobs,
        })
    }
}

fn json_limits() -> Limits {
    Limits::new()
        .with_max_depth(32)
        .with_max_input_bytes(MAX_MANIFEST_BYTES as usize)
        .with_max_entries(200_000)
}

/// An OCI error body: `{"errors":[{"code":"…","message":"…"}]}`.
async fn oci_error(resp: Response) -> StoreError {
    let status = resp.status().as_u16();
    let body = read_capped(resp, ERROR_BODY_LIMIT)
        .await
        .unwrap_or_default();
    let first = json::from_slice_with(&body, json_limits())
        .ok()
        .and_then(|v| v.get("errors").and_then(|e| e.get_index(0)).cloned());
    let field = |name: &str| {
        first
            .as_ref()
            .and_then(|e| e.get(name))
            .and_then(Value::as_str)
            .map(|s| truncate(s, 512))
            .unwrap_or_default()
    };
    let mut message = field("message");
    if message.is_empty() && first.is_none() {
        message = truncate(String::from_utf8_lossy(&body).trim(), 512);
    }
    StoreError::remote(status, field("code"), message)
}

fn parse_manifest(
    bytes: Vec<u8>,
    digest: Digest,
    content_type: Option<String>,
) -> Result<Manifest, StoreError> {
    let value = json::from_slice_with(&bytes, json_limits())
        .map_err(|e| StoreError::Protocol(format!("malformed manifest: {e}")))?;
    if value.as_object().is_none() {
        return Err(StoreError::Protocol("manifest is not a JSON object".into()));
    }
    let media_type = value
        .get("mediaType")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(content_type)
        .unwrap_or_default();
    let config = match value.get("config") {
        Some(c) => Some(parse_descriptor(c)?),
        None => None,
    };
    let list = |name: &str| -> Result<Vec<Descriptor>, StoreError> {
        let Some(items) = value.get(name) else {
            return Ok(Vec::new());
        };
        let items = items
            .as_array()
            .ok_or_else(|| StoreError::Protocol(format!("manifest `{name}` is not an array")))?;
        if items.len() > MAX_DESCRIPTORS {
            return Err(StoreError::Protocol(format!(
                "manifest lists more than {MAX_DESCRIPTORS} {name}"
            )));
        }
        items.iter().map(parse_descriptor).collect()
    };
    Ok(Manifest {
        layers: list("layers")?,
        manifests: list("manifests")?,
        media_type,
        digest,
        bytes,
        config,
    })
}

fn parse_descriptor(value: &Value) -> Result<Descriptor, StoreError> {
    let bad = |what: &str| StoreError::Protocol(format!("descriptor {what}"));
    let digest = value
        .get("digest")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("without a digest"))?;
    let digest = Digest::parse(digest)
        .map_err(|_| bad("with an unsupported digest (only sha256 is supported)"))?;
    let size = value
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("without a valid size"))?;
    let platform = value.get("platform").map(|p| Platform {
        architecture: str_field(p, "architecture").unwrap_or_default(),
        os: str_field(p, "os").unwrap_or_default(),
        variant: str_field(p, "variant"),
    });
    let title = value
        .get("annotations")
        .and_then(|a| str_field(a, "org.opencontainers.image.title"));
    Ok(Descriptor {
        media_type: str_field(value, "mediaType").unwrap_or_default(),
        digest,
        size,
        platform,
        title,
    })
}

fn str_field(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).map(str::to_string)
}

/// Parses a `WWW-Authenticate` header value into its challenges
/// (`scheme param="value", …[, scheme …]`). Malformed input yields what
/// was parsed before the error.
fn parse_challenges(header: &str) -> Vec<Challenge> {
    let bytes = header.as_bytes();
    let mut i = 0;
    let mut out: Vec<Challenge> = Vec::new();
    let is_tchar = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    let skip_ws = |i: &mut usize| {
        while *i < bytes.len() && (bytes[*i] == b' ' || bytes[*i] == b'\t') {
            *i += 1;
        }
    };
    let token = |i: &mut usize| -> Option<String> {
        let start = *i;
        while *i < bytes.len() && is_tchar(bytes[*i]) {
            *i += 1;
        }
        (start < *i).then(|| header[start..*i].to_string())
    };
    loop {
        skip_ws(&mut i);
        while i < bytes.len() && bytes[i] == b',' {
            i += 1;
            skip_ws(&mut i);
        }
        if i >= bytes.len() || out.len() >= 8 {
            return out;
        }
        let Some(word) = token(&mut i) else {
            return out;
        };
        skip_ws(&mut i);
        if i < bytes.len() && bytes[i] == b'=' {
            // A parameter of the current challenge.
            i += 1;
            skip_ws(&mut i);
            let value = if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
                let mut v = String::new();
                loop {
                    match bytes.get(i) {
                        None => return out,
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(b'\\') if i + 1 < bytes.len() => {
                            // A quoted-pair: take the next character whole
                            // (obs-text makes it multi-byte), so `i` stays on
                            // a character boundary.
                            let ch = header[i + 1..].chars().next().unwrap_or('\u{fffd}');
                            v.push(ch);
                            i += 1 + ch.len_utf8();
                        }
                        Some(_) => {
                            // Copy one UTF-8 character.
                            let ch = header[i..].chars().next().unwrap_or('\u{fffd}');
                            v.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                v
            } else {
                match token(&mut i) {
                    Some(v) => v,
                    None => return out,
                }
            };
            match out.last_mut() {
                Some(c) if c.params.len() < 16 => c.params.push((word, value)),
                Some(_) => {}
                None => return out,
            }
        } else {
            out.push(Challenge {
                scheme: word,
                params: Vec::new(),
            });
        }
    }
}

/// Fuzz entry point: the `WWW-Authenticate` challenge parser over arbitrary
/// text (invalid UTF-8 is replaced, as obs-text may arrive). Never panics,
/// stays within its challenge and parameter caps, and never produces more
/// text than it was given. Not a stable API.
#[doc(hidden)]
pub fn __fuzz_challenges(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let challenges = parse_challenges(&text);
    assert!(challenges.len() <= 8, "too many challenges");
    let mut produced = 0;
    for c in &challenges {
        assert!(!c.scheme.is_empty(), "empty scheme");
        assert!(c.params.len() <= 16, "too many parameters");
        produced += c.scheme.len();
        for (name, value) in &c.params {
            assert!(!name.is_empty(), "empty parameter name");
            produced += name.len() + value.len();
        }
        let _ = c.param("realm");
    }
    assert!(
        produced <= text.len(),
        "the parser produced more than its input"
    );
}

fn validate_registry(registry: &str) -> Result<(), StoreError> {
    let ok = !registry.is_empty()
        && registry.len() <= 255
        && registry
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
        && Uri::parse(&format!("https://{registry}/")).is_ok();
    if ok {
        Ok(())
    } else {
        Err(StoreError::Config(format!(
            "invalid registry {:?}",
            truncate(registry, 80)
        )))
    }
}

/// Repository names: `/`-separated components of lower-case letters and
/// digits joined by `.`, `_`, `__` or `-` runs.
fn validate_repository(repository: &str) -> Result<(), StoreError> {
    let component_ok = |c: &str| {
        let b = c.as_bytes();
        !b.is_empty()
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && b.iter().all(|&x| {
                x.is_ascii_lowercase() || x.is_ascii_digit() || matches!(x, b'.' | b'_' | b'-')
            })
    };
    let ok = repository.len() <= 255 && repository.split('/').all(component_ok);
    if ok {
        Ok(())
    } else {
        Err(StoreError::Config(format!(
            "invalid repository {:?}",
            truncate(repository, 80)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references() {
        let r = Reference::parse("registry.example:5000/a/b/c:v1.0").unwrap();
        assert_eq!(r.registry(), "registry.example:5000");
        assert_eq!(r.repository(), "a/b/c");
        assert_eq!(r.tag(), Some("v1.0"));
        let r = Reference::parse("org/app").unwrap();
        assert_eq!(r.registry(), "docker.io");
        assert_eq!(r.api_host(), "registry-1.docker.io");
        assert_eq!(r.repository(), "org/app");
        assert_eq!(r.tag(), Some("latest"));
        let d = Digest::of(b"x");
        let r = Reference::parse(&format!("localhost/app:1@{d}")).unwrap();
        assert_eq!(r.manifest_ref(), d.to_string());
        assert_eq!(r.to_string(), format!("localhost/app:1@{d}"));
        for bad in [
            "",
            "UPPER/case",
            "ghcr.io/org/app:",
            "ghcr.io/org/app:-bad",
            "ghcr.io//app",
            "ghcr.io/org/app@sha256:zz",
            "ghcr.io/org/../app",
            "bad host/app",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Reference::parse(&"a".repeat(5000)).is_err());
    }

    #[test]
    fn challenges() {
        let c = parse_challenges(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:samalba/my-app:pull,push""#,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].scheme, "Bearer");
        assert_eq!(c[0].param("realm"), Some("https://auth.docker.io/token"));
        assert_eq!(c[0].param("service"), Some("registry.docker.io"));
        assert_eq!(
            c[0].param("scope"),
            Some("repository:samalba/my-app:pull,push")
        );

        let c =
            parse_challenges(r#"Basic realm="Registry Realm", Bearer realm="x\"y",service=tok"#);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].scheme, "Basic");
        assert_eq!(c[1].param("realm"), Some("x\"y"));
        assert_eq!(c[1].param("service"), Some("tok"));

        assert!(parse_challenges("").is_empty());
        assert!(parse_challenges("=x").is_empty());
        let c = parse_challenges(r#"Bearer realm="unterminated"#);
        assert!(c[0].params.is_empty());
        let many = "Bearer a=1, ".repeat(100);
        assert!(parse_challenges(&many).len() <= 8);
    }

    #[test]
    fn quoted_pair_with_a_multibyte_character() {
        // ST-1: used to slice inside the `é` and panic.
        let c = parse_challenges(r#"Bearer realm="\é""#);
        assert_eq!(c[0].param("realm"), Some("é"));
        let c = parse_challenges(r#"Bearer realm="a\é\€\😀b",service="s\"x""#);
        assert_eq!(c[0].param("realm"), Some("aé€😀b"));
        assert_eq!(c[0].param("service"), Some("s\"x"));
        for text in [
            r#"Bearer realm="\é"#,
            "Bearer realm=\"\\",
            "Bearer r=\"\\\u{10ffff}\"",
        ] {
            __fuzz_challenges(text.as_bytes());
        }
    }

    #[test]
    fn challenge_parser_survives_arbitrary_text() {
        // A small deterministic generator over the characters that matter
        // to the grammar, plus multi-byte ones in every position.
        let alphabet: Vec<char> = "Bb\"\\=, \t;realmé€😀\u{80}\u{7f}x1".chars().collect();
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 24) as usize;
            let text: String = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            __fuzz_challenges(text.as_bytes());
            __fuzz_challenges(format!("Bearer realm=\"{text}\"").as_bytes());
        }
        // Raw bytes, including invalid UTF-8.
        for _ in 0..5_000 {
            let len = (next() % 32) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            __fuzz_challenges(&bytes);
        }
    }

    #[test]
    fn manifests() {
        let d = Digest::of(b"layer");
        let doc = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}",
                "config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{d}","size":5}},
                "layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{d}","size":5,
                            "annotations":{{"org.opencontainers.image.title":"weights.bin"}}}}]}}"#
        );
        let m = parse_manifest(doc.clone().into_bytes(), Digest::of(doc.as_bytes()), None).unwrap();
        assert!(!m.is_index());
        assert!(m.is_image());
        assert_eq!(m.layers.len(), 1);
        assert_eq!(m.layers[0].title.as_deref(), Some("weights.bin"));
        assert_eq!(m.config.as_ref().unwrap().size, 5);

        let index = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"{OCI_MANIFEST}","digest":"{d}","size":7,
                "platform":{{"architecture":"arm64","os":"linux","variant":"v8"}}}}]}}"#
        );
        let m = parse_manifest(index.into_bytes(), d, Some(OCI_INDEX.into())).unwrap();
        assert!(m.is_index());
        assert!(!m.is_image());

        // ST-7: not images, although they parse.
        let schema1 = r#"{"schemaVersion":1,"name":"x","tag":"v1","fsLayers":[{"blobSum":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}]}"#;
        let schema1_typed = parse_manifest(
            schema1.as_bytes().to_vec(),
            d,
            Some("application/vnd.docker.distribution.manifest.v1+prettyjws".into()),
        )
        .unwrap();
        assert!(!schema1_typed.is_image() && !schema1_typed.is_index());
        let untyped = parse_manifest(schema1.as_bytes().to_vec(), d, None).unwrap();
        assert!(!untyped.is_image());
        let artifact = format!(
            r#"{{"mediaType":"application/vnd.oci.artifact.manifest.v1+json","artifactType":"x","blobs":[{{"digest":"{d}","size":5}}]}}"#
        );
        let artifact = parse_manifest(artifact.into_bytes(), d, None).unwrap();
        assert!(!artifact.is_image());
        let docker = format!(
            r#"{{"schemaVersion":2,"mediaType":"{DOCKER_MANIFEST}","config":{{"digest":"{d}","size":5}},"layers":[]}}"#
        );
        assert!(parse_manifest(docker.into_bytes(), d, None)
            .unwrap()
            .is_image());
        assert_eq!(
            m.manifests[0].platform.as_ref().unwrap().variant.as_deref(),
            Some("v8")
        );

        for bad in [
            "[]",
            "{",
            r#"{"layers":{}}"#,
            r#"{"layers":[{"digest":"sha512:abc","size":1}]}"#,
            r#"{"layers":[{"digest":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":-1}]}"#,
        ] {
            assert!(
                parse_manifest(bad.as_bytes().to_vec(), d, None).is_err(),
                "{bad}"
            );
        }
    }
}
