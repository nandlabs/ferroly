//! OAuth 2.0 client-credentials grant (RFC 6749 §4.4).

use std::fmt;
use std::time::Duration;

use ferroly::clients::auth::{basic_header_value, AccessToken, RefreshingAuth, TokenSource};
use ferroly::clients::error::AuthError;
use ferroly::codec::{json, Value};
use ferroly::http::{BoxFuture, Client, HttpError, Method, Request};

use super::Secret;

/// The largest token-endpoint response that is read.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// How the client authenticates itself to the token endpoint
/// (RFC 6749 §2.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientAuthStyle {
    /// `Authorization: Basic base64(client_id:client_secret)` — the method
    /// every OAuth 2.0 server must support, and the default.
    #[default]
    BasicHeader,
    /// `client_id` and `client_secret` as form fields in the request body,
    /// for servers that do not accept the Basic header.
    RequestBody,
}

/// A [`TokenSource`] that obtains access tokens with the OAuth 2.0
/// **client-credentials** grant: the application authenticates as itself
/// (no user involved) and receives a bearer token.
///
/// Wrap it in a [`RefreshingAuth`] — [`into_provider`](Self::into_provider)
/// does that — to get an [`AuthProvider`](ferroly::clients::AuthProvider)
/// that caches the token until shortly before it expires and refreshes it
/// with a single request however many callers are waiting:
///
/// ```no_run
/// use std::sync::Arc;
/// use ferroly::clients::{authorize, AuthProvider, OAuth2ClientCredentials};
/// use ferroly::http::{Client, Method, Request};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let auth: Arc<dyn AuthProvider> = Arc::new(
///     OAuth2ClientCredentials::new(
///         "https://login.example/oauth2/token",
///         "my-client-id",
///         std::env::var("CLIENT_SECRET")?,
///     )
///     .scope("orders:read")
///     .into_provider(),
/// );
///
/// let mut req = Request::builder(Method::Get, "https://api.example/orders")?.build();
/// authorize(auth.as_ref(), &mut req).await?; // fetches or reuses the token
/// let resp = Client::new().send(req).await?;
/// # let _ = resp;
/// # Ok(())
/// # }
/// ```
///
/// The request is a `POST` with an `application/x-www-form-urlencoded` body
/// (`grant_type=client_credentials`, plus `scope`, `audience` and any extra
/// parameters). The response must be JSON with an `access_token`; a
/// `token_type` other than `Bearer` is rejected; `expires_in` (a number, or a
/// numeric string as some servers send) sets the cache lifetime.
///
/// The client secret is held in a [`Secret`] and never appears in `Debug`
/// output or in an [`AuthError`]. Always use an `https://` token URL.
///
/// Needs the `codec` feature (to parse the JSON response) in addition to
/// `clients`.
#[derive(Clone)]
pub struct OAuth2ClientCredentials {
    token_url: String,
    client_id: String,
    client_secret: Secret,
    scopes: Vec<String>,
    audience: Option<String>,
    params: Vec<(String, String)>,
    style: ClientAuthStyle,
    client: Client,
}

impl fmt::Debug for OAuth2ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuth2ClientCredentials")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .field("scopes", &self.scopes)
            .field("audience", &self.audience)
            .field("style", &self.style)
            .finish_non_exhaustive()
    }
}

