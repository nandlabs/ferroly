//! Hugging Face Hub files: resolve `repo` / `revision` / `file` to a
//! download, authenticate with a bearer token, and cache by content digest.
//!
//! A file URL is `<endpoint>/<repo>/resolve/<revision>/<file>` (with a
//! `datasets/` or `spaces/` prefix for those repository types). The Hub
//! answers a `HEAD` with the commit the revision resolved to
//! (`X-Repo-Commit`) and, for files stored in Git LFS, the content's SHA-256
//! (`X-Linked-Etag`) and size (`X-Linked-Size`), then redirects the download
//! to a CDN. [`HfClient::download`] uses the SHA-256 as the cache key, so a
//! file already in the cache costs one `HEAD` and no download, and a partial
//! download resumes. Files not in LFS (small text files; their ETag is a git
//! object id, not a SHA-256) are hashed as they are stored.
//!
//! ```no_run
//! use ferroly::store::hf::{HfClient, HfFile};
//! use ferroly::store::Cache;
//!
//! # async fn demo() -> Result<(), ferroly::store::StoreError> {
//! let hf = HfClient::from_env()?; // HF_ENDPOINT, HF_TOKEN
//! let cache = Cache::open("/var/cache/hf")?;
//! let file = HfFile::model("openai-community/gpt2", "model.safetensors");
//! let got = hf.download(&cache, &file).await?;
//! println!("{} at commit {:?}: {}", got.digest, got.commit, got.path.display());
//! # Ok(())
//! # }
//! ```
//!
//! Not included: the Hub's JSON API (repository listings, file trees,
//! metadata), uploads, Xet storage, and the `huggingface_hub` cache layout
//! (`~/.cache/huggingface`); files go into a [`Cache`].

use std::fmt;

use ferroly::hash::Secret;
use ferroly::http::{percent_encode, Client, HeaderMap, Method, Request, Response, Uri};

use super::fetch::{adapter_client, error_from_response, has_content, ranged, truncate};
use super::redirect::{send_following, CrossOrigin};
use super::{Cache, Digest, Fetched, StoreError};

/// The public Hub.
pub const DEFAULT_ENDPOINT: &str = "https://huggingface.co";

/// The kind of repository a file lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepoType {
    /// A model repository (the default; no URL prefix).
    Model,
    /// A dataset repository (`datasets/…`).
    Dataset,
    /// A Space (`spaces/…`).
    Space,
}

/// A file in a Hub repository at a revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HfFile {
    repo_type: RepoType,
    repo: String,
    revision: String,
    path: String,
}

impl HfFile {
    /// A file in a repository of the given type, at revision `main`.
    pub fn new(repo_type: RepoType, repo: impl Into<String>, path: impl Into<String>) -> Self {
        HfFile {
            repo_type,
            repo: repo.into(),
            revision: "main".into(),
            path: path.into(),
        }
    }

    /// A file in a model repository (`org/name`).
    pub fn model(repo: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(RepoType::Model, repo, path)
    }

    /// A file in a dataset repository.
    pub fn dataset(repo: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(RepoType::Dataset, repo, path)
    }

    /// A file in a Space.
    pub fn space(repo: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(RepoType::Space, repo, path)
    }

    /// Selects a branch, tag, commit id or `refs/pr/<n>` (default `main`).
    pub fn revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = revision.into();
        self
    }

    /// The repository type.
    pub fn repo_type(&self) -> RepoType {
        self.repo_type
    }

    /// The repository id (`org/name`).
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The revision.
    pub fn revision_name(&self) -> &str {
        &self.revision
    }

    /// The path of the file inside the repository.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// What a `HEAD` of a file reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HfFileInfo {
    /// The commit the revision resolved to (`X-Repo-Commit`).
    pub commit: Option<String>,
    /// The entity tag without quotes: the LFS SHA-256 (`X-Linked-Etag`) or
    /// the git object id (`ETag`).
    pub etag: Option<String>,
    /// The file size (`X-Linked-Size`, or `Content-Length` for a file served
    /// directly).
    pub size: Option<u64>,
    /// The content digest, when the ETag is a SHA-256 (LFS files).
    pub digest: Option<Digest>,
    /// Where the download is redirected to, if it is.
    pub location: Option<String>,
}

/// A file downloaded into the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HfDownload {
    /// The content digest.
    pub digest: Digest,
    /// The file's path in the cache.
    pub path: std::path::PathBuf,
    /// The commit the revision resolved to, when the Hub reported it.
    pub commit: Option<String>,
}

/// A Hugging Face Hub client. Cheap to clone; `Debug` redacts the token.
#[derive(Clone)]
pub struct HfClient {
    http: Client,
    endpoint: String,
    token: Option<Secret>,
    plain_http: bool,
}

