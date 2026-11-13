//! Fluent request builder.

use std::sync::Arc;

use ferroly::clients::{
    authorize, parse_retry_after, ErrorClass, Outcome, PolicyError, RetryDecision,
};
use ferroly::codec::Encode;
use ferroly::http::{percent_encode, HttpError, Method, Uri};

use super::{ClientInner, Response};
use ferroly::rest::ClientError;

/// A fluent builder for a single HTTP request.
///
/// Path placeholders use `${name}` syntax and are substituted from
/// [`path_param`](RequestBuilder::path_param) before the request is sent.
#[must_use]
pub struct RequestBuilder {
    inner: Arc<ClientInner>,
    method: Method,
    url: String,
    path_params: Vec<(String, String)>,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    content_type: String,
    accept: Option<String>,
    body: Option<Vec<u8>>,
    /// `None`: decided by the method. `Some`: the caller's explicit choice.
    retry_safe: Option<bool>,
    deferred_err: Option<ClientError>,
}

/// The parts of a request that every page of a paginated walk repeats.
pub(crate) struct Template {
    method: Method,
    headers: Vec<(String, String)>,
    accept: Option<String>,
    content_type: String,
}

impl Template {
    /// A request for `url` with the template's method and headers.
    pub(crate) fn request(&self, inner: Arc<ClientInner>, url: String) -> RequestBuilder {
        let mut builder = RequestBuilder::new(inner, self.method.clone(), url);
        builder.headers = self.headers.clone();
        builder.accept = self.accept.clone();
        builder.content_type = self.content_type.clone();
        builder
    }
}

/// One failed attempt, as seen by the retry and policy machinery.
enum AttemptError {
    /// The server answered with a status worth retrying (`502`, `503`, `504`,
    /// `429`). The response is kept: if no retry follows, it is what the
    /// caller gets.
    Status(Response),
    /// No response was obtained.
    Failed(ClientError),
}

impl AttemptError {
    /// Whether the failure is a transport error after which sending the
    /// request again can help.
    fn transient_transport(error: &HttpError) -> bool {
        matches!(
            error,
            HttpError::Io(_)
                | HttpError::Timeout
                | HttpError::ConnectTimeout
                | HttpError::Protocol(_)
                | HttpError::Proxy(_)
        )
    }

    /// Whether to retry. `retry_safe` is true for idempotent methods and for
    /// requests the caller declared safe to repeat.
    fn decision(&self, retry_safe: bool) -> RetryDecision {
        match self {
            AttemptError::Status(response) if retry_safe => {
                match response.header("retry-after").and_then(parse_retry_after) {
                    Some(delay) => RetryDecision::RetryAfter(delay),
                    None => RetryDecision::Retry,
                }
            }
            // Nothing was sent, so any method may be tried again.
            AttemptError::Failed(ClientError::Transport(HttpError::ConnectTimeout)) => {
                RetryDecision::Retry
            }
            AttemptError::Failed(ClientError::Transport(error))
                if retry_safe && Self::transient_transport(error) =>
            {
                RetryDecision::Retry
            }
            _ => RetryDecision::Stop,
        }
    }

    /// How the failure counts toward a circuit breaker.
    fn outcome(&self) -> Outcome {
        match self {
            AttemptError::Status(_) => Outcome::Failure,
            AttemptError::Failed(ClientError::Transport(HttpError::InvalidUrl(_))) => {
                Outcome::Ignored
            }
            AttemptError::Failed(ClientError::Transport(_)) => Outcome::Failure,
            // The caller's own mistakes, and rejections by a breaker.
            AttemptError::Failed(_) => Outcome::Ignored,
        }
    }

    /// The classification handed to a [`Policy`](ferroly::clients::Policy).
    fn class(&self, retry_safe: bool) -> ErrorClass {
        ErrorClass {
            retry: self.decision(retry_safe),
            outcome: self.outcome(),
        }
    }
}

/// Statuses that signal a temporary condition on the server's side.
fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

impl RequestBuilder {
    pub(crate) fn new(inner: Arc<ClientInner>, method: Method, url: String) -> Self {
        let content_type = inner.default_content_type.clone();
        Self {
            inner,
            method,
            url,
            path_params: Vec::new(),
            query: Vec::new(),
            headers: Vec::new(),
            content_type,
            accept: None,
            body: None,
            retry_safe: None,
            deferred_err: None,
        }
    }

    pub(crate) fn template(&self) -> Template {
        Template {
            method: self.method.clone(),
            headers: self.headers.clone(),
            accept: self.accept.clone(),
            content_type: self.content_type.clone(),
        }
    }

