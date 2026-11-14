//! Static file serving with validators, conditional requests and ranges,
//! confined to a root directory.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

use ferroly::http::{format_http_date, HeaderMap, HttpResponse, Method, StatusCode};

use super::ctx::Ctx;
use super::router::{Route, Router};

/// Files up to this size are read into memory and sent with a
/// `Content-Length`; larger ones are streamed in chunks.
const BUFFER_LIMIT: u64 = 256 * 1024;
/// The chunk size of a streamed file.
const CHUNK: usize = 64 * 1024;

/// Serves files from a directory. Register it with
/// [`Router::static_files`], or as a [`Route`] with [`route`](Self::route).
///
/// ```no_run
/// use ferroly::turbo::{Router, StaticFiles};
///
/// let _router = Router::new().static_files(
///     "/assets",
///     StaticFiles::new("./public")
///         .index_file("index.html")
///         .cache_control("public, max-age=3600"),
/// ); // GET /assets/css/site.css -> ./public/css/site.css
/// ```
///
/// # What a response carries
///
/// - `Content-Type` from the file extension (the `fsutils` table when that
///   feature is on, a small built-in table otherwise;
///   `application/octet-stream` when unknown) and
///   `X-Content-Type-Options: nosniff`.
/// - `ETag` (a weak validator from size and modification time) and
///   `Last-Modified`. `If-None-Match` and `If-Modified-Since` are answered
///   with `304 Not Modified`.
/// - `Accept-Ranges: bytes`. A single-range `Range` request is answered with
///   `206 Partial Content`, an unsatisfiable one with `416` and
///   `Content-Range: bytes */<size>`. Several ranges in one request, and a
///   `Range` whose `If-Range` does not match, get the whole file.
/// - `Cache-Control`, if configured.
///
/// # Confinement
///
/// Requests can only reach regular files **inside the root**:
///
/// - the path comes from the route's `*` catch-all, already normalised by
///   the router (no `.` or `..` segments);
/// - each segment is percent-decoded on its own and refused if it then
///   contains `/`, `\`, a NUL or control character, or is `.` or `..` — so
///   an encoded separator or encoded dots cannot build a traversal;
/// - the final path is resolved through symbolic links and must still lie
///   under the resolved root; a link that points outside is not followed;
/// - names beginning with `.` (`.env`, `.git`) are not served unless
///   [`allow_hidden`](Self::allow_hidden) is set;
/// - a directory is served only through its index file, when one is
///   configured; there are no listings.
///
/// Anything refused looks like any other missing file: `404`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct StaticFiles {
    root: PathBuf,
    index: Option<String>,
    cache_control: Option<String>,
    allow_hidden: bool,
}

