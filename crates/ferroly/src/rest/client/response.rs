//! HTTP client response wrapper.

use ferroly::codec::Decode;
use ferroly::http::{HeaderMap, StatusCode, Uri};

use ferroly::rest::ClientError;

/// A fully-read HTTP response with codec-aware decoding.
#[derive(Debug, Clone)]
pub struct Response {
    status: u16,
    headers: HeaderMap,
    url: String,
    body: Vec<u8>,
}

impl Response {
    pub(crate) fn new(status: u16, headers: HeaderMap, url: String, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            url,
            body,
        }
    }

    /// The numeric HTTP status code.
    pub fn status_code(&self) -> u16 {
        self.status
    }

    /// The status as a [`StatusCode`].
    pub fn status(&self) -> StatusCode {
        StatusCode(self.status)
    }

    /// Whether the status is a 2xx success, matching
    /// [`StatusCode::is_success`](ferroly::http::StatusCode::is_success).
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Turns a non-2xx response into [`ClientError::Status`] (which carries
    /// the status and the body); a 2xx response is returned unchanged.
    ///
    /// ```no_run
    /// use ferroly::rest::{Client, ClientError};
    ///
    /// # async fn ex() -> Result<(), ClientError> {
    /// let result = Client::new()
    ///     .get("https://api.example.com/users/1")
    ///     .send()
    ///     .await?
    ///     .error_for_status();
    /// match result {
    ///     Ok(response) => println!("{}", response.text()),
    ///     Err(ClientError::Status { status: 404, .. }) => println!("no such user"),
    ///     Err(other) => return Err(other),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn error_for_status(self) -> Result<Response, ClientError> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(ClientError::Status {
                status: self.status,
                body: self.body,
            })
        }
    }

    /// The response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The first value of a response header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// The response `Content-Type`, if present.
    pub fn content_type(&self) -> Option<&str> {
        self.headers.get("content-type")
    }

    /// The URL the response came from (the final one, after any redirects).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The raw response body bytes.
    pub fn raw(&self) -> &[u8] {
        &self.body
    }

    /// Consumes the response, returning the body bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.body
    }

    /// The response body as a UTF-8 string (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Decodes the body into `T` using the codec selected by `Content-Type`
    /// (defaulting to JSON when absent).
    ///
    /// This decodes whatever the server sent, whatever the status: check
    /// [`is_success`](Self::is_success) or call
    /// [`error_for_status`](Self::error_for_status) first, or an error body
    /// may be decoded as if it were the result.
    pub fn decode<T: Decode>(&self) -> Result<T, ClientError> {
        let ct = self.content_type().unwrap_or("application/json");
        Ok(ferroly::codec::decode(ct, &self.body)?)
    }

    /// The target of the `Link` header entry with relation `rel`
    /// (RFC 8288), as an absolute URL — a relative target is resolved
    /// against [`url`](Self::url).
    pub fn link(&self, rel: &str) -> Option<String> {
        let target = self
            .headers
            .get_all("link")
            .find_map(|value| find_link(value, rel))?;
        match Uri::parse(&self.url).and_then(|base| base.resolve(&target)) {
            Ok(resolved) => Some(resolved.absolute()),
            Err(_) => None,
        }
    }

    /// The URL of the next page: [`link("next")`](Self::link).
    pub fn next_link(&self) -> Option<String> {
        self.link("next")
    }
}

/// Finds the target of the first link in a `Link` header value whose `rel`
/// parameter contains `rel`.
fn find_link(header: &str, rel: &str) -> Option<String> {
    let mut rest = header;
    loop {
        let open = rest.find('<')?;
        let close = open + rest[open..].find('>')?;
        let target = &rest[open + 1..close];
        // The parameters run to the next `,` that is outside a quoted string.
        let params_start = close + 1;
        let mut end = rest.len();
        let mut quoted = false;
        for (i, c) in rest[params_start..].char_indices() {
            match c {
                '"' => quoted = !quoted,
                ',' if !quoted => {
                    end = params_start + i;
                    break;
                }
                _ => {}
            }
        }
        let matches = rest[params_start..end].split(';').any(|param| {
            let Some((name, value)) = param.split_once('=') else {
                return false;
            };
            name.trim().eq_ignore_ascii_case("rel")
                && value
                    .trim()
                    .trim_matches('"')
                    .split_ascii_whitespace()
                    .any(|r| r.eq_ignore_ascii_case(rel))
        });
        if matches {
            return Some(target.trim().to_string());
        }
        if end >= rest.len() {
            return None;
        }
        rest = &rest[end + 1..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(url: &str, link: &str) -> Response {
        let mut headers = HeaderMap::new();
        headers.set("link", link);
        Response::new(200, headers, url.to_string(), Vec::new())
    }

    #[test]
    fn finds_links_by_relation() {
        let header = r#"<https://api.test/items?page=1>; rel="prev", <https://api.test/items?page=3>; rel="next"; title="a, b", </items?page=9>; rel="last""#;
        assert_eq!(
            find_link(header, "next").as_deref(),
            Some("https://api.test/items?page=3")
        );
        assert_eq!(find_link(header, "last").as_deref(), Some("/items?page=9"));
        assert_eq!(find_link(header, "first"), None);
        assert_eq!(
            find_link(r#"<x>; rel="prev NEXT""#, "next").as_deref(),
            Some("x")
        );
        assert_eq!(find_link("garbage", "next"), None);
    }

    #[test]
    fn relative_links_resolve_against_the_page_url() {
        let page = response(
            "https://api.test/v1/items?page=1",
            r#"<?page=2>; rel="next""#,
        );
        assert_eq!(
            page.next_link().as_deref(),
            Some("https://api.test/v1/items?page=2")
        );
        let page = response("https://api.test/v1/items", r#"</v2/items>; rel=next"#);
        assert_eq!(
            page.next_link().as_deref(),
            Some("https://api.test/v2/items")
        );
    }

    #[test]
    fn error_for_status_carries_status_and_body() {
        let ok = Response::new(204, HeaderMap::new(), "http://h/".into(), Vec::new());
        assert!(ok.error_for_status().is_ok());
        let bad = Response::new(404, HeaderMap::new(), "http://h/".into(), b"gone".to_vec());
        match bad.error_for_status() {
            Err(ClientError::Status { status, body }) => {
                assert_eq!(status, 404);
                assert_eq!(body, b"gone");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