    /// Adds a query parameter (percent-encoded when the request is sent).
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Sets the value for a `${name}` placeholder in the URL.
    ///
    /// The value is **data, never URL syntax**. Where the placeholder stands
    /// decides how it is inserted:
    ///
    /// - In the **path** it is percent-encoded as a single segment, so `/`,
    ///   `?`, `#`, `%`, spaces and CR/LF in the value cannot add segments,
    ///   start a query, or reach the request line. A value that is empty,
    ///   `.` or `..` cannot be a path segment and is refused when the request
    ///   is sent ([`ClientError::InvalidRequest`]).
    /// - In the **query** it is percent-encoded as one component.
    /// - In the **scheme or authority** (`http://${host}/…`) it is inserted
    ///   as written, but only if it consists of the characters a host and
    ///   port can contain (`A-Z a-z 0-9 . - _ ~ : [ ]`); anything else is
    ///   refused.
    ///
    /// ```
    /// use ferroly::rest::Client;
    ///
    /// // Sends GET /files/..%2F..%2Fetc%2Fpasswd — one segment, not a traversal.
    /// let _request = Client::new()
    ///     .get("https://api.example.com/files/${name}")
    ///     .path_param("name", "../../etc/passwd");
    /// ```
    pub fn path_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.path_params.push((key.into(), value.into()));
        self
    }

    /// Adds a request header (replacing a client default of the same name).
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Overrides the request content type (also selects the body codec, and
    /// the default `Accept`).
    pub fn content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = ct.into();
        self
    }

    /// Sets the `Accept` header: the media type the response is expected in.
    ///
    /// Without this call the request asks for its own content type
    /// (`application/json` unless changed), with `*/*` as a lower-priority
    /// fallback: `Accept: application/json, */*;q=0.5`. An `Accept` set with
    /// [`header`](Self::header) or as a client default is left alone.
    pub fn accept(mut self, media_type: impl Into<String>) -> Self {
        self.accept = Some(media_type.into());
        self
    }

    /// Encodes `value` as the request body using the codec for the current
    /// content type. An encoding failure is deferred until [`send`](Self::send).
    pub fn body<T: Encode>(mut self, value: &T) -> Self {
        match ferroly::codec::encode(&self.content_type, value) {
            Ok(bytes) => self.body = Some(bytes),
            Err(e) => self.deferred_err = Some(ClientError::Codec(e)),
        }
        self
    }

    /// Sets a raw byte body.
    pub fn body_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.body = Some(bytes);
        self
    }

    /// Sends an `Idempotency-Key` header and marks the request as safe to
    /// retry, so the client's retry policy applies to it even though its
    /// method (`POST`, `PATCH`) is not idempotent.
    ///
    /// Use this only with servers that deduplicate on the key: the same key
    /// is sent with every attempt, and the server must answer a repeat
    /// without performing the operation twice.
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.headers
            .push(("Idempotency-Key".to_string(), key.into()));
        self.retry_safe = Some(true);
        self
    }

    /// Declares whether this request is safe to send more than once,
    /// overriding the default taken from its method — `true` to let a
    /// `POST` be retried (when you know the operation is idempotent),
    /// `false` to keep a `PUT` or `DELETE` from being retried.
    pub fn idempotent(mut self, safe_to_retry: bool) -> Self {
        self.retry_safe = Some(safe_to_retry);
        self
    }

    /// Sends the request, applying auth, retry, circuit breaking and any
    /// policy, and returns the [`Response`].
    ///
    /// `Ok` means the server answered — with **any** status, `404` and `500`
    /// included. Call [`Response::error_for_status`] (or build the client
    /// with `error_for_status(true)`) to turn a non-2xx status into
    /// [`ClientError::Status`].
    pub async fn send(mut self) -> Result<Response, ClientError> {
        if let Some(e) = self.deferred_err.take() {
            return Err(e);
        }
        let inner = self.inner.clone();
        if let Some(problem) = &inner.config_error {
            return Err(ClientError::InvalidRequest(problem.clone()));
        }
        let url = self.build_url()?;
        let retry_safe = self
            .retry_safe
            .unwrap_or_else(|| self.method.is_idempotent());

        let outcome = if let Some(policy) = &inner.policy {
            let attempted = policy
                .execute_with(
                    |error: &AttemptError| error.class(retry_safe),
                    || self.attempt(&url),
                )
                .await;
            match attempted {
                Ok(response) => Ok(response),
                Err(PolicyError::Inner(error)) => Err(error),
                Err(PolicyError::Timeout) => Err(AttemptError::Failed(ClientError::Timeout)),
                Err(PolicyError::CircuitOpen) => Err(AttemptError::Failed(
                    ClientError::CircuitOpen("policy".to_string()),
                )),
                Err(PolicyError::BulkheadFull) => Err(AttemptError::Failed(ClientError::Rejected(
                    "the bulkhead is full".to_string(),
                ))),
            }
        } else if let Some(retry) = &inner.retry {
            retry
                .run_with(
                    |error: &AttemptError, _| error.decision(retry_safe),
                    || self.attempt(&url),
                )
                .await
        } else {
            self.attempt(&url).await
        };

        let response = match outcome {
            Ok(response) | Err(AttemptError::Status(response)) => response,
            Err(AttemptError::Failed(error)) => return Err(error),
        };
        if inner.error_for_status {
            response.error_for_status()
        } else {
            Ok(response)
        }
    }

    /// The final URL: base URL applied, placeholders substituted, query
    /// parameters appended.
    fn build_url(&self) -> Result<String, ClientError> {
        let template = join_base(self.inner.base_url.as_deref(), &self.url);
        let url = expand_template(&template, &self.path_params)?;
        let pairs = self
            .inner
            .default_query
            .iter()
            .chain(self.query.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()));
        Ok(append_query(&url, pairs))
    }

    /// One attempt, accounted for by the destination's circuit breaker.
    async fn attempt(&self, url: &str) -> Result<Response, AttemptError> {
        let permit = match &self.inner.breakers {
            Some(registry) => {
                let key = breaker_key(url);
                match registry.get(&key).try_acquire() {
                    Ok(permit) => Some(permit),
                    Err(_) => return Err(AttemptError::Failed(ClientError::CircuitOpen(key))),
                }
            }
            None => None,
        };
        let result = match self.execute_once(url).await {
            Ok(response) if retryable_status(response.status_code()) => {
                Err(AttemptError::Status(response))
            }
            Ok(response) => Ok(response),
            Err(error) => Err(AttemptError::Failed(error)),
        };
        if let Some(permit) = permit {
            permit.record(match &result {
                Ok(response) if response.status_code() >= 500 => Outcome::Failure,
                // The request was wrong, not the destination.
                Ok(response) if response.status_code() >= 400 => Outcome::Ignored,
                Ok(_) => Outcome::Success,
                Err(error) => error.outcome(),
            });
        }
        result
    }

    async fn execute_once(&self, url: &str) -> Result<Response, ClientError> {
        let mut req = ferroly::http::Request::builder(self.method.clone(), url)
            .map_err(|e| ClientError::InvalidRequest(e.to_string()))?
            .build();
        for (k, v) in &self.inner.default_headers {
            req.headers.set(k.clone(), v.clone());
        }
        for (k, v) in &self.headers {
            req.headers.set(k.clone(), v.clone());
        }
        if let Some(body) = &self.body {
            req.headers.set("content-type", self.content_type.clone());
            req.body = body.clone();
        }
        if !req.headers.contains("accept") {
            let accept = match &self.accept {
                Some(accept) => accept.clone(),
                None => format!("{}, */*;q=0.5", self.content_type),
            };
            req.headers.set("accept", accept);
        }
        if let Some(auth) = &self.inner.auth {
            // prepare() then apply(): works for static and refreshing providers.
            authorize(auth.as_ref(), &mut req).await?;
        }

        #[cfg(feature = "obs")]
        let span = {
            let span = ferroly::obs::http::client_span(self.method.as_str(), url).start();
            if !req.headers.contains(ferroly::obs::TRACEPARENT_HEADER) {
                let mut set = |name: &str, value: &str| req.headers.set(name, value);
                match span.context() {
                    Some(ctx) => ferroly::obs::inject(&ctx, &mut set),
                    None => {
                        ferroly::obs::inject_current(&mut set);
                    }
                }
            }
            span
        };

        let exchange = async {
            let resp = self.inner.http.send(req).await?;
            let status = resp.status().as_u16();
            let headers = resp.headers().clone();
            let final_url = resp.uri().absolute();
            let bytes = resp.bytes().await?;
            Ok::<Response, ClientError>(Response::new(status, headers, final_url, bytes))
        };
        let result = exchange.await;

        #[cfg(feature = "obs")]
        {
            match &result {
                Ok(response) => {
                    ferroly::obs::http::record_client_status(&span, response.status_code())
                }
                Err(error) => span.set_error(error.to_string()),
            }
            span.end();
        }
        result
    }
}