impl fmt::Debug for HfClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HfClient")
            .field("endpoint", &self.endpoint)
            .field("token", &self.token)
            .field("plain_http", &self.plain_http)
            .finish_non_exhaustive()
    }
}

impl Default for HfClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HfClient {
    /// A client for the public Hub, without a token.
    pub fn new() -> Self {
        HfClient {
            http: Client::new(),
            endpoint: DEFAULT_ENDPOINT.into(),
            token: None,
            plain_http: false,
        }
    }

    /// A client configured from `HF_ENDPOINT` and `HF_TOKEN`.
    pub fn from_env() -> Result<Self, StoreError> {
        Self::from_lookup(crate::store::env_lookup)
    }

    /// [`from_env`](HfClient::from_env), reading variables through `lookup`.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, StoreError> {
        let mut client = HfClient::new();
        if let Some(endpoint) = lookup("HF_ENDPOINT").filter(|v| !v.is_empty()) {
            client = client.with_endpoint(endpoint)?;
        }
        if let Some(token) = lookup("HF_TOKEN").filter(|v| !v.is_empty()) {
            client = client.with_token(token);
        }
        Ok(client)
    }

    /// Uses another Hub endpoint (a mirror, an enterprise Hub). An `http://`
    /// endpoint together with a token needs [`plain_http`](HfClient::plain_http).
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Result<Self, StoreError> {
        let url = url.into();
        let uri = Uri::parse(&url)
            .map_err(|e| StoreError::Config(format!("invalid HF endpoint: {e}")))?;
        if uri.query.is_some() || (uri.scheme != "http" && uri.scheme != "https") {
            return Err(StoreError::Config(
                "the HF endpoint must be an http(s) URL without a query".into(),
            ));
        }
        self.endpoint = url.trim_end_matches('/').to_string();
        Ok(self)
    }

    /// Sends `Authorization: Bearer <token>` to the Hub (never to the CDN it
    /// redirects to).
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(Secret::from(token.into()));
        self
    }

    /// Allows sending the token to an `http://` endpoint (a local mirror).
    /// Off by default: with a token set, requests to an `http://` endpoint
    /// fail with [`StoreError::Config`] instead of sending the token
    /// unencrypted. Without a token it makes no difference.
    pub fn plain_http(mut self, enabled: bool) -> Self {
        self.plain_http = enabled;
        self
    }

    /// The HTTP client to use (timeouts, proxy, TLS roots). Its own redirect
    /// following and response decompression are turned off for Hub
    /// requests: this client follows redirects itself (never forwarding the
    /// token to another origin, refusing `https` → `http`, and reading the
    /// Hub's own answer to a `HEAD` rather than the CDN's), and verifies the
    /// bytes exactly as served.
    pub fn with_client(mut self, client: Client) -> Self {
        self.http = adapter_client(client);
        self
    }

    /// The endpoint in use.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The `resolve` URL of `file`.
    pub fn resolve_url(&self, file: &HfFile) -> Result<String, StoreError> {
        validate_repo(&file.repo)?;
        if file.revision.is_empty() || file.revision.len() > 256 {
            return Err(StoreError::Config("invalid revision".into()));
        }
        let segments: Vec<&str> = file.path.split('/').collect();
        if file.path.len() > 4096
            || segments
                .iter()
                .any(|s| s.is_empty() || *s == "." || *s == "..")
        {
            return Err(StoreError::Config(format!(
                "invalid file path {:?}",
                truncate(&file.path, 80)
            )));
        }
        let prefix = match file.repo_type {
            RepoType::Model => "",
            RepoType::Dataset => "datasets/",
            RepoType::Space => "spaces/",
        };
        let path = segments
            .iter()
            .map(|s| percent_encode(s))
            .collect::<Vec<_>>()
            .join("/");
        Ok(format!(
            "{}/{prefix}{}/resolve/{}/{path}",
            self.endpoint,
            file.repo,
            percent_encode(&file.revision)
        ))
    }

    fn request(&self, method: Method, file: &HfFile) -> Result<Request, StoreError> {
        let url = self.resolve_url(file)?;
        let mut req = Request::builder(method, &url)?.build();
        if let Some(token) = &self.token {
            if !self.plain_http && !req.uri.is_tls() {
                return Err(StoreError::Config(
                    "refusing to send the HF token to an http:// endpoint \
                     (enable HfClient::plain_http to allow it)"
                        .into(),
                ));
            }
            req.headers.set(
                "authorization",
                format!("Bearer {}", token.expose_str().unwrap_or_default()),
            );
        }
        Ok(req)
    }

    /// `HEAD`s the file: the resolved commit, its ETag, size and (for LFS
    /// files) content digest. Same-origin redirects (a renamed repository)
    /// are followed; the redirect to the CDN is not.
    pub async fn file_info(&self, file: &HfFile) -> Result<HfFileInfo, StoreError> {
        let mut req = self.request(Method::Head, file)?;
        req.headers.set("accept-encoding", "identity");
        let hub = req.uri.clone();
        let resp = send_following(&self.http, req, CrossOrigin::Stop).await?;
        let status = resp.status().as_u16();
        if !(200..400).contains(&status) {
            return Err(hf_error(resp).await);
        }
        if !resp.uri().same_origin(&hub) {
            // Only the Hub's own answer carries the commit and the LFS
            // digest; a CDN's would silently turn off verification.
            return Err(StoreError::Protocol(format!(
                "file info came from {} instead of the Hub",
                resp.uri().authority()
            )));
        }
        Ok(info_from_headers(resp.headers(), status))
    }

    /// `GET`s the file, following the redirect to the CDN, with a streaming
    /// body.
    pub async fn open(&self, file: &HfFile) -> Result<Response, StoreError> {
        let req = self.request(Method::Get, file)?;
        let resp = send_following(&self.http, req, CrossOrigin::Follow).await?;
        if !resp.status().is_success() {
            return Err(hf_error(resp).await);
        }
        Ok(resp)
    }

    /// A [`Fetched`] for `file` from byte `offset` — the building block for
    /// [`Cache::get_or_fetch`].
    pub async fn fetch(&self, file: &HfFile, offset: u64) -> Result<Fetched, StoreError> {
        let req = ranged(&self.request(Method::Get, file)?, offset);
        let resp = send_following(&self.http, req, CrossOrigin::Follow).await?;
        if !has_content(resp.status().as_u16(), offset) {
            return Err(hf_error(resp).await);
        }
        Fetched::from_response(resp, offset).await
    }

    /// Downloads `file` into `cache`, verified, and reports the digest, path
    /// and resolved commit.
    ///
    /// The file is first `HEAD`ed. The download itself is pinned to the
    /// commit the Hub reported, so a branch moving in between cannot mix
    /// two versions. An LFS file is keyed by its SHA-256 (a hit needs no
    /// download; a partial download resumes); any other file is hashed as
    /// it is stored.
    pub async fn download(&self, cache: &Cache, file: &HfFile) -> Result<HfDownload, StoreError> {
        let info = self.file_info(file).await?;
        let pinned = match &info.commit {
            Some(commit) if is_commit_id(commit) => file.clone().revision(commit.clone()),
            _ => file.clone(),
        };
        let fetch = |offset| self.fetch(&pinned, offset);
        let (digest, path) = match (info.digest, info.size) {
            (Some(digest), Some(size)) => (
                digest,
                cache.get_or_fetch_sized(&digest, size, fetch).await?,
            ),
            (Some(digest), None) => (digest, cache.get_or_fetch(&digest, fetch).await?),
            (None, _) => {
                let stored = cache.insert(|| self.fetch(&pinned, 0)).await?;
                (stored.digest, stored.path)
            }
        };
        Ok(HfDownload {
            digest,
            path,
            commit: info.commit,
        })
    }
}