impl StaticFiles {
    /// Serves the directory `root`. No index file, no `Cache-Control`,
    /// hidden files refused.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StaticFiles {
            root: root.into(),
            index: None,
            cache_control: None,
            allow_hidden: false,
        }
    }

    /// Serves `name` (for example `index.html`) for a request that names a
    /// directory.
    pub fn index_file(mut self, name: impl Into<String>) -> Self {
        self.index = Some(name.into());
        self
    }

    /// Sends this `Cache-Control` value with every file.
    pub fn cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = Some(value.into());
        self
    }

    /// Whether files and directories whose name begins with `.` may be
    /// served (default `false`).
    pub fn allow_hidden(mut self, allow: bool) -> Self {
        self.allow_hidden = allow;
        self
    }

    /// A `GET` route (answering `HEAD` too, through the router's automatic
    /// `HEAD`) that serves the path captured by the route's `*` catch-all.
    /// Register it on a pattern that ends in one: `/assets/*path`.
    pub fn route(self) -> Route {
        let files = std::sync::Arc::new(self);
        Route::get(move |ctx: Ctx| {
            let files = files.clone();
            async move { files.serve(&ctx).await }
        })
    }

    /// Serves the file named by the request's catch-all segments
    /// ([`Ctx::tail_segments`]).
    pub async fn serve(&self, ctx: &Ctx) -> HttpResponse {
        self.serve_segments(ctx.tail_segments(), ctx.method(), ctx.headers())
            .await
    }

    /// Serves the file at `segments` (already percent-decoded path
    /// components relative to the root) for a request with the given method
    /// and headers.
    pub async fn serve_segments(
        &self,
        segments: &[String],
        method: &Method,
        headers: &HeaderMap,
    ) -> HttpResponse {
        if !matches!(method, Method::Get | Method::Head) {
            return HttpResponse::text(StatusCode::METHOD_NOT_ALLOWED, "405 Method Not Allowed")
                .header("Allow", "GET, HEAD");
        }
        let Some(file) = self.resolve(segments).await else {
            return not_found();
        };
        self.respond(file, method, headers).await
    }

    fn segment_allowed(&self, segment: &str) -> bool {
        !(segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment
                .chars()
                .any(|c| c == '/' || c == '\\' || c.is_control())
            || (cfg!(windows) && segment.contains(':'))
            || (!self.allow_hidden && segment.starts_with('.')))
    }

    /// Maps request segments to a regular file inside the root.
    async fn resolve(&self, segments: &[String]) -> Option<Resolved> {
        if !segments.iter().all(|s| self.segment_allowed(s)) {
            return None;
        }
        let root = tokio::fs::canonicalize(&self.root).await.ok()?;
        let mut path = root.clone();
        for segment in segments {
            path.push(segment);
        }
        let mut found = confine(&root, &path).await?;
        if found.meta.is_dir() {
            let index = self.index.as_deref()?;
            if !self.segment_allowed(index) {
                return None;
            }
            found = confine(&root, &found.path.join(index)).await?;
        }
        found.meta.is_file().then_some(found)
    }

    async fn respond(&self, file: Resolved, method: &Method, headers: &HeaderMap) -> HttpResponse {
        let len = file.meta.len();
        let modified = file.meta.modified().ok();
        let etag = etag_for(len, modified);
        let last_modified = modified.map(format_http_date);

        let decorate = |mut resp: HttpResponse| {
            resp.headers.set("ETag", etag.clone());
            if let Some(value) = &last_modified {
                resp.headers.set("Last-Modified", value.clone());
            }
            if let Some(value) = &self.cache_control {
                resp.headers.set("Cache-Control", value.clone());
            }
            resp
        };

        // RFC 9110 §13.2.2: If-None-Match wins over If-Modified-Since.
        let not_modified = match headers.get("if-none-match") {
            Some(candidates) => etag_listed(candidates, &etag),
            None => match (
                headers.get("if-modified-since").and_then(parse_http_date),
                modified.and_then(unix_seconds),
            ) {
                (Some(since), Some(mtime)) => mtime <= since,
                _ => false,
            },
        };
        if not_modified {
            return decorate(HttpResponse::new(StatusCode::NOT_MODIFIED));
        }

        let content_type = content_type_for(&file.path);
        let range = match headers.get("range") {
            Some(range) if if_range_matches(headers, last_modified.as_deref()) => {
                parse_range(range, len)
            }
            _ => RangeRequest::Whole,
        };
        let (status, start, count) = match range {
            RangeRequest::Whole => (StatusCode::OK, 0, len),
            RangeRequest::Part(start, end) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
            RangeRequest::Unsatisfiable => {
                return decorate(
                    HttpResponse::text(
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        "416 Range Not Satisfiable",
                    )
                    .header("Content-Range", format!("bytes */{len}"))
                    .header("Accept-Ranges", "bytes"),
                );
            }
        };

        let head = |resp: HttpResponse| {
            let mut resp = decorate(resp)
                .header("Content-Type", content_type.clone())
                .header("X-Content-Type-Options", "nosniff")
                .header("Accept-Ranges", "bytes");
            if status == StatusCode::PARTIAL_CONTENT {
                resp.headers.set(
                    "Content-Range",
                    format!("bytes {}-{}/{}", start, start + count - 1, len),
                );
            }
            resp
        };

        if *method == Method::Head {
            // The server keeps a Content-Length set on a HEAD response.
            return head(HttpResponse::new(status)).header("Content-Length", count.to_string());
        }

        let mut handle = match tokio::fs::File::open(&file.path).await {
            Ok(handle) => handle,
            Err(_) => return not_found(),
        };
        if start > 0 && handle.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return server_error();
        }

        if count <= BUFFER_LIMIT {
            let mut body = Vec::with_capacity(count as usize);
            match handle.take(count).read_to_end(&mut body).await {
                // The file shrank underneath us: do not send a short body
                // under headers that promise more.
                Ok(read) if read as u64 == count => head(HttpResponse::new(status)).body(body),
                _ => server_error(),
            }
        } else {
            let (tx, rx) = mpsc::channel::<Vec<u8>>(4);
            tokio::spawn(async move {
                let mut remaining = count;
                while remaining > 0 {
                    let want = remaining.min(CHUNK as u64) as usize;
                    let mut chunk = vec![0u8; want];
                    match handle.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            chunk.truncate(read);
                            remaining -= read as u64;
                            if tx.send(chunk).await.is_err() {
                                break; // the client went away
                            }
                        }
                    }
                }
            });
            head(HttpResponse::stream(status, rx))
        }
    }
}