/// The name of the circuit breaker for a URL: its `host:port`.
fn breaker_key(url: &str) -> String {
    match Uri::parse(url) {
        Ok(uri) => format!("{}:{}", uri.host, uri.port),
        Err(_) => url.to_string(),
    }
}

/// The index just past `scheme://` when `url` begins with a scheme. A `://`
/// further in (inside a query value, say) does not make a URL absolute. A
/// `${name}` placeholder may stand for the scheme.
fn scheme_end(url: &str) -> Option<usize> {
    let at = url.find("://")?;
    let scheme = &url[..at];
    let plausible = !scheme.is_empty()
        && scheme.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.' | b'$' | b'{' | b'}' | b'_')
        });
    plausible.then_some(at + 3)
}

/// Applies the client's base URL to a request URL that has no scheme.
fn join_base(base: Option<&str>, url: &str) -> String {
    match base {
        Some(base) if scheme_end(url).is_none() => {
            let base = base.trim_end_matches('/');
            if url.is_empty() {
                base.to_string()
            } else if url.starts_with('?') || url.starts_with('#') {
                format!("{base}{url}")
            } else {
                format!("{base}/{}", url.trim_start_matches('/'))
            }
        }
        _ => url.to_string(),
    }
}

/// Where in a URL template a placeholder stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    /// Scheme or authority: before the path begins.
    Authority,
    Path,
    /// Query or fragment.
    Query,
}

