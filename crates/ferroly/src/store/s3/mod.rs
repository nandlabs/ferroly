//! S3-compatible object storage: `GetObject` (streaming, ranges),
//! `HeadObject` and `ListObjectsV2`, signed with AWS Signature Version 4.
//!
//! Works against AWS S3 and against S3-compatible services through an
//! endpoint override (MinIO, LocalStack, Cloudflare R2, Ceph RGW, …), with
//! virtual-hosted (`https://bucket.host/key`) or path-style
//! (`https://host/bucket/key`) addressing. Without credentials, requests are
//! sent unsigned (public buckets).
//!
//! ```no_run
//! use ferroly::store::s3::S3Client;
//! use ferroly::store::Cache;
//!
//! # async fn demo() -> Result<(), ferroly::store::StoreError> {
//! // AWS_REGION, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN,
//! // and optionally AWS_ENDPOINT_URL_S3 / AWS_ENDPOINT_URL.
//! let s3 = S3Client::from_env()?;
//! let cache = Cache::open("/var/cache/models")?;
//! let stored = s3.download(&cache, "my-bucket", "models/weights.bin", None).await?;
//! println!("{} -> {}", stored.digest, stored.path.display());
//! # Ok(())
//! # }
//! ```
//!
//! Not included: instance-metadata (IMDS), SSO, profile-file and web-identity
//! credential providers, uploads (`PutObject`, multipart), presigned URLs and
//! SigV4a. Supply credentials explicitly or through the environment.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroly::codec::{xml, Limits, Value};
use ferroly::hash::base64;
use ferroly::http::{percent_encode, Client, HeaderMap, Method, Request, Response, Uri};

use super::fetch::{
    adapter_client, error_from_response, has_content, read_capped, truncate, ERROR_BODY_LIMIT,
};
use super::{Cache, Digest, Fetched, StoreError, Stored};

pub mod sigv4;

pub use sigv4::{Credentials, PayloadHash, Signature};

/// The SigV4 service name for S3.
const SERVICE: &str = "s3";

/// Largest `ListObjectsV2` response body read (a 1000-key page is far
/// smaller).
const MAX_LIST_BYTES: u64 = 16 * 1024 * 1024;

/// Longest object key S3 accepts, in bytes.
const MAX_KEY_BYTES: usize = 1024;

/// How far the service's clock may be from ours before a `403`
/// `RequestTimeTooSkewed` is taken as clock skew and the request signed
/// again.
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);

/// The skew a `403` to a `HEAD` (which has no body, so no error code) must
/// show before it is taken as clock skew: AWS refuses requests 15 minutes
/// off.
const HEAD_CLOCK_SKEW: Duration = Duration::from_secs(15 * 60);

/// The largest clock correction ever learnt. A `Date` further away is not
/// believed: following it would hand whoever sent it a request signed for
/// a time it could replay it at.
const MAX_CLOCK_CORRECTION: Duration = Duration::from_secs(24 * 3600);

/// A client for one S3-compatible service (one region and endpoint).
///
/// Cheap to clone. `Debug` redacts the credentials.
#[derive(Clone, Debug)]
pub struct S3Client {
    http: Client,
    region: String,
    endpoint: Uri,
    custom_endpoint: bool,
    path_style: bool,
    credentials: Option<Credentials>,
    unsigned_payload: bool,
    /// Whether a `RequestTimeTooSkewed` refusal is answered by re-signing at
    /// the service's time.
    correct_clock_skew: bool,
    /// Seconds the service's clock is ahead of ours, learnt from a `403`
    /// whose `Date` is too far from local time. Shared by clones.
    clock_skew: Arc<AtomicI64>,
}

/// Builds an [`S3Client`].
#[derive(Clone, Debug, Default)]
pub struct S3ClientBuilder {
    http: Option<Client>,
    region: Option<String>,
    endpoint: Option<String>,
    path_style: bool,
    credentials: Option<Credentials>,
    unsigned_payload: bool,
    correct_clock_skew: Option<bool>,
}