impl Router {
    /// Serves `files` under `prefix`: registers a `GET` route (answering
    /// `HEAD` too) at `prefix/*path`. See [`StaticFiles`] for behaviour and
    /// the confinement rules.
    pub fn static_files(self, prefix: &str, files: StaticFiles) -> Self {
        let pattern = format!("{}/*path", prefix.trim_end_matches('/'));
        self.route(&pattern, files.route())
    }
}

struct Resolved {
    path: PathBuf,
    meta: std::fs::Metadata,
}

/// Resolves `path` through symbolic links and accepts it only if the result
/// is still inside `root` (itself already resolved).
async fn confine(root: &Path, path: &Path) -> Option<Resolved> {
    let resolved = tokio::fs::canonicalize(path).await.ok()?;
    if !resolved.starts_with(root) {
        return None;
    }
    let meta = tokio::fs::metadata(&resolved).await.ok()?;
    Some(Resolved {
        path: resolved,
        meta,
    })
}

fn not_found() -> HttpResponse {
    HttpResponse::text(StatusCode::NOT_FOUND, "404 Not Found")
}

fn server_error() -> HttpResponse {
    HttpResponse::text(
        StatusCode::INTERNAL_SERVER_ERROR,
        "500 Internal Server Error",
    )
}

fn unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// A weak validator: it changes whenever the size or the modification time
/// does, which is not proof that the bytes are identical.
fn etag_for(len: u64, modified: Option<SystemTime>) -> String {
    let stamp = modified
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .unwrap_or(Duration::ZERO);
    format!("W/\"{:x}-{:x}\"", len, stamp.as_nanos())
}

/// Whether an `If-None-Match` list names `etag` (weak comparison) or is `*`.
fn etag_listed(candidates: &str, etag: &str) -> bool {
    let opaque = |tag: &str| tag.trim().trim_start_matches("W/").to_string();
    let ours = opaque(etag);
    candidates
        .split(',')
        .any(|candidate| candidate.trim() == "*" || opaque(candidate) == ours)
}

