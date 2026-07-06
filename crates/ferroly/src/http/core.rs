//! Core HTTP value types: [`Method`], [`StatusCode`], [`HeaderMap`], [`Uri`].

use super::HttpError;

/// An HTTP request method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// DELETE.
    Delete,
    /// PATCH.
    Patch,
    /// HEAD.
    Head,
    /// OPTIONS.
    Options,
    /// TRACE.
    Trace,
    /// CONNECT.
    Connect,
    /// Any other (extension) method.
    Other(String),
}

impl Method {
    /// The method's canonical uppercase token.
    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
            Method::Trace => "TRACE",
            Method::Connect => "CONNECT",
            Method::Other(s) => s,
        }
    }

    /// Parses a method token (case-sensitive per RFC, uppercase expected).
    pub fn parse(s: &str) -> Method {
        match s {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            "HEAD" => Method::Head,
            "OPTIONS" => Method::Options,
            "TRACE" => Method::Trace,
            "CONNECT" => Method::Connect,
            other => Method::Other(other.to_string()),
        }
    }
}

/// An HTTP status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCode(pub u16);

impl StatusCode {
    /// 200 OK.
    pub const OK: StatusCode = StatusCode(200);
    /// 201 Created.
    pub const CREATED: StatusCode = StatusCode(201);
    /// 204 No Content.
    pub const NO_CONTENT: StatusCode = StatusCode(204);
    /// 400 Bad Request.
    pub const BAD_REQUEST: StatusCode = StatusCode(400);
    /// 401 Unauthorized.
    pub const UNAUTHORIZED: StatusCode = StatusCode(401);
    /// 403 Forbidden.
    pub const FORBIDDEN: StatusCode = StatusCode(403);
    /// 404 Not Found.
    pub const NOT_FOUND: StatusCode = StatusCode(404);
    /// 405 Method Not Allowed.
    pub const METHOD_NOT_ALLOWED: StatusCode = StatusCode(405);
    /// 406 Not Acceptable.
    pub const NOT_ACCEPTABLE: StatusCode = StatusCode(406);
    /// 413 Payload Too Large.
    pub const PAYLOAD_TOO_LARGE: StatusCode = StatusCode(413);
    /// 429 Too Many Requests.
    pub const TOO_MANY_REQUESTS: StatusCode = StatusCode(429);
    /// 500 Internal Server Error.
    pub const INTERNAL_SERVER_ERROR: StatusCode = StatusCode(500);
    /// 503 Service Unavailable.
    pub const SERVICE_UNAVAILABLE: StatusCode = StatusCode(503);

    /// The numeric code.
    pub fn as_u16(self) -> u16 {
        self.0
    }

    /// Whether the code is in the 2xx success range.
    pub fn is_success(self) -> bool {
        (200..300).contains(&self.0)
    }

    /// A canonical reason phrase for common codes.
    pub fn reason(self) -> &'static str {
        match self.0 {
            200 => "OK",
            201 => "Created",
            202 => "Accepted",
            204 => "No Content",
            301 => "Moved Permanently",
            302 => "Found",
            304 => "Not Modified",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "",
        }
    }
}

/// An ordered, case-insensitive collection of HTTP headers.
#[derive(Debug, Clone, Default)]
pub struct HeaderMap {
    entries: Vec<(String, String)>,
}