impl S3ClientBuilder {
    /// The region requests are signed for (required; `us-east-1` for most
    /// S3-compatible services that ignore it).
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Credentials to sign with. Without them requests are sent unsigned.
    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// The service endpoint, for S3-compatible services
    /// (`http://127.0.0.1:9000` for MinIO, `https://<account>.r2.cloudflarestorage.com`
    /// for R2). The default is `https://s3.<region>.amazonaws.com`.
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into());
        self
    }

    /// Uses path-style addressing (`host/bucket/key`) instead of
    /// virtual-hosted (`bucket.host/key`). Path style is used anyway for a
    /// bucket name that is not a valid DNS label, an IP-address endpoint, or
    /// (over HTTPS) a bucket name with dots.
    pub fn path_style(mut self, enabled: bool) -> Self {
        self.path_style = enabled;
        self
    }

    /// Sends `x-amz-content-sha256: UNSIGNED-PAYLOAD` instead of the hash of
    /// the (empty) request body. Off by default.
    pub fn unsigned_payload(mut self, enabled: bool) -> Self {
        self.unsigned_payload = enabled;
        self
    }

    /// Whether a signed request refused for clock skew is signed again at
    /// the service's time, and the correction kept for later requests.
    ///
    /// The correction is learnt only from a `403` whose error code is
    /// `RequestTimeTooSkewed` (for a `HEAD`, which has no body: a `Date` more
    /// than 15 minutes away), is at most 24 hours, and is kept only if the
    /// re-signed request is not refused again.
    ///
    /// Default: on for `https://` endpoints, **off for `http://`** ones,
    /// where anyone on the path could forge the refusal and its `Date` and
    /// so obtain a request signed for a later time, replayable then. Turn it
    /// on for a plain-http endpoint (a local MinIO) only if the network path
    /// is trusted.
    pub fn correct_clock_skew(mut self, enabled: bool) -> Self {
        self.correct_clock_skew = Some(enabled);
        self
    }

    /// The HTTP client to use (timeouts, proxy, TLS roots). Redirects are
    /// not followed for S3 requests whatever the client's
    /// [`max_redirects`](ferroly::http::ClientBuilder::max_redirects) says
    /// (a signature, and the session token sent with it, are for one host
    /// only: a redirect is returned as a [`StoreError::Remote`] error), and
    /// response decompression is turned off (an object stored with
    /// `Content-Encoding` is fetched and verified exactly as stored).
    pub fn client(mut self, client: Client) -> Self {
        self.http = Some(client);
        self
    }

    /// Fills unset options from environment-style variables read through
    /// `lookup`: the region from `AWS_REGION` or `AWS_DEFAULT_REGION`,
    /// credentials as in [`Credentials::from_lookup`], and the endpoint from
    /// `AWS_ENDPOINT_URL_S3` or `AWS_ENDPOINT_URL`.
    pub fn from_lookup(mut self, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| lookup(name).filter(|v| !v.is_empty());
        if self.region.is_none() {
            self.region = get("AWS_REGION").or_else(|| get("AWS_DEFAULT_REGION"));
        }
        if self.credentials.is_none() {
            self.credentials = Credentials::from_lookup(&lookup);
        }
        if self.endpoint.is_none() {
            self.endpoint = get("AWS_ENDPOINT_URL_S3").or_else(|| get("AWS_ENDPOINT_URL"));
        }
        self
    }

    /// Builds the client.
    pub fn build(self) -> Result<S3Client, StoreError> {
        let region = self
            .region
            .ok_or_else(|| StoreError::Config("no S3 region configured".into()))?;
        if region.is_empty()
            || region.len() > 64
            || !region
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(StoreError::Config(format!("invalid region {region:?}")));
        }
        let custom_endpoint = self.endpoint.is_some();
        let url = self
            .endpoint
            .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
        let endpoint = Uri::parse(&url)
            .map_err(|e| StoreError::Config(format!("invalid S3 endpoint: {e}")))?;
        if endpoint.path != "/" || endpoint.query.is_some() {
            return Err(StoreError::Config(
                "an S3 endpoint must not have a path or query".into(),
            ));
        }
        if endpoint.scheme != "http" && endpoint.scheme != "https" {
            return Err(StoreError::Config("an S3 endpoint must be http(s)".into()));
        }
        let correct_clock_skew = self
            .correct_clock_skew
            .unwrap_or(endpoint.scheme == "https");
        Ok(S3Client {
            http: adapter_client(self.http.unwrap_or_default()),
            region,
            endpoint,
            custom_endpoint,
            path_style: self.path_style,
            credentials: self.credentials,
            unsigned_payload: self.unsigned_payload,
            correct_clock_skew,
            clock_skew: Arc::new(AtomicI64::new(0)),
        })
    }
}

/// Metadata of an object, from `HeadObject` or `GetObject` response headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObjectMeta {
    /// `Content-Length` of the response (the whole object for `HeadObject`
    /// and a full `GetObject`, the range for a ranged one).
    pub content_length: Option<u64>,
    /// The entity tag, without quotes.
    pub etag: Option<String>,
    /// `Last-Modified`, as sent.
    pub last_modified: Option<String>,
    /// `Content-Type`.
    pub content_type: Option<String>,
    /// `x-amz-version-id`.
    pub version_id: Option<String>,
    /// `x-amz-checksum-sha256` (base64), present when the object was stored
    /// with a SHA-256 checksum and checksum mode was requested.
    pub checksum_sha256: Option<String>,
    /// `x-amz-checksum-type`: `FULL_OBJECT` or `COMPOSITE` (a checksum of
    /// the parts' checksums), when the service says.
    pub checksum_type: Option<String>,
    /// User metadata (`x-amz-meta-*` headers, name without the prefix).
    pub metadata: Vec<(String, String)>,
    /// The `If-Match` value for this ETag: the header as sent, when it is a
    /// strong entity tag. `None` for a weak (`W/`) or malformed one, which a
    /// strong comparison can never match.
    if_match: Option<String>,
}

