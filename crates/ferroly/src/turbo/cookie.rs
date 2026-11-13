//! Request cookies and `Set-Cookie` construction (RFC 6265).

use std::time::{Duration, SystemTime};

use ferroly::http::{format_http_date, HttpResponse};
use ferroly_derive::FerrolyError;

/// Parses a `Cookie` request header value into `(name, value)` pairs, in
/// order. Pairs without `=` or with an empty name are skipped; one pair of
/// surrounding double quotes is removed from a value. Values are **not**
/// percent-decoded — cookie values are opaque to HTTP.
///
/// ```
/// use ferroly::turbo::parse_cookies;
///
/// let cookies = parse_cookies("sid=abc123; theme=\"dark\"; broken");
/// assert_eq!(cookies, vec![
///     ("sid".to_string(), "abc123".to_string()),
///     ("theme".to_string(), "dark".to_string()),
/// ]);
/// ```
pub fn parse_cookies(header: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in header.split(';') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        out.push((name.to_string(), value.to_string()));
    }
    out
}

/// The `SameSite` attribute of a cookie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// Sent only for same-site requests.
    Strict,
    /// Sent for same-site requests and top-level cross-site navigations.
    Lax,
    /// Sent for all requests. Requires `Secure`.
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

/// Why a [`SetCookie`] could not be turned into a header.
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[non_exhaustive]
pub enum CookieError {
    /// The name is empty or is not an HTTP token.
    #[error("invalid cookie name")]
    InvalidName,
    /// The value contains a character a cookie value cannot carry (a space,
    /// `"`, `,`, `;`, `\`, a control character or a non-ASCII byte).
    /// Percent-encode it first.
    #[error("invalid cookie value")]
    InvalidValue,
    /// An attribute value is malformed.
    #[error("invalid cookie attribute: {0}")]
    InvalidAttribute(&'static str),
    /// The attributes break the rules of the name's `__Host-` / `__Secure-`
    /// prefix, or `SameSite=None` is used without `Secure`.
    #[error("cookie attributes not allowed: {0}")]
    Rule(&'static str),
}

/// A `Set-Cookie` header under construction.
///
/// [`SetCookie::new`] starts from **secure defaults** — `Path=/`, `Secure`,
/// `HttpOnly` and `SameSite=Lax` — and each can be changed explicitly.
/// Building validates the name and value and enforces the browser rules for
/// prefixed names:
///
/// - `__Host-…` requires `Secure`, `Path=/` and **no** `Domain`;
/// - `__Secure-…` requires `Secure`;
/// - `SameSite=None` requires `Secure`.
///
/// ```
/// use std::time::Duration;
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{SameSite, SetCookie};
///
/// let cookie = SetCookie::new("__Host-session", "abc123")
///     .max_age(Duration::from_secs(3600))
///     .same_site(SameSite::Strict);
/// assert_eq!(
///     cookie.to_header_value().unwrap(),
///     "__Host-session=abc123; Path=/; Max-Age=3600; Secure; HttpOnly; SameSite=Strict"
/// );
///
/// let resp = cookie.apply(HttpResponse::new(StatusCode::NO_CONTENT)).unwrap();
/// assert!(resp.headers.contains("set-cookie"));
///
/// // A `__Host-` cookie may not name a domain.
/// assert!(SetCookie::new("__Host-session", "x").domain("example.com").to_header_value().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct SetCookie {
    name: String,
    value: String,
    path: Option<String>,
    domain: Option<String>,
    max_age: Option<u64>,
    expires: Option<SystemTime>,
    secure: bool,
    http_only: bool,
    same_site: Option<SameSite>,
}

impl SetCookie {
    /// A cookie with `Path=/`, `Secure`, `HttpOnly` and `SameSite=Lax`.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        SetCookie {
            name: name.into(),
            value: value.into(),
            path: Some("/".to_string()),
            domain: None,
            max_age: None,
            expires: None,
            secure: true,
            http_only: true,
            same_site: Some(SameSite::Lax),
        }
    }

    /// A header that makes the browser delete the cookie called `name`
    /// (`Max-Age=0` and an `Expires` in the past). Use the same `path` and
    /// `domain` the cookie was set with.
    pub fn removal(name: impl Into<String>) -> Self {
        let mut cookie = SetCookie::new(name, "");
        cookie.max_age = Some(0);
        cookie.expires = Some(SystemTime::UNIX_EPOCH);
        cookie
    }

    /// Sets the `Path` attribute.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Removes the `Path` attribute (the browser then uses the request's
    /// directory).
    pub fn no_path(mut self) -> Self {
        self.path = None;
        self
    }

    /// Sets the `Domain` attribute, making the cookie visible to subdomains.
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Sets `Max-Age` (whole seconds). Without it or `expires` the cookie
    /// lasts for the browser session.
    pub fn max_age(mut self, age: Duration) -> Self {
        self.max_age = Some(age.as_secs());
        self
    }

    /// Sets `Expires`.
    pub fn expires(mut self, at: SystemTime) -> Self {
        self.expires = Some(at);
        self
    }

    /// Sets or clears `Secure` (default on).
    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Sets or clears `HttpOnly` (default on).
    pub fn http_only(mut self, http_only: bool) -> Self {
        self.http_only = http_only;
        self
    }

    /// Sets `SameSite` (default `Lax`).
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.same_site = Some(same_site);
        self
    }

    /// Omits the `SameSite` attribute.
    pub fn no_same_site(mut self) -> Self {
        self.same_site = None;
        self
    }

    /// The cookie name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The cookie value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Validates the cookie and renders the `Set-Cookie` header value.
    pub fn to_header_value(&self) -> Result<String, CookieError> {
        if self.name.is_empty() || !self.name.bytes().all(is_token_byte) {
            return Err(CookieError::InvalidName);
        }
        if !self.value.bytes().all(is_cookie_octet) {
            return Err(CookieError::InvalidValue);
        }
        if let Some(path) = &self.path {
            if !path.starts_with('/') || !path.bytes().all(is_attribute_byte) {
                return Err(CookieError::InvalidAttribute("Path"));
            }
        }
        if let Some(domain) = &self.domain {
            let ok = !domain.is_empty()
                && domain
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
            if !ok {
                return Err(CookieError::InvalidAttribute("Domain"));
            }
        }
        if self.name.starts_with("__Host-") {
            if !self.secure {
                return Err(CookieError::Rule("a __Host- cookie must be Secure"));
            }
            if self.path.as_deref() != Some("/") {
                return Err(CookieError::Rule("a __Host- cookie must have Path=/"));
            }
            if self.domain.is_some() {
                return Err(CookieError::Rule("a __Host- cookie must not set Domain"));
            }
        }
        if self.name.starts_with("__Secure-") && !self.secure {
            return Err(CookieError::Rule("a __Secure- cookie must be Secure"));
        }
        if self.same_site == Some(SameSite::None) && !self.secure {
            return Err(CookieError::Rule("SameSite=None requires Secure"));
        }

        let mut out = format!("{}={}", self.name, self.value);
        if let Some(domain) = &self.domain {
            out.push_str("; Domain=");
            out.push_str(domain);
        }
        if let Some(path) = &self.path {
            out.push_str("; Path=");
            out.push_str(path);
        }
        if let Some(age) = self.max_age {
            out.push_str(&format!("; Max-Age={age}"));
        }
        if let Some(at) = self.expires {
            out.push_str("; Expires=");
            out.push_str(&format_http_date(at));
        }
        if self.secure {
            out.push_str("; Secure");
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        if let Some(same_site) = self.same_site {
            out.push_str("; SameSite=");
            out.push_str(same_site.as_str());
        }
        Ok(out)
    }

    /// Appends this cookie to `resp` as a `Set-Cookie` header (other
    /// `Set-Cookie` headers are kept).
    pub fn apply(&self, mut resp: HttpResponse) -> Result<HttpResponse, CookieError> {
        resp.headers.append("Set-Cookie", self.to_header_value()?);
        Ok(resp)
    }
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// RFC 6265 `cookie-octet`.
fn is_cookie_octet(b: u8) -> bool {
    matches!(b, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

/// A byte allowed in an attribute value: visible ASCII except `;`.
fn is_attribute_byte(b: u8) -> bool {
    (0x20..=0x7e).contains(&b) && b != b';'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_header() {
        let cookies = parse_cookies("a=1; b=two;c=\"3\"; =skip; bare; d=x=y");
        assert_eq!(
            cookies,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "two".to_string()),
                ("c".to_string(), "3".to_string()),
                ("d".to_string(), "x=y".to_string()),
            ]
        );
    }

    #[test]
    fn defaults_are_secure() {
        assert_eq!(
            SetCookie::new("sid", "v").to_header_value().unwrap(),
            "sid=v; Path=/; Secure; HttpOnly; SameSite=Lax"
        );
    }

    #[test]
    fn rejects_injection_and_bad_values() {
        assert_eq!(
            SetCookie::new("sid", "a; Domain=evil").to_header_value(),
            Err(CookieError::InvalidValue)
        );
        assert_eq!(
            SetCookie::new("sid", "a\r\nSet-Cookie: x=y").to_header_value(),
            Err(CookieError::InvalidValue)
        );
        assert_eq!(
            SetCookie::new("s id", "a").to_header_value(),
            Err(CookieError::InvalidName)
        );
        assert!(SetCookie::new("sid", "a")
            .path("/x; Secure")
            .to_header_value()
            .is_err());
    }

    #[test]
    fn prefix_rules() {
        assert!(SetCookie::new("__Host-a", "v").to_header_value().is_ok());
        assert!(SetCookie::new("__Host-a", "v")
            .secure(false)
            .to_header_value()
            .is_err());
        assert!(SetCookie::new("__Host-a", "v")
            .path("/app")
            .to_header_value()
            .is_err());
        assert!(SetCookie::new("__Host-a", "v")
            .domain("example.com")
            .to_header_value()
            .is_err());
        assert!(SetCookie::new("__Secure-a", "v")
            .secure(false)
            .to_header_value()
            .is_err());
        assert!(SetCookie::new("a", "v")
            .secure(false)
            .same_site(SameSite::None)
            .to_header_value()
            .is_err());
    }

    #[test]
    fn removal_expires_the_cookie() {
        let header = SetCookie::removal("sid").to_header_value().unwrap();
        assert!(header.starts_with("sid=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970"));
    }
}