/// Substitutes `${name}` placeholders, treating each value as data for the
/// position its placeholder stands in.
fn expand_template(template: &str, params: &[(String, String)]) -> Result<String, ClientError> {
    let invalid = |what: String| ClientError::InvalidRequest(what);
    // The path begins at the first `/`, `?` or `#` after `scheme://`.
    let path_start = match scheme_end(template) {
        Some(after) => template[after..]
            .find(['/', '?', '#'])
            .map_or(template.len(), |i| after + i),
        None => 0,
    };

    let mut out = String::with_capacity(template.len());
    let mut in_query = false;
    let mut rest = template;
    let mut offset = 0;
    loop {
        let Some(open) = rest.find("${") else {
            out.push_str(rest);
            break;
        };
        let literal = &rest[..open];
        // A `?` or `#` in the literal text (after the authority) starts the
        // query; one inside a substituted value never does.
        if literal
            .char_indices()
            .any(|(i, c)| (c == '?' || c == '#') && offset + i >= path_start)
        {
            in_query = true;
        }
        out.push_str(literal);
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find('}') else {
            return Err(invalid(format!(
                "unsubstituted path placeholder in: {template}"
            )));
        };
        let name = &after_open[..close];
        let value = params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| {
                invalid(format!(
                    "unsubstituted path placeholder '${{{name}}}' in: {template}"
                ))
            })?;
        let position = if offset + open < path_start {
            Position::Authority
        } else if in_query {
            Position::Query
        } else {
            Position::Path
        };
        match position {
            Position::Authority => {
                let ok = !value.is_empty()
                    && value.bytes().all(|b| {
                        b.is_ascii_alphanumeric()
                            || matches!(b, b'.' | b'-' | b'_' | b'~' | b':' | b'[' | b']')
                    });
                if !ok {
                    return Err(invalid(format!(
                        "the value of path parameter '{name}' is not valid in the host part of a URL"
                    )));
                }
                out.push_str(value);
            }
            Position::Path => {
                if value.is_empty() || value == "." || value == ".." {
                    return Err(invalid(format!(
                        "the value of path parameter '{name}' cannot be a path segment (empty, '.' or '..')"
                    )));
                }
                out.push_str(&percent_encode(value));
            }
            Position::Query => out.push_str(&percent_encode(value)),
        }
        let consumed = open + 2 + close + 1;
        offset += consumed;
        rest = &rest[consumed..];
    }
    Ok(out)
}

