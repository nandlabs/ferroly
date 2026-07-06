//! Fluent request builder.

use std::sync::Arc;

use ferroly::clients::{retry, RetryPolicy};
use ferroly::codec::Encode;
use ferroly::http::Method;

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
    body: Option<Vec<u8>>,
    deferred_err: Option<ClientError>,
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
            body: None,
            deferred_err: None,
        }
    }

    /// Adds a query parameter.
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Sets a `${name}` path parameter.
    pub fn path_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.path_params.push((key.into(), value.into()));
        self
    }

    /// Adds a request header.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Overrides the request content type (also selects the body codec).
    pub fn content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = ct.into();
        self
    }

    /// Encodes `value` as the request body using the codec for the current
    /// content type. A encoding failure is deferred until [`send`](Self::send).
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

    /// Sends the request, applying auth and retry, returning the [`Response`].
    pub async fn send(self) -> Result<Response, ClientError> {
        if let Some(e) = self.deferred_err {
            return Err(e);
        }
        let url = substitute_path(&self.url, &self.path_params)?;

        // Only auto-retry idempotent methods: retrying a POST/PATCH after a
        // transport error risks double-submitting (the server may have processed
        // the first attempt). Non-idempotent requests execute exactly once.
        match &self.inner.retry {
            Some(policy) if is_idempotent(&self.method) => {
                let policy: RetryPolicy = policy.clone();
                retry(&policy, is_retryable, || self.execute_once(&url)).await
            }
            _ => self.execute_once(&url).await,
        }
    }

    async fn execute_once(&self, url: &str) -> Result<Response, ClientError> {
        let full_url = append_query(url, &self.query);
        let mut req = ferroly::http::Request::builder(self.method.clone(), &full_url)
            .map_err(|e| ClientError::InvalidRequest(e.to_string()))?
            .build();
        for (k, v) in &self.headers {
            req.headers.set(k.clone(), v.clone());
        }
        if let Some(body) = &self.body {
            req.headers.set("content-type", self.content_type.clone());
            req.body = body.clone();
        }
        if let Some(auth) = &self.inner.auth {
            auth.apply(&mut req);
        }

        let resp = self.inner.http.send(req).await?;
        let status = resp.status().as_u16();
        let content_type = resp.headers().get("content-type").map(str::to_string);
        let bytes = resp.bytes().await?;

        Ok(Response::new(status, content_type, bytes))
    }
}

/// Appends query parameters to a URL, percent-encoding keys and values.
fn append_query(url: &str, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return url.to_string();
    }
    let mut out = url.to_string();
    out.push(if url.contains('?') { '&' } else { '?' });
    for (i, (k, v)) in query.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&url_encode(k));
        out.push('=');
        out.push_str(&url_encode(v));
    }
    out
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Whether a failed request may be safely retried. Only **transport** errors are
/// retryable: a 5xx is returned as a successful `Ok(Response)` (the exchange
/// completed) and is left for the caller to handle, not auto-retried.
fn is_retryable(err: &ClientError) -> bool {
    matches!(err, ClientError::Transport(_))
}

/// Whether `method` is idempotent (safe to retry after a transport failure).
fn is_idempotent(method: &ferroly::http::Method) -> bool {
    use ferroly::http::Method;
    matches!(
        method,
        Method::Get | Method::Head | Method::Put | Method::Delete | Method::Options | Method::Trace
    )
}

fn substitute_path(url: &str, params: &[(String, String)]) -> Result<String, ClientError> {
    let mut out = url.to_string();
    for (k, v) in params {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    if out.contains("${") {
        return Err(ClientError::InvalidRequest(format!(
            "unsubstituted path placeholder in: {out}"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_path_params() {
        let params = vec![
            ("id".to_string(), "42".to_string()),
            ("k".into(), "v".into()),
        ];
        let url = substitute_path("http://h/items/${id}/x/${k}", &params).unwrap();
        assert_eq!(url, "http://h/items/42/x/v");
    }

    #[test]
    fn errors_on_missing_placeholder() {
        let params = vec![];
        assert!(substitute_path("http://h/${id}", &params).is_err());
    }
}