impl OAuth2ClientCredentials {
    /// A client-credentials source for the token endpoint at `token_url`,
    /// using a client with a 30-second request timeout.
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<Secret>,
    ) -> Self {
        OAuth2ClientCredentials {
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scopes: Vec::new(),
            audience: None,
            params: Vec::new(),
            style: ClientAuthStyle::default(),
            client: Client::new().with_timeout(Some(Duration::from_secs(30))),
        }
    }

    /// Requests a scope. May be called repeatedly; the scopes are sent
    /// space-delimited.
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.push(scope.into());
        self
    }

    /// Requests several scopes.
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }

    /// Sends an `audience` parameter (required by Auth0 and some others to
    /// select the API the token is for).
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }

    /// Adds an extra form parameter (for example `resource`).
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.push((name.into(), value.into()));
        self
    }

    /// Chooses how the client credentials are sent. Default
    /// [`ClientAuthStyle::BasicHeader`].
    pub fn auth_style(mut self, style: ClientAuthStyle) -> Self {
        self.style = style;
        self
    }

    /// Uses `client` for the token requests (custom timeout, shared pool).
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Wraps this source in a caching, auto-refreshing
    /// [`AuthProvider`](ferroly::clients::AuthProvider).
    pub fn into_provider(self) -> RefreshingAuth {
        RefreshingAuth::new(self)
    }

    fn form_body(&self) -> String {
        let mut fields: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
        let scope = self.scopes.join(" ");
        if !scope.is_empty() {
            fields.push(("scope", &scope));
        }
        if let Some(audience) = &self.audience {
            fields.push(("audience", audience));
        }
        for (name, value) in &self.params {
            fields.push((name, value));
        }
        let secret = self.client_secret.expose_str().unwrap_or_default();
        if self.style == ClientAuthStyle::RequestBody {
            fields.push(("client_id", &self.client_id));
            fields.push(("client_secret", secret));
        }
        let mut body = String::new();
        for (name, value) in fields {
            if !body.is_empty() {
                body.push('&');
            }
            form_encode_into(name.as_bytes(), &mut body);
            body.push('=');
            form_encode_into(value.as_bytes(), &mut body);
        }
        body
    }

    async fn request_token(&self) -> Result<AccessToken, AuthError> {
        if self.client_secret.expose_str().is_none() {
            return Err(AuthError::Config("client secret is not valid UTF-8".into()));
        }
        let mut builder = Request::builder(Method::Post, &self.token_url)
            // Do not echo the URL: it is configuration, and may embed secrets.
            .map_err(|_| AuthError::Config("invalid token endpoint URL".into()))?
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json");
        if self.style == ClientAuthStyle::BasicHeader {
            // RFC 6749 §2.3.1: both parts are form-encoded before Basic.
            let mut user = String::new();
            form_encode_into(self.client_id.as_bytes(), &mut user);
            let mut pass = String::new();
            form_encode_into(self.client_secret.expose(), &mut pass);
            let pass = Secret::from(pass);
            builder = builder.header("authorization", basic_header_value(&user, pass.expose()));
        }
        let request = builder.body(self.form_body().into_bytes()).build();

        let mut response = self.client.send(request).await.map_err(transport_error)?;
        let status = response.status().as_u16();
        let success = response.is_success();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(AuthError::InvalidResponse("response too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        // The body may hold a token: keep it wiped and out of every error.
        let body = Secret::new(body);
        let parsed = json::from_slice(body.expose()).ok();

        if !success {
            let code = parsed
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(Value::as_str)
                .and_then(sanitize_error_code);
            return Err(AuthError::Rejected { status, code });
        }
        let parsed = parsed.ok_or_else(|| AuthError::InvalidResponse("body is not JSON".into()))?;
        if parsed.as_object().is_none() {
            return Err(AuthError::InvalidResponse(
                "body is not a JSON object".into(),
            ));
        }
        let token = match parsed.get("access_token") {
            Some(Value::Str(token)) if !token.is_empty() => token,
            _ => return Err(AuthError::InvalidResponse("missing access_token".into())),
        };
        match parsed.get("token_type") {
            // RFC 6749 requires it, but tolerate servers that omit it.
            None => {}
            Some(Value::Str(kind)) if kind.eq_ignore_ascii_case("bearer") => {}
            Some(_) => return Err(AuthError::InvalidResponse("unsupported token_type".into())),
        }
        let mut access = AccessToken::new(token.as_str());
        match parsed.get("expires_in") {
            None | Some(Value::Null) => {}
            Some(value) => {
                let seconds = match value {
                    Value::Str(text) => text.trim().parse::<f64>().ok(),
                    other => other.as_f64(),
                };
                match seconds {
                    Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
                        // Cap at ten years so the arithmetic cannot overflow.
                        access =
                            access.expires_in(Duration::from_secs_f64(seconds.min(315_360_000.0)));
                    }
                    _ => return Err(AuthError::InvalidResponse("invalid expires_in".into())),
                }
            }
        }
        Ok(access)
    }
}

impl TokenSource for OAuth2ClientCredentials {
    fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>> {
        Box::pin(self.request_token())
    }
}

/// Describes a transport failure without echoing the URL or any payload.
fn transport_error(error: HttpError) -> AuthError {
    // `HttpError` may grow variants; anything unknown stays generic.
    #[allow(unreachable_patterns)]
    let reason = match error {
        HttpError::InvalidUrl(_) => "invalid token endpoint URL".to_string(),
        HttpError::Io(e) => format!("io error: {}", e.kind()),
        HttpError::Tls(_) => "tls error".to_string(),
        HttpError::Protocol(_) => "malformed http response".to_string(),
        HttpError::Timeout => "timed out".to_string(),
        _ => "http error".to_string(),
    };
    AuthError::Transport(reason)
}

/// Keeps an OAuth error code only if it looks like one (RFC 6749 §5.2 allows
/// printable ASCII without quotes or backslashes): a server-controlled string
/// should not be able to smuggle arbitrary text into logs.
fn sanitize_error_code(code: &str) -> Option<String> {
    let ok = !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    ok.then(|| code.to_string())
}

/// Appends the `application/x-www-form-urlencoded` encoding of `input`.
fn form_encode_into(input: &[u8], out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in input {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(text: &str) -> String {
        let mut out = String::new();
        form_encode_into(text.as_bytes(), &mut out);
        out
    }

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(encode("abc-XYZ_0.9*"), "abc-XYZ_0.9*");
        assert_eq!(encode("a b"), "a+b");
        assert_eq!(encode("a+b&c=d"), "a%2Bb%26c%3Dd");
        assert_eq!(encode("p@ss:w/rd?#%"), "p%40ss%3Aw%2Frd%3F%23%25");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(encode("~"), "%7E");
    }

    #[test]
    fn body_carries_grant_scope_audience_and_params() {
        let source = OAuth2ClientCredentials::new("http://localhost/token", "id", "s3cret")
            .scope("read")
            .scopes(["write", "admin stuff"])
            .audience("https://api.example/")
            .param("resource", "a&b");
        assert_eq!(
            source.form_body(),
            "grant_type=client_credentials&scope=read+write+admin+stuff\
             &audience=https%3A%2F%2Fapi.example%2F&resource=a%26b"
        );
        let in_body = source.auth_style(ClientAuthStyle::RequestBody);
        assert!(in_body
            .form_body()
            .ends_with("&client_id=id&client_secret=s3cret"));
    }

    #[test]
    fn debug_never_shows_the_client_secret() {
        let source =
            OAuth2ClientCredentials::new("http://localhost/token", "my-id", "s3cret-value");
        let debug = format!("{source:?}");
        assert!(debug.contains("my-id"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("s3cret-value"), "{debug}");
    }

    #[test]
    fn error_codes_are_sanitised() {
        assert_eq!(
            sanitize_error_code("invalid_client"),
            Some("invalid_client".to_string())
        );
        assert_eq!(sanitize_error_code(""), None);
        assert_eq!(sanitize_error_code("bad code\nINFO forged log line"), None);
        assert_eq!(sanitize_error_code(&"x".repeat(65)), None);
    }
}