fn info_from_headers(headers: &HeaderMap, status: u16) -> HfFileInfo {
    let etag = headers
        .get("x-linked-etag")
        .or_else(|| headers.get("etag"))
        .map(unquote);
    let size = headers
        .get("x-linked-size")
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| {
            if (200..300).contains(&status) {
                headers.content_length()
            } else {
                None
            }
        });
    let digest = etag
        .as_deref()
        .filter(|e| e.len() == 64)
        .and_then(|e| Digest::from_hex(e).ok());
    HfFileInfo {
        commit: headers.get("x-repo-commit").map(|s| s.trim().to_string()),
        etag,
        size,
        digest,
        location: if (300..400).contains(&status) {
            headers.get("location").map(str::to_string)
        } else {
            None
        },
    }
}

/// A Hub error: `X-Error-Code` / `X-Error-Message` when present (a `HEAD`
/// has no body), otherwise the body.
async fn hf_error(resp: Response) -> StoreError {
    let status = resp.status().as_u16();
    let code = resp.headers().get("x-error-code").map(|c| truncate(c, 128));
    let message = resp
        .headers()
        .get("x-error-message")
        .map(|m| truncate(m, 512));
    match (code, message) {
        (None, None) => error_from_response(resp).await,
        (code, message) => StoreError::remote(
            status,
            code.unwrap_or_default(),
            message.unwrap_or_default(),
        ),
    }
}