impl ObjectMeta {
    fn from_headers(headers: &HeaderMap) -> ObjectMeta {
        let get = |name: &str| headers.get(name).map(str::to_string);
        ObjectMeta {
            content_length: headers.content_length(),
            etag: headers.get("etag").map(unquote),
            if_match: headers.get("etag").and_then(strong_etag),
            last_modified: get("last-modified"),
            content_type: get("content-type"),
            version_id: get("x-amz-version-id"),
            checksum_sha256: get("x-amz-checksum-sha256"),
            checksum_type: get("x-amz-checksum-type"),
            metadata: headers
                .iter()
                .filter_map(|(k, v)| {
                    let lower = k.to_ascii_lowercase();
                    lower
                        .strip_prefix("x-amz-meta-")
                        .map(|name| (name.to_string(), v.to_string()))
                })
                .collect(),
        }
    }

    /// The object's SHA-256 as a cache [`Digest`], when S3 reports a
    /// full-object `x-amz-checksum-sha256`. A multipart composite checksum
    /// is not a digest of the content and gives `None`: one written
    /// `…-<parts>`, or one whose `x-amz-checksum-type` is anything but
    /// `FULL_OBJECT` (some S3-compatible services omit the suffix).
    pub fn sha256_digest(&self) -> Option<Digest> {
        let value = self.checksum_sha256.as_deref()?;
        if value.contains('-') {
            return None;
        }
        if let Some(kind) = &self.checksum_type {
            if !kind.trim().eq_ignore_ascii_case("FULL_OBJECT") {
                return None;
            }
        }
        let bytes = base64::decode(value).ok()?;
        let bytes: [u8; 32] = bytes.try_into().ok()?;
        Some(Digest::from_bytes(bytes))
    }
}

/// A `GetObject` response: the object's metadata and its streaming body.
#[derive(Debug)]
pub struct GetObject {
    meta: ObjectMeta,
    response: Response,
}

impl GetObject {
    /// The object's metadata.
    pub fn meta(&self) -> &ObjectMeta {
        &self.meta
    }

    /// The next piece of the body, or `None` at the end.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.response.chunk().await?)
    }

    /// The whole body in memory (subject to the HTTP client's body cap).
    pub async fn bytes(self) -> Result<Vec<u8>, StoreError> {
        Ok(self.response.bytes().await?)
    }

    /// The underlying HTTP response.
    pub fn into_response(self) -> Response {
        self.response
    }
}

/// Parameters of a `ListObjectsV2` request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListObjects {
    /// Only keys that begin with this prefix.
    pub prefix: Option<String>,
    /// Groups keys that contain the delimiter after the prefix into
    /// [`ListPage::common_prefixes`].
    pub delimiter: Option<String>,
    /// Page size (S3 caps it at 1000).
    pub max_keys: Option<u32>,
    /// The token from the previous page.
    pub continuation_token: Option<String>,
    /// Start listing after this key.
    pub start_after: Option<String>,
}

impl ListObjects {
    /// An unfiltered listing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets [`prefix`](ListObjects::prefix).
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Sets [`delimiter`](ListObjects::delimiter).
    pub fn delimiter(mut self, delimiter: impl Into<String>) -> Self {
        self.delimiter = Some(delimiter.into());
        self
    }

    /// Sets [`max_keys`](ListObjects::max_keys).
    pub fn max_keys(mut self, max_keys: u32) -> Self {
        self.max_keys = Some(max_keys);
        self
    }

    /// Sets [`continuation_token`](ListObjects::continuation_token).
    pub fn continuation_token(mut self, token: impl Into<String>) -> Self {
        self.continuation_token = Some(token.into());
        self
    }

    /// Sets [`start_after`](ListObjects::start_after).
    pub fn start_after(mut self, key: impl Into<String>) -> Self {
        self.start_after = Some(key.into());
        self
    }
}

/// One object in a listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObjectSummary {
    /// The object key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// The entity tag, without quotes.
    pub etag: Option<String>,
    /// `LastModified` (ISO 8601), as sent.
    pub last_modified: Option<String>,
    /// The storage class.
    pub storage_class: Option<String>,
}

/// One page of a `ListObjectsV2` listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListPage {
    /// The objects on this page.
    pub objects: Vec<ObjectSummary>,
    /// Key prefixes rolled up by the delimiter.
    pub common_prefixes: Vec<String>,
    /// Whether more pages follow.
    pub is_truncated: bool,
    /// The token for the next page, when truncated.
    pub next_continuation_token: Option<String>,
}

impl S3Client {
    /// A builder; set at least the [region](S3ClientBuilder::region).
    pub fn builder() -> S3ClientBuilder {
        S3ClientBuilder::default()
    }

    /// A client configured from the process environment (see
    /// [`S3ClientBuilder::from_lookup`]).
    pub fn from_env() -> Result<S3Client, StoreError> {
        S3Client::builder()
            .from_lookup(crate::store::env_lookup)
            .build()
    }

    /// The region requests are signed for.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// The URL of `key` in `bucket` (no query), with this client's
    /// addressing style.
    pub fn object_url(&self, bucket: &str, key: &str) -> Result<String, StoreError> {
        validate_key(key)?;
        self.url(bucket, Some(key), &[])
    }

