//! HTTP client response wrapper.

use ferroly::codec::Decode;

use ferroly::rest::ClientError;

/// A fully-read HTTP response with codec-aware decoding.
#[derive(Debug, Clone)]
pub struct Response {
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl Response {
    pub(crate) fn new(status: u16, content_type: Option<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body,
        }
    }

    /// The numeric HTTP status code.
    pub fn status_code(&self) -> u16 {
        self.status
    }

    /// Whether the status is a 2xx success, matching
    /// [`StatusCode::is_success`](ferroly::http::StatusCode::is_success).
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The response `Content-Type`, if present.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The raw response body bytes.
    pub fn raw(&self) -> &[u8] {
        &self.body
    }

    /// The response body as a UTF-8 string (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Decodes the body into `T` using the codec selected by `Content-Type`
    /// (defaulting to JSON when absent).
    pub fn decode<T: Decode>(&self) -> Result<T, ClientError> {
        let ct = self.content_type.as_deref().unwrap_or("application/json");
        Ok(ferroly::codec::decode(ct, &self.body)?)
    }
}