/// Appends query parameters to a URL, percent-encoding keys and values.
fn append_query<'a>(url: &str, query: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut out = url.to_string();
    let mut separator = if url.contains('?') { '&' } else { '?' };
    for (k, v) in query {
        out.push(separator);
        separator = '&';
        out.push_str(&percent_encode(k));
        out.push('=');
        out.push_str(&percent_encode(v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn substitutes_path_params() {
        let url = expand_template(
            "http://h/items/${id}/x/${k}",
            &params(&[("id", "42"), ("k", "v")]),
        )
        .unwrap();
        assert_eq!(url, "http://h/items/42/x/v");
    }

    #[test]
    fn errors_on_missing_placeholder() {
        assert!(expand_template("http://h/${id}", &[]).is_err());
        assert!(expand_template("http://h/${id", &params(&[("id", "1")])).is_err());
    }

    #[test]
    fn path_values_are_one_encoded_segment() {
        let expand =
            |value: &str| expand_template("http://h/items/${id}/detail", &params(&[("id", value)]));
        assert_eq!(expand("a/b").unwrap(), "http://h/items/a%2Fb/detail");
        assert_eq!(
            expand("x?admin=1#frag").unwrap(),
            "http://h/items/x%3Fadmin%3D1%23frag/detail"
        );
        assert_eq!(
            expand("../../etc").unwrap(),
            "http://h/items/..%2F..%2Fetc/detail"
        );
        assert_eq!(
            expand("a HTTP/1.1\r\nHost: evil").unwrap(),
            "http://h/items/a%20HTTP%2F1.1%0D%0AHost%3A%20evil/detail"
        );
        assert_eq!(expand("100%").unwrap(), "http://h/items/100%25/detail");
        // A value is never re-scanned for placeholders.
        assert_eq!(
            expand("${id}").unwrap(),
            "http://h/items/%24%7Bid%7D/detail"
        );
        for refused in ["", ".", ".."] {
            assert!(expand(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn query_and_authority_positions() {
        let url = expand_template(
            "http://${host}/s?q=${q}&empty=${e}",
            &params(&[("host", "127.0.0.1:8080"), ("q", "a&b=c"), ("e", "")]),
        )
        .unwrap();
        assert_eq!(url, "http://127.0.0.1:8080/s?q=a%26b%3Dc&empty=");

        let host = |value: &str| expand_template("http://${h}/x", &params(&[("h", value)]));
        assert_eq!(host("[::1]:9").unwrap(), "http://[::1]:9/x");
        for refused in ["", "evil.test/x", "a b", "u@h", "h\r\nX: y", "h?x", "h#x"] {
            assert!(host(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn base_url_joining() {
        assert_eq!(
            join_base(Some("http://h/v1/"), "/users"),
            "http://h/v1/users"
        );
        assert_eq!(join_base(Some("http://h/v1"), "users"), "http://h/v1/users");
        assert_eq!(join_base(Some("http://h/v1"), ""), "http://h/v1");
        assert_eq!(join_base(Some("http://h/v1"), "?a=1"), "http://h/v1?a=1");
        assert_eq!(join_base(Some("http://h/v1"), "http://o/x"), "http://o/x");
        assert_eq!(join_base(None, "http://o/x"), "http://o/x");
        // A `://` inside the query does not make a relative URL absolute.
        assert_eq!(
            join_base(Some("http://h/v1"), "go?to=http://o/x"),
            "http://h/v1/go?to=http://o/x"
        );
        assert_eq!(
            join_base(Some("http://h/v1"), "${scheme}://o/x"),
            "${scheme}://o/x"
        );
    }

    #[test]
    fn appends_encoded_query() {
        let q = [("a b", "c&d"), ("e", "")];
        assert_eq!(
            append_query("http://h/x", q.iter().copied()),
            "http://h/x?a%20b=c%26d&e="
        );
        assert_eq!(
            append_query("http://h/x?z=1", q.iter().copied()),
            "http://h/x?z=1&a%20b=c%26d&e="
        );
    }

    #[test]
    fn retry_decisions() {
        let status = |code: u16, headers: &[(&str, &str)]| {
            let mut map = ferroly::http::HeaderMap::new();
            for (k, v) in headers {
                map.set(*k, *v);
            }
            AttemptError::Status(Response::new(code, map, "http://h/".into(), Vec::new()))
        };
        assert_eq!(status(503, &[]).decision(true), RetryDecision::Retry);
        assert_eq!(status(503, &[]).decision(false), RetryDecision::Stop);
        assert_eq!(
            status(429, &[("retry-after", "7")]).decision(true),
            RetryDecision::RetryAfter(std::time::Duration::from_secs(7))
        );

        let transport = |e: HttpError| AttemptError::Failed(ClientError::Transport(e));
        assert_eq!(
            transport(HttpError::Timeout).decision(true),
            RetryDecision::Retry
        );
        assert_eq!(
            transport(HttpError::Timeout).decision(false),
            RetryDecision::Stop
        );
        // Nothing was sent: safe for any method.
        assert_eq!(
            transport(HttpError::ConnectTimeout).decision(false),
            RetryDecision::Retry
        );
        assert_eq!(
            transport(HttpError::Tls("bad cert".into())).decision(true),
            RetryDecision::Stop
        );
        assert_eq!(
            AttemptError::Failed(ClientError::CircuitOpen("h:1".into())).decision(true),
            RetryDecision::Stop
        );
    }
}