    fn virtual_hosted(&self, bucket: &str) -> bool {
        if self.path_style || !dns_compatible(bucket) || is_ip(&self.endpoint.host) {
            return false;
        }
        // A dotted bucket does not match the `*.s3…` wildcard certificate.
        !(self.endpoint.is_tls() && bucket.contains('.'))
            // A custom endpoint on a plain hostname (no dot) is almost
            // certainly local; don't invent `bucket.localhost`.
            && (!self.custom_endpoint || self.endpoint.host.contains('.'))
    }

    fn url(
        &self,
        bucket: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
    ) -> Result<String, StoreError> {
        validate_bucket(bucket)?;
        let encoded_key = key.map(|k| {
            k.split('/')
                .map(percent_encode)
                .collect::<Vec<_>>()
                .join("/")
        });
        let (authority, path) = if self.virtual_hosted(bucket) {
            let default = if self.endpoint.is_tls() { 443 } else { 80 };
            let host = format!("{bucket}.{}", self.endpoint.host);
            let authority = if self.endpoint.port == default {
                host
            } else {
                format!("{host}:{}", self.endpoint.port)
            };
            (authority, format!("/{}", encoded_key.unwrap_or_default()))
        } else {
            let path = match encoded_key {
                Some(k) => format!("/{bucket}/{k}"),
                None => format!("/{bucket}"),
            };
            (self.endpoint.authority(), path)
        };
        let mut url = format!("{}://{authority}{path}", self.endpoint.scheme);
        if !query.is_empty() {
            let q = query
                .iter()
                .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&q);
        }
        Ok(url)
    }

    /// Builds and signs a request (signing needs every header, so `headers`
    /// are added first) at local time corrected by `skew` seconds.
    fn request(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        skew: i64,
    ) -> Result<Request, StoreError> {
        let mut req = Request::builder(method, url)?.build();
        for (name, value) in headers {
            req.headers.set(*name, value.clone());
        }
        if let Some(creds) = &self.credentials {
            let payload = if self.unsigned_payload {
                PayloadHash::Unsigned
            } else {
                PayloadHash::empty()
            };
            sigv4::sign(
                &mut req,
                creds,
                &self.region,
                SERVICE,
                skewed_now(skew),
                &payload,
            )?;
        }
        Ok(req)
    }

    /// Sends a freshly signed request.
    ///
    /// With [clock-skew correction](S3ClientBuilder::correct_clock_skew) on,
    /// a signed request refused with `403` `RequestTimeTooSkewed` (for a
    /// `HEAD`: a `403` whose `Date` is over 15 minutes away) by a service
    /// whose `Date` is more than [`MAX_CLOCK_SKEW`], and at most
    /// [`MAX_CLOCK_CORRECTION`], from local time is signed again once at the
    /// service's time. The correction is kept for later requests unless the
    /// retry is refused too.
    ///
    /// A `403` whose XML error body was read to find its code is returned
    /// as the [`StoreError::Remote`] it describes.
    async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<Response, StoreError> {
        let known = self.clock_skew.load(Ordering::Relaxed);
        let req = self.request(method.clone(), url, headers, known)?;
        let resp = self.http.send(req).await?;
        if resp.status().as_u16() != 403 || self.credentials.is_none() || !self.correct_clock_skew {
            return Ok(resp);
        }
        let Some(server) = resp.headers().get("date").and_then(parse_http_date) else {
            return Ok(resp);
        };
        let skew = signed_secs_between(SystemTime::now(), server);
        if skew.unsigned_abs() > MAX_CLOCK_CORRECTION.as_secs() {
            // Not believable; never sign for that time.
            return Ok(resp);
        }
        let off_by = (skew - known).unsigned_abs();
        if method == Method::Head {
            if off_by <= HEAD_CLOCK_SKEW.as_secs() {
                return Ok(resp);
            }
            drop(resp);
        } else {
            if off_by <= MAX_CLOCK_SKEW.as_secs() || !is_xml(resp.headers()) {
                return Ok(resp);
            }
            let error = s3_error(resp).await;
            if !matches!(&error, StoreError::Remote { code, .. } if code == "RequestTimeTooSkewed")
            {
                return Err(error);
            }
        }
        let req = self.request(method, url, headers, skew)?;
        let retry = self.http.send(req).await?;
        if retry.status().as_u16() != 403 {
            self.clock_skew.store(skew, Ordering::Relaxed);
        }
        Ok(retry)
    }

    /// `GetObject`: the whole object, streaming.
    pub async fn get_object(&self, bucket: &str, key: &str) -> Result<GetObject, StoreError> {
        self.get(bucket, key, None).await
    }

    /// `GetObject` for the bytes `start..=end` (`end: None` for "to the end
    /// of the object"). The answer is `206` with `Content-Range`, or `200`
    /// with the whole object if the service ignores ranges.
    pub async fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<GetObject, StoreError> {
        if end.is_some_and(|e| e < start) {
            return Err(StoreError::Config("range end is before its start".into()));
        }
        let range = match end {
            Some(end) => format!("bytes={start}-{end}"),
            None => format!("bytes={start}-"),
        };
        self.get(bucket, key, Some(range)).await
    }

    async fn get(
        &self,
        bucket: &str,
        key: &str,
        range: Option<String>,
    ) -> Result<GetObject, StoreError> {
        let url = self.object_url(bucket, key)?;
        let mut headers = Vec::new();
        if let Some(range) = range {
            headers.push(("range", range));
            headers.push(("accept-encoding", "identity".to_string()));
        }
        let resp = self.send(Method::Get, &url, &headers).await?;
        if !resp.status().is_success() {
            return Err(s3_error(resp).await);
        }
        Ok(GetObject {
            meta: ObjectMeta::from_headers(resp.headers()),
            response: resp,
        })
    }

    /// `HeadObject`: the object's metadata. Asks for checksums
    /// (`x-amz-checksum-mode: ENABLED`), so
    /// [`ObjectMeta::sha256_digest`] is available for objects stored with a
    /// SHA-256 checksum. A missing object is a `404`
    /// [`StoreError::Remote`] ([`StoreError::is_not_found`]).
    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StoreError> {
        let url = self.object_url(bucket, key)?;
        let resp = self
            .send(
                Method::Head,
                &url,
                &[("x-amz-checksum-mode", "ENABLED".to_string())],
            )
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let code = if status == 404 { "NotFound" } else { "" };
            return Err(with_region_hint(
                StoreError::remote(
                    status,
                    code,
                    format!("HeadObject {bucket}/{} failed", truncate(key, 200)),
                ),
                bucket_region(resp.headers()),
            ));
        }
        Ok(ObjectMeta::from_headers(resp.headers()))
    }

    /// `ListObjectsV2`: one page of keys.
    pub async fn list_objects_v2(
        &self,
        bucket: &str,
        params: &ListObjects,
    ) -> Result<ListPage, StoreError> {
        let max_keys = params.max_keys.map(|n| n.to_string());
        let mut query: Vec<(&str, &str)> = vec![("list-type", "2")];
        for (name, value) in [
            ("continuation-token", params.continuation_token.as_deref()),
            ("delimiter", params.delimiter.as_deref()),
            ("max-keys", max_keys.as_deref()),
            ("prefix", params.prefix.as_deref()),
            ("start-after", params.start_after.as_deref()),
        ] {
            if let Some(value) = value {
                query.push((name, value));
            }
        }
        let url = self.url(bucket, None, &query)?;
        let resp = self.send(Method::Get, &url, &[]).await?;
        if !resp.status().is_success() {
            return Err(s3_error(resp).await);
        }
        let body = read_capped(resp, MAX_LIST_BYTES).await?;
        parse_list(&body)
    }

    /// Lists every key matching `params`, following continuation tokens.
    /// Fails with [`StoreError::SizeMismatch`] rather than return more than
    /// `max_objects` entries, and with [`StoreError::Protocol`] if the
    /// service repeats a token or reports more pages without giving one.
    pub async fn list_all(
        &self,
        bucket: &str,
        params: &ListObjects,
        max_objects: usize,
    ) -> Result<Vec<ObjectSummary>, StoreError> {
        let mut params = params.clone();
        let mut all = Vec::new();
        let mut empty_pages = 0;
        loop {
            let page = self.list_objects_v2(bucket, &params).await?;
            if all.len() + page.objects.len() > max_objects {
                return Err(StoreError::SizeMismatch(format!(
                    "listing has more than {max_objects} objects"
                )));
            }
            empty_pages = if page.objects.is_empty() {
                empty_pages + 1
            } else {
                0
            };
            all.extend(page.objects);
            if !page.is_truncated {
                return Ok(all);
            }
            let token = page.next_continuation_token.ok_or_else(|| {
                StoreError::Protocol("truncated listing without a continuation token".into())
            })?;
            if params.continuation_token.as_deref() == Some(token.as_str()) || empty_pages > 100 {
                return Err(StoreError::Protocol(
                    "listing does not make progress".into(),
                ));
            }
            params.continuation_token = Some(token);
        }
    }

    /// A [`Fetched`] for `key` from byte `offset` — the building block for
    /// [`Cache::get_or_fetch`]. Each call signs a fresh request.
    pub async fn fetch(&self, bucket: &str, key: &str, offset: u64) -> Result<Fetched, StoreError> {
        self.fetch_matching(bucket, key, offset, None).await
    }

    /// [`fetch`](S3Client::fetch) with `If-Match: <if_match>` (a strong
    /// entity tag, quoted) when one is given, so an object replaced since it
    /// was looked at fails fast with `412` instead of downloading the new
    /// content.
    async fn fetch_matching(
        &self,
        bucket: &str,
        key: &str,
        offset: u64,
        if_match: Option<&str>,
    ) -> Result<Fetched, StoreError> {
        let url = self.object_url(bucket, key)?;
        let mut headers = vec![("accept-encoding", "identity".to_string())];
        if let Some(tag) = if_match {
            headers.push(("if-match", tag.to_string()));
        }
        if offset > 0 {
            headers.push(("range", format!("bytes={offset}-")));
        }
        let resp = self.send(Method::Get, &url, &headers).await?;
        if !has_content(resp.status().as_u16(), offset) {
            return Err(s3_error(resp).await);
        }
        Fetched::from_response(resp, offset).await
    }

    /// Downloads `key` into `cache`, verified, and returns its digest and
    /// path.
    ///
    /// - With `expected`, the object is fetched (resuming a partial
    ///   download) and must hash to it; a cache hit makes no request.
    /// - Without, `HeadObject` asks for the object's SHA-256 checksum; if S3
    ///   has one, it keys the cache exactly as above. Otherwise the object
    ///   is downloaded and stored under the digest it turns out to have.
    ///   Either way the `GET` carries `If-Match` with the ETag the `HEAD`
    ///   reported, so an object replaced in between is a `412`
    ///   [`StoreError::Remote`] error, not a mismatch after a full download.
    ///   A weak ETag (`W/"…"`, which a strong `If-Match` never matches) is
    ///   not sent; the content is then checked only by its digest.
    pub async fn download(
        &self,
        cache: &Cache,
        bucket: &str,
        key: &str,
        expected: Option<&Digest>,
    ) -> Result<Stored, StoreError> {
        if let Some(digest) = expected {
            let path = cache
                .get_or_fetch(digest, |offset| self.fetch(bucket, key, offset))
                .await?;
            return Ok(Stored {
                digest: *digest,
                path,
            });
        }
        let meta = self.head_object(bucket, key).await?;
        let etag = meta.if_match.as_deref();
        let fetch = |offset| self.fetch_matching(bucket, key, offset, etag);
        match (meta.sha256_digest(), meta.content_length) {
            (Some(digest), Some(size)) => {
                let path = cache.get_or_fetch_sized(&digest, size, fetch).await?;
                Ok(Stored { digest, path })
            }
            (Some(digest), None) => {
                let path = cache.get_or_fetch(&digest, fetch).await?;
                Ok(Stored { digest, path })
            }
            _ => {
                cache
                    .insert(|| self.fetch_matching(bucket, key, 0, etag))
                    .await
            }
        }
    }
}