/// Whether a `Range` may be honoured given `If-Range`. That header requires
/// a strong validator; ours is weak, so only a date equal to `Last-Modified`
/// qualifies.
fn if_range_matches(headers: &HeaderMap, last_modified: Option<&str>) -> bool {
    match headers.get("if-range") {
        None => true,
        Some(value) => last_modified.is_some_and(|lm| lm == value.trim()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RangeRequest {
    /// No usable range: send the whole file.
    Whole,
    /// Send bytes `start..=end`.
    Part(u64, u64),
    /// A well-formed range that lies outside the file.
    Unsatisfiable,
}

/// Parses a single-range `Range: bytes=…` header against a file of `len`
/// bytes. A malformed header, another unit, or several ranges are ignored.
fn parse_range(header: &str, len: u64) -> RangeRequest {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeRequest::Whole;
    };
    if spec.contains(',') {
        return RangeRequest::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeRequest::Whole;
    };
    let number = |s: &str| -> Option<u64> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    match (first.is_empty(), last.is_empty()) {
        // `-N`: the final N bytes.
        (true, false) => match number(last) {
            Some(0) => RangeRequest::Unsatisfiable,
            Some(_) if len == 0 => RangeRequest::Unsatisfiable,
            Some(n) => RangeRequest::Part(len.saturating_sub(n), len - 1),
            None => RangeRequest::Whole,
        },
        // `N-`: from N to the end.
        (false, true) => match number(first) {
            Some(start) if start < len => RangeRequest::Part(start, len - 1),
            Some(_) => RangeRequest::Unsatisfiable,
            None => RangeRequest::Whole,
        },
        // `N-M`.
        (false, false) => match (number(first), number(last)) {
            (Some(start), Some(end)) if start > end => RangeRequest::Whole,
            (Some(start), Some(_)) if start >= len => RangeRequest::Unsatisfiable,
            (Some(start), Some(end)) => RangeRequest::Part(start, end.min(len - 1)),
            _ => RangeRequest::Whole,
        },
        (true, true) => RangeRequest::Whole,
    }
}

/// Parses an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) into Unix seconds.
fn parse_http_date(value: &str) -> Option<u64> {
    let mut fields = value.trim().split(' ');
    let weekday = fields.next()?;
    if weekday.len() != 4 || !weekday.ends_with(',') {
        return None;
    }
    let day: u64 = fields.next()?.parse().ok()?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = fields.next()?.parse().ok()?;
    let mut clock = fields.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || fields.next() != Some("GMT") || fields.next().is_some() {
        return None;
    }
    if !(1970..=9999).contains(&year)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm), March-based.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let year_of_era = y - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// The content type for a file, from its extension.
fn content_type_for(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    #[cfg(feature = "fsutils")]
    {
        if let Some(found) = ferroly::fsutils::lookup_content_type(&name) {
            return with_charset(found);
        }
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let known = match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "txt" => "text/plain",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    };
    with_charset(known.to_string())
}

/// Declares UTF-8 for textual types that do not name a charset.
fn with_charset(content_type: String) -> String {
    let textual = content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type == "application/xml"
        || content_type == "image/svg+xml"
        || content_type == "application/javascript";
    if textual && !content_type.contains("charset") {
        format!("{content_type}; charset=utf-8")
    } else {
        content_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        use RangeRequest::*;
        assert_eq!(parse_range("bytes=0-4", 10), Part(0, 4));
        assert_eq!(parse_range("bytes=5-", 10), Part(5, 9));
        assert_eq!(parse_range("bytes=-3", 10), Part(7, 9));
        assert_eq!(parse_range("bytes=-30", 10), Part(0, 9));
        assert_eq!(parse_range("bytes=2-99", 10), Part(2, 9));
        assert_eq!(parse_range("bytes=10-", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=12-20", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 10), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=5-2", 10), Whole);
        assert_eq!(parse_range("bytes=0-1,4-5", 10), Whole);
        assert_eq!(parse_range("items=0-1", 10), Whole);
        assert_eq!(parse_range("bytes=a-b", 10), Whole);
        assert_eq!(parse_range("bytes=+1-2", 10), Whole);
    }

    #[test]
    fn parses_http_dates() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        let now = SystemTime::now();
        assert_eq!(parse_http_date(&format_http_date(now)), unix_seconds(now));
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn etag_comparison_is_weak() {
        let etag = etag_for(10, Some(UNIX_EPOCH + Duration::from_secs(5)));
        assert!(etag.starts_with("W/\""));
        assert!(etag_listed(&etag, &etag));
        assert!(etag_listed(&etag.replace("W/", ""), &etag));
        assert!(etag_listed("\"x\", *", &etag));
        assert!(!etag_listed("\"other\"", &etag));
    }

    #[test]
    fn hidden_and_separator_segments_are_refused() {
        let files = StaticFiles::new(".");
        assert!(files.segment_allowed("site.css"));
        for bad in ["", ".", "..", ".env", "a/b", "a\\b", "a\0b", "a\nb"] {
            assert!(!files.segment_allowed(bad), "{bad:?}");
        }
        assert!(files.allow_hidden(true).segment_allowed(".well-known"));
    }

    #[test]
    fn content_types() {
        assert!(content_type_for(Path::new("a/site.CSS")).starts_with("text/css"));
        assert_eq!(
            content_type_for(Path::new("blob.unknownext")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for(Path::new("noext")),
            "application/octet-stream"
        );
    }
}