impl HeaderMap {
    /// Creates an empty header map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a header, replacing any existing values with the same name. CR/LF are
    /// stripped from the name and value to prevent header/response splitting.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = strip_crlf(name.into());
        self.entries.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
        self.entries.push((name, strip_crlf(value.into())));
    }

    /// Appends a header without removing existing same-named values. CR/LF are
    /// stripped from the name and value to prevent header/response splitting.
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries
            .push((strip_crlf(name.into()), strip_crlf(value.into())));
    }

    /// Returns the first value for a header name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether a header is present (case-insensitive).
    pub fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// The parsed `Content-Length`, if present and valid.
    pub fn content_length(&self) -> Option<u64> {
        self.get("content-length")
            .and_then(|v| v.trim().parse().ok())
    }

    /// The `Content-Length`, rejecting the request-smuggling-prone cases of an
    /// invalid value or multiple **conflicting** values. `Ok(None)` = absent;
    /// `Ok(Some(n))` = a single value (or several identical ones); `Err` =
    /// present but unparseable or contradictory.
    pub fn content_length_checked(&self) -> Result<Option<u64>, HttpError> {
        let mut seen: Option<u64> = None;
        for (k, v) in &self.entries {
            if k.eq_ignore_ascii_case("content-length") {
                let n: u64 = v
                    .trim()
                    .parse()
                    .map_err(|_| HttpError::Protocol("invalid Content-Length".into()))?;
                match seen {
                    Some(prev) if prev != n => {
                        return Err(HttpError::Protocol(
                            "conflicting Content-Length values".into(),
                        ))
                    }
                    _ => seen = Some(n),
                }
            }
        }
        Ok(seen)
    }

    /// Whether `Transfer-Encoding` indicates chunked framing.
    pub fn is_chunked(&self) -> bool {
        self.get("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false)
    }

    /// Iterates over `(name, value)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Removes CR and LF so a header name/value cannot inject extra header lines or
/// split the message (response/header injection).
fn strip_crlf(s: String) -> String {
    if s.contains(['\r', '\n']) {
        s.chars().filter(|&c| c != '\r' && c != '\n').collect()
    } else {
        s
    }
}

/// A parsed absolute URL (client side) or request target (server side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uri {
    /// `http` or `https` (empty for a bare request target).
    pub scheme: String,
    /// Host name (empty for a bare request target).
    pub host: String,
    /// Port (defaulted from the scheme).
    pub port: u16,
    /// Absolute path (always begins with `/`).
    pub path: String,
    /// Raw query string (without the leading `?`).
    pub query: Option<String>,
}

impl Uri {
    /// Parses an absolute `http(s)` URL.
    pub fn parse(url: &str) -> Result<Uri, HttpError> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| HttpError::InvalidUrl(url.to_string()))?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" | "ws" => 80,
            "https" | "wss" => 443,
            _ => {
                return Err(HttpError::InvalidUrl(format!(
                    "unsupported scheme: {scheme}"
                )))
            }
        };

        // authority is up to the first '/', '?', or end.
        let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        let remainder = &rest[auth_end..];

        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port = p
                    .parse()
                    .map_err(|_| HttpError::InvalidUrl(url.to_string()))?;
                (h.to_string(), port)
            }
            None => (authority.to_string(), default_port),
        };
        if host.is_empty() {
            return Err(HttpError::InvalidUrl(url.to_string()));
        }

        let (path, query) = split_target(remainder);
        Ok(Uri {
            scheme,
            host,
            port,
            path,
            query,
        })
    }

    /// Whether the scheme uses TLS (`https` or `wss`).
    pub fn is_tls(&self) -> bool {
        self.scheme == "https" || self.scheme == "wss"
    }

    /// The `Host` header value (`host` or `host:port` for non-default ports).
    pub fn authority(&self) -> String {
        let default = if self.is_tls() { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The origin-form request target (`/path?query`).
    pub fn request_target(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{}", self.path, q),
            None => self.path.clone(),
        }
    }
}

/// Splits a request target into path and query, defaulting an empty path to `/`.
pub fn split_target(target: &str) -> (String, Option<String>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (target, None),
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    (path, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Uri::parse("https://api.example.com/v1/chat?x=1").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "api.example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/v1/chat");
        assert_eq!(u.query.as_deref(), Some("x=1"));
        assert!(u.is_tls());
        assert_eq!(u.authority(), "api.example.com");
        assert_eq!(u.request_target(), "/v1/chat?x=1");

        let u = Uri::parse("http://localhost:8080/").unwrap();
        assert_eq!(u.port, 8080);
        assert_eq!(u.authority(), "localhost:8080");
        assert_eq!(u.request_target(), "/");

        assert!(Uri::parse("ftp://x/").is_err());
        assert!(Uri::parse("not-a-url").is_err());
    }

    #[test]
    fn header_map_case_insensitive() {
        let mut h = HeaderMap::new();
        h.set("Content-Type", "application/json");
        assert_eq!(h.get("content-type"), Some("application/json"));
        h.set("content-type", "text/plain");
        assert_eq!(h.get("Content-Type"), Some("text/plain"));
        h.set("Content-Length", "10");
        assert_eq!(h.content_length(), Some(10));
    }

    #[test]
    fn header_set_strips_crlf_to_prevent_injection() {
        let mut h = HeaderMap::new();
        h.set("x-test", "value\r\nInjected: evil");
        // CR/LF removed, so no extra header line can be injected.
        assert_eq!(h.get("x-test"), Some("valueInjected: evil"));
        assert!(h.get("injected").is_none());
        h.append("na\rme", "v\nal");
        assert_eq!(h.get("name"), Some("val"));
    }
}