/// Builds a [`StoreError::Remote`] from an S3 error response
/// (`<Error><Code>…</Code><Message>…</Message></Error>`).
async fn s3_error(resp: Response) -> StoreError {
    let status = resp.status().as_u16();
    let region = bucket_region(resp.headers());
    if !is_xml(resp.headers()) {
        return with_region_hint(error_from_response(resp).await, region);
    }
    let body = read_capped(resp, ERROR_BODY_LIMIT)
        .await
        .unwrap_or_default();
    let parsed = std::str::from_utf8(&body)
        .ok()
        .and_then(|s| xml::from_str_with(s, xml_limits()).ok());
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(Value::as_str)
            .map(|s| truncate(s, 512))
            .unwrap_or_default()
    };
    with_region_hint(
        StoreError::remote(status, field("Code"), field("Message")),
        region,
    )
}

/// Whether a response says its body is XML (an S3 error document).
fn is_xml(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .is_some_and(|t| t.contains("xml"))
}

/// Local time corrected by `skew` seconds (the service's clock minus ours).
fn skewed_now(skew: i64) -> SystemTime {
    let now = SystemTime::now();
    let shift = Duration::from_secs(skew.unsigned_abs());
    if skew >= 0 {
        now + shift
    } else {
        now.checked_sub(shift).unwrap_or(now)
    }
}