fn unquote(etag: &str) -> String {
    let e = etag.trim();
    let e = e.strip_prefix("W/").unwrap_or(e);
    e.trim_matches('"').to_string()
}

fn is_commit_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `name` or `org/name`, each part made of `A-Z a-z 0-9 - _ .`, not `.`/`..`.
fn validate_repo(repo: &str) -> Result<(), StoreError> {
    let parts: Vec<&str> = repo.split('/').collect();
    let ok = (1..=2).contains(&parts.len())
        && repo.len() <= 256
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        });
    if ok {
        Ok(())
    } else {
        Err(StoreError::Config(format!(
            "invalid repository id {:?}",
            truncate(repo, 80)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_urls() {
        let hf = HfClient::new();
        assert_eq!(
            hf.resolve_url(&HfFile::model("org/m", "config.json"))
                .unwrap(),
            "https://huggingface.co/org/m/resolve/main/config.json"
        );
        assert_eq!(
            hf.resolve_url(&HfFile::dataset("org/d", "data/train 1.parquet").revision("refs/pr/3"))
                .unwrap(),
            "https://huggingface.co/datasets/org/d/resolve/refs%2Fpr%2F3/data/train%201.parquet"
        );
        assert_eq!(
            hf.resolve_url(&HfFile::space("org/s", "app.py")).unwrap(),
            "https://huggingface.co/spaces/org/s/resolve/main/app.py"
        );
        let mirror = HfClient::new()
            .with_endpoint("https://hf-mirror.example/")
            .unwrap();
        assert_eq!(
            mirror.resolve_url(&HfFile::model("gpt2", "a.bin")).unwrap(),
            "https://hf-mirror.example/gpt2/resolve/main/a.bin"
        );
        for bad in [
            HfFile::model("a/b/c", "f"),
            HfFile::model("../x", "f"),
            HfFile::model("a b", "f"),
            HfFile::model("org/m", "../secret"),
            HfFile::model("org/m", "a//b"),
            HfFile::model("org/m", ""),
            HfFile::model("org/m", "f").revision(""),
        ] {
            assert!(hf.resolve_url(&bad).is_err(), "{bad:?}");
        }
        assert!(HfClient::new().with_endpoint("ftp://x").is_err());
    }

    #[test]
    fn env_and_debug() {
        let hf = HfClient::from_lookup(|name| match name {
            "HF_ENDPOINT" => Some("http://127.0.0.1:1".into()),
            "HF_TOKEN" => Some("hf_secret_token".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(hf.endpoint(), "http://127.0.0.1:1");
        let shown = format!("{hf:?}");
        assert!(!shown.contains("hf_secret"), "{shown}");
    }

    #[test]
    fn token_over_plain_http_needs_an_opt_in() {
        let file = HfFile::model("org/m", "f");
        let hf = HfClient::new()
            .with_endpoint("http://127.0.0.1:1")
            .unwrap()
            .with_token("hf_secret_token");
        let err = hf.request(Method::Get, &file).unwrap_err();
        assert!(matches!(err, StoreError::Config(_)), "{err:?}");
        let req = hf
            .clone()
            .plain_http(true)
            .request(Method::Get, &file)
            .unwrap();
        assert!(req.headers.contains("authorization"));
        // Without a token, or over https, nothing to protect.
        let anonymous = HfClient::new().with_endpoint("http://127.0.0.1:1").unwrap();
        assert!(anonymous.request(Method::Get, &file).is_ok());
        let tls = HfClient::new().with_token("hf_secret_token");
        assert!(tls.request(Method::Head, &file).is_ok());
    }

    #[test]
    fn info_parsing() {
        let sha = "a".repeat(64);
        let mut h = HeaderMap::new();
        h.set("x-linked-etag", format!("\"{sha}\""));
        h.set("etag", "W/\"abc\"");
        h.set("x-linked-size", "1234");
        h.set("x-repo-commit", "0123456789abcdef0123456789abcdef01234567");
        h.set("location", "https://cdn/x");
        h.set("content-length", "10");
        let info = info_from_headers(&h, 302);
        assert_eq!(info.size, Some(1234));
        assert_eq!(info.digest, Some(Digest::from_hex(&sha).unwrap()));
        assert_eq!(info.location.as_deref(), Some("https://cdn/x"));

        let mut h = HeaderMap::new();
        h.set("etag", "\"0123456789abcdef0123456789abcdef01234567\"");
        h.set("content-length", "10");
        let info = info_from_headers(&h, 200);
        assert_eq!(info.digest, None);
        assert_eq!(info.size, Some(10));
    }
}