/// The `x-amz-bucket-region` S3 sends with a wrong-region answer (a `301`
/// `PermanentRedirect`, a `400`), if it is a plausible region name.
fn bucket_region(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-amz-bucket-region")
        .map(str::trim)
        .filter(|r| {
            !r.is_empty()
                && r.len() <= 64
                && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .map(str::to_string)
}

/// Adds "the bucket is in region …" to a remote error's message.
fn with_region_hint(err: StoreError, region: Option<String>) -> StoreError {
    match (err, region) {
        (
            StoreError::Remote {
                status,
                code,
                message,
            },
            Some(region),
        ) => {
            let hint = format!("the bucket is in region {region}; sign for that region");
            let message = if message.is_empty() {
                hint
            } else {
                format!("{message} ({hint})")
            };
            StoreError::Remote {
                status,
                code,
                message,
            }
        }
        (err, _) => err,
    }
}

fn xml_limits() -> Limits {
    Limits::new()
        .with_max_depth(16)
        .with_max_input_bytes(MAX_LIST_BYTES as usize)
        .with_max_entries(200_000)
}

/// Decodes a `ListBucketResult` document.
fn parse_list(body: &[u8]) -> Result<ListPage, StoreError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| StoreError::Protocol("listing is not UTF-8".into()))?;
    let root = xml::from_str_with(text, xml_limits())
        .map_err(|e| StoreError::Protocol(format!("malformed listing: {e}")))?;
    if root.as_object().is_none() {
        return Err(StoreError::Protocol("malformed listing".into()));
    }
    let mut page = ListPage {
        is_truncated: text_of(&root, "IsTruncated").as_deref() == Some("true"),
        next_continuation_token: text_of(&root, "NextContinuationToken"),
        ..ListPage::default()
    };
    for item in items(root.get("Contents")) {
        let key = text_of(item, "Key")
            .ok_or_else(|| StoreError::Protocol("listing entry without a Key".into()))?;
        let size = match text_of(item, "Size") {
            Some(s) => s
                .parse()
                .map_err(|_| StoreError::Protocol(format!("invalid Size {s:?}")))?,
            None => 0,
        };
        page.objects.push(ObjectSummary {
            key,
            size,
            etag: text_of(item, "ETag").map(|e| unquote(&e)),
            last_modified: text_of(item, "LastModified"),
            storage_class: text_of(item, "StorageClass"),
        });
    }
    for item in items(root.get("CommonPrefixes")) {
        if let Some(prefix) = text_of(item, "Prefix") {
            page.common_prefixes.push(prefix);
        }
    }
    Ok(page)
}

/// A repeated element decodes to an array, a single one to its value.
fn items(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![other],
    }
}

fn text_of(value: &Value, name: &str) -> Option<String> {
    match value.get(name)? {
        Value::Str(s) => Some(s.clone()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

/// The `If-Match` form of an `ETag` header: the quoted value as sent, or
/// an unquoted one quoted. `None` for a weak tag — `If-Match` uses the
/// strong comparison (RFC 9110 §13.1.1), which a weak tag never passes —
/// and for anything malformed.
fn strong_etag(etag: &str) -> Option<String> {
    let e = etag.trim();
    if e.is_empty() || e.starts_with("W/") {
        return None;
    }
    let inner = match e.strip_prefix('"').and_then(|e| e.strip_suffix('"')) {
        Some(inner) => inner,
        None if !e.contains('"') => e,
        None => return None,
    };
    let ok = !inner.is_empty()
        && inner
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x7e).contains(&b) || b >= 0x80);
    ok.then(|| format!("\"{inner}\""))
}

fn unquote(etag: &str) -> String {
    let e = etag.trim();
    let e = e.strip_prefix("W/").unwrap_or(e);
    e.trim_matches('"').to_string()
}

/// Bucket names: S3's rules are stricter, but anything outside this set
/// could change the meaning of the URL.
fn validate_bucket(bucket: &str) -> Result<(), StoreError> {
    let ok = !bucket.is_empty()
        && bucket.len() <= 255
        && bucket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && bucket != "."
        && bucket != "..";
    if ok {
        Ok(())
    } else {
        Err(StoreError::Config(format!(
            "invalid bucket name {:?}",
            truncate(bucket, 80)
        )))
    }
}

fn validate_key(key: &str) -> Result<(), StoreError> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(StoreError::Config(format!(
            "an object key must be 1 to {MAX_KEY_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Whether `bucket` can be a DNS label sequence in a virtual-hosted URL.
fn dns_compatible(bucket: &str) -> bool {
    (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && bucket
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
        && !is_ip(bucket)
}

fn is_ip(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// `b - a` in whole seconds, negative when `b` is earlier.
fn signed_secs_between(a: SystemTime, b: SystemTime) -> i64 {
    match b.duration_since(a) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// Parses an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), the form every
/// S3 service sends in `Date`.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let mut parts = value.split_whitespace();
    let (_weekday, day, month, year, time, zone) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if zone != "GMT" || parts.next().is_some() {
        return None;
    }
    let number = |s: &str, max_len: usize| -> Option<i64> {
        if s.is_empty() || s.len() > max_len || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let day = number(day, 2)?;
    let year = number(year, 4)?;
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month)? as i64 + 1;
    let mut hms = time.split(':');
    let (h, m, s) = (
        number(hms.next()?, 2)?,
        number(hms.next()?, 2)?,
        number(hms.next()?, 2)?,
    );
    if hms.next().is_some() || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    // Days from 1970-01-01 to year-month-day (proleptic Gregorian).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(endpoint: Option<&str>, path_style: bool) -> S3Client {
        let mut b = S3Client::builder()
            .region("us-east-1")
            .path_style(path_style);
        if let Some(e) = endpoint {
            b = b.endpoint(e);
        }
        b.build().unwrap()
    }

    #[test]
    fn addressing() {
        let aws = client(None, false);
        assert_eq!(
            aws.object_url("examplebucket", "a b/c+d.txt").unwrap(),
            "https://examplebucket.s3.us-east-1.amazonaws.com/a%20b/c%2Bd.txt"
        );
        assert_eq!(
            aws.object_url("my.bucket", "k").unwrap(),
            "https://s3.us-east-1.amazonaws.com/my.bucket/k"
        );
        let path = client(None, true);
        assert_eq!(
            path.object_url("examplebucket", "k").unwrap(),
            "https://s3.us-east-1.amazonaws.com/examplebucket/k"
        );
        let minio = client(Some("http://127.0.0.1:9000"), false);
        assert_eq!(
            minio.object_url("bucket", "dir/file").unwrap(),
            "http://127.0.0.1:9000/bucket/dir/file"
        );
        let r2 = client(Some("https://acct.r2.cloudflarestorage.com"), false);
        assert_eq!(
            r2.object_url("bucket", "k").unwrap(),
            "https://bucket.acct.r2.cloudflarestorage.com/k"
        );
        assert!(aws.object_url("bad/bucket", "k").is_err());
        assert!(aws.object_url("bucket", "").is_err());
        assert!(aws.object_url("bucket", &"k".repeat(2000)).is_err());
        assert!(aws.object_url("..", "k").is_err());
    }

    #[test]
    fn builder_validation() {
        assert!(S3Client::builder().build().is_err());
        assert!(S3Client::builder().region("us east").build().is_err());
        assert!(S3Client::builder()
            .region("us-east-1")
            .endpoint("http://h/x")
            .build()
            .is_err());
        let env = |name: &str| match name {
            "AWS_DEFAULT_REGION" => Some("eu-west-1".to_string()),
            "AWS_ACCESS_KEY_ID" => Some("AKID".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("SECRET".to_string()),
            "AWS_ENDPOINT_URL" => Some("http://127.0.0.1:9000".to_string()),
            _ => None,
        };
        let c = S3Client::builder().from_lookup(env).build().unwrap();
        assert_eq!(c.region(), "eu-west-1");
        assert!(c.credentials.is_some());
        let shown = format!("{c:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    #[test]
    fn list_parsing() {
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bucket</Name><Prefix></Prefix><KeyCount>2</KeyCount><MaxKeys>2</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>
  <Contents><Key>a &amp; b.txt</Key><LastModified>2009-10-12T17:50:30.000Z</LastModified>
    <ETag>&quot;fba9dede5f27731c9771645a39863328&quot;</ETag><Size>434234</Size>
    <StorageClass>STANDARD</StorageClass></Contents>
  <Contents><Key>c</Key><Size>0</Size></Contents>
  <CommonPrefixes><Prefix>photos/</Prefix></CommonPrefixes>
</ListBucketResult>"#;
        let page = parse_list(doc.as_bytes()).unwrap();
        assert!(page.is_truncated);
        assert_eq!(
            page.next_continuation_token.as_deref(),
            Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
        );
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "a & b.txt");
        assert_eq!(page.objects[0].size, 434234);
        assert_eq!(
            page.objects[0].etag.as_deref(),
            Some("fba9dede5f27731c9771645a39863328")
        );
        assert_eq!(page.common_prefixes, vec!["photos/".to_string()]);

        let single = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                      <Contents><Key>only</Key><Size>5</Size></Contents></ListBucketResult>";
        let page = parse_list(single.as_bytes()).unwrap();
        assert_eq!(page.objects.len(), 1);
        assert!(!page.is_truncated);

        let empty = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert!(parse_list(empty.as_bytes()).unwrap().objects.is_empty());

        for bad in [
            "",
            "not xml",
            "<ListBucketResult><Contents><Size>1</Size></Contents></ListBucketResult>",
            "<ListBucketResult><Contents><Key>k</Key><Size>-1</Size></Contents></ListBucketResult>",
        ] {
            assert!(parse_list(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let deep = format!("{}{}", "<a>".repeat(1000), "</a>".repeat(1000));
        assert!(parse_list(deep.as_bytes()).is_err());
    }

    #[test]
    fn checksum_digest() {
        let digest = Digest::of(b"hello");
        let meta = ObjectMeta {
            checksum_sha256: Some(base64::encode(digest.as_bytes())),
            ..ObjectMeta::default()
        };
        assert_eq!(meta.sha256_digest(), Some(digest));
        let composite = ObjectMeta {
            checksum_sha256: Some(format!("{}-3", base64::encode(digest.as_bytes()))),
            ..ObjectMeta::default()
        };
        assert_eq!(composite.sha256_digest(), None);
        // ST-14: a composite checksum without the `-N` suffix.
        let typed = |kind: &str| ObjectMeta {
            checksum_sha256: Some(base64::encode(digest.as_bytes())),
            checksum_type: Some(kind.to_string()),
            ..ObjectMeta::default()
        };
        assert_eq!(typed("COMPOSITE").sha256_digest(), None);
        assert_eq!(typed("FULL_OBJECT").sha256_digest(), Some(digest));
        assert_eq!(typed("full_object").sha256_digest(), Some(digest));
    }

    #[test]
    fn http_dates() {
        let t = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(t, UNIX_EPOCH + Duration::from_secs(784_111_777));
        let t = parse_http_date("Thu, 29 Feb 2024 23:59:59 GMT").unwrap();
        assert_eq!(t, UNIX_EPOCH + Duration::from_secs(1_709_251_199));
        for bad in [
            "",
            "Sun, 06 Nov 1994 08:49:37",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 25:00:00 GMT",
            "Sun, 06 Nov 1969 08:49:37 GMT",
            "Sun, 06 Nov 99999999999999999999 08:49:37 GMT",
        ] {
            assert!(parse_http_date(bad).is_none(), "{bad:?}");
        }
    }
}
