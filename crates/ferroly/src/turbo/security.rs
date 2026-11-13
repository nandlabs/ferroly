//! Security middleware: CORS, security response headers, request ids, and
//! (with the `hash` feature) a double-submit CSRF check.

use std::sync::Arc;
use std::time::Duration;

use ferroly::http::{HttpResponse, Method, StatusCode};

use super::ctx::Ctx;
use super::error::RouteError;
use super::router::Router;

// ---- CORS ----------------------------------------------------------------------

type OriginPredicate = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// A Cross-Origin Resource Sharing policy; install it with [`Router::cors`].
///
/// [`Cors::new`] allows **no** origin: add the ones you serve with
/// [`allow_origin`](Self::allow_origin), a predicate with
/// [`allow_origin_fn`](Self::allow_origin_fn), or — for public, credential-free
/// APIs only — everything with [`allow_any_origin`](Self::allow_any_origin).
///
/// ```
/// use std::time::Duration;
/// use ferroly::http::{HttpResponse, Method, StatusCode};
/// use ferroly::turbo::{Cors, Router};
///
/// let cors = Cors::new()
///     .allow_origin("https://app.example.com")
///     .allow_methods([Method::Get, Method::Post, Method::Delete])
///     .allow_headers(["content-type", "authorization"])
///     .expose_headers(["x-request-id"])
///     .allow_credentials(true)
///     .max_age(Duration::from_secs(600));
///
/// let _router = Router::new()
///     .cors(cors) // first, so preflight requests are answered before authentication
///     .get("/items", |_| async { HttpResponse::text(StatusCode::OK, "[]") });
/// ```
///
/// What the middleware does:
///
/// - **Preflight** (`OPTIONS` with `Access-Control-Request-Method`) from an
///   allowed origin is answered directly with `204` and the
///   `Access-Control-Allow-*` headers; the authenticator, filters and handler
///   do not run. From any other origin it is answered `403` with no CORS
///   headers.
/// - **Other requests** run normally. `Access-Control-Allow-Origin` (and the
///   credentials and expose headers) are added only when the `Origin` is
///   allowed; a disallowed origin gets the plain response, which the browser
///   then withholds from the calling page. CORS is a browser-enforced read
///   protection, not access control — keep authenticating requests.
/// - `Vary: Origin` is added whenever the answer depends on the origin.
/// - **`*` is never combined with credentials.** With
///   [`allow_any_origin`](Self::allow_any_origin) the header is the literal
///   `*` and `Access-Control-Allow-Credentials` is never sent, whatever
///   [`allow_credentials`](Self::allow_credentials) says (and
///   [`Router::validate`] reports the contradiction). To send credentials,
///   name the origins or supply a predicate; the specific origin is then
///   echoed.
#[derive(Clone)]
#[must_use]
pub struct Cors {
    any_origin: bool,
    origins: Vec<String>,
    predicate: Option<OriginPredicate>,
    methods: Vec<Method>,
    headers: Vec<String>,
    any_header: bool,
    expose: Vec<String>,
    credentials: bool,
    max_age: Option<Duration>,
}

impl std::fmt::Debug for Cors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cors")
            .field("any_origin", &self.any_origin)
            .field("origins", &self.origins)
            .field("predicate", &self.predicate.is_some())
            .field("methods", &self.methods)
            .field("headers", &self.headers)
            .field("any_header", &self.any_header)
            .field("expose", &self.expose)
            .field("credentials", &self.credentials)
            .field("max_age", &self.max_age)
            .finish()
    }
}

impl Default for Cors {
    fn default() -> Self {
        Cors::new()
    }
}

impl Cors {
    /// A policy that allows no origin, the methods `GET`, `HEAD` and `POST`,
    /// no extra request headers, and no credentials.
    pub fn new() -> Self {
        Cors {
            any_origin: false,
            origins: Vec::new(),
            predicate: None,
            methods: vec![Method::Get, Method::Head, Method::Post],
            headers: Vec::new(),
            any_header: false,
            expose: Vec::new(),
            credentials: false,
            max_age: None,
        }
    }

    /// A policy for public, unauthenticated APIs: any origin, the common
    /// methods, any request header — and **no** credentials.
    pub fn permissive() -> Self {
        Cors::new()
            .allow_any_origin()
            .allow_any_header()
            .allow_methods([
                Method::Get,
                Method::Head,
                Method::Post,
                Method::Put,
                Method::Patch,
                Method::Delete,
            ])
    }

    /// Allows one origin, written as the browser sends it: scheme, host and
    /// non-default port, no path (`https://app.example.com`).
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.origins
            .push(origin.into().trim_end_matches('/').to_string());
        self
    }

    /// Allows each origin in the list; see [`allow_origin`](Self::allow_origin).
    pub fn allow_origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for origin in origins {
            self = self.allow_origin(origin);
        }
        self
    }

    /// Allows every origin (`Access-Control-Allow-Origin: *`). Credentials
    /// are never allowed together with this.
    pub fn allow_any_origin(mut self) -> Self {
        self.any_origin = true;
        self
    }

    /// Allows the origins for which `predicate` returns `true` (in addition
    /// to any listed ones) — for a set of tenant subdomains, say. The
    /// predicate receives the `Origin` header value; match it exactly or by a
    /// full suffix including the dot (`.example.com`), never by substring.
    pub fn allow_origin_fn<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.predicate = Some(Arc::new(predicate));
        self
    }

    /// Sets the methods a cross-origin request may use (replacing the
    /// default `GET`, `HEAD`, `POST`).
    pub fn allow_methods<I>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = Method>,
    {
        self.methods = methods.into_iter().collect();
        self
    }

    /// Sets the request headers a cross-origin request may send beyond the
    /// ones browsers always allow.
    pub fn allow_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers = headers.into_iter().map(Into::into).collect();
        self.any_header = false;
        self
    }

    /// Allows whatever request headers a preflight asks for (they are echoed
    /// back).
    pub fn allow_any_header(mut self) -> Self {
        self.any_header = true;
        self
    }

    /// Sets the response headers scripts may read beyond the ones browsers
    /// always expose.
    pub fn expose_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.expose = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Whether cross-origin requests may carry credentials (cookies, HTTP
    /// authentication). Has no effect with
    /// [`allow_any_origin`](Self::allow_any_origin).
    pub fn allow_credentials(mut self, allow: bool) -> Self {
        self.credentials = allow;
        self
    }

    /// How long a browser may cache a preflight answer.
    pub fn max_age(mut self, age: Duration) -> Self {
        self.max_age = Some(age);
        self
    }

    /// Whether `origin` is named or matched by the predicate (not counting
    /// the any-origin wildcard).
    fn origin_listed(&self, origin: &str) -> bool {
        self.origins.iter().any(|o| o.eq_ignore_ascii_case(origin))
            || self.predicate.as_ref().is_some_and(|p| p(origin))
    }

    /// The `Access-Control-Allow-Origin` value for `origin` and whether
    /// credentials may be allowed with it; `None` when the origin is not
    /// allowed.
    fn decide(&self, origin: &str) -> Option<(String, bool)> {
        if self.origin_listed(origin) {
            Some((origin.to_string(), self.credentials))
        } else if self.any_origin {
            Some(("*".to_string(), false))
        } else {
            None
        }
    }

    /// Whether responses differ by `Origin` (so caches must key on it).
    fn varies(&self) -> bool {
        !self.origins.is_empty() || self.predicate.is_some()
    }

    fn apply_origin(&self, mut resp: HttpResponse, allow: &str, credentials: bool) -> HttpResponse {
        resp.headers.set("Access-Control-Allow-Origin", allow);
        if credentials {
            resp.headers.set("Access-Control-Allow-Credentials", "true");
        }
        resp
    }

    fn preflight(&self, ctx: &Ctx, allow: &str, credentials: bool) -> HttpResponse {
        let mut resp = self.apply_origin(
            HttpResponse::new(StatusCode::NO_CONTENT),
            allow,
            credentials,
        );
        let methods: Vec<&str> = self.methods.iter().map(Method::as_str).collect();
        resp.headers
            .set("Access-Control-Allow-Methods", methods.join(", "));
        let headers = if self.any_header {
            ctx.header("access-control-request-headers")
                .map(str::to_string)
        } else if self.headers.is_empty() {
            None
        } else {
            Some(self.headers.join(", "))
        };
        if let Some(headers) = headers {
            resp.headers.set("Access-Control-Allow-Headers", headers);
        }
        if let Some(age) = self.max_age {
            resp.headers
                .set("Access-Control-Max-Age", age.as_secs().to_string());
        }
        add_vary(
            resp,
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        )
    }
}

/// Adds `value` to the response's `Vary` unless it is already covered.
fn add_vary(mut resp: HttpResponse, value: &str) -> HttpResponse {
    let covered = resp.headers.get_all("vary").any(|existing| {
        existing.trim() == "*"
            || value.split(',').all(|wanted| {
                existing
                    .split(',')
                    .any(|have| have.trim().eq_ignore_ascii_case(wanted.trim()))
            })
    });
    if !covered {
        resp.headers.append("Vary", value);
    }
    resp
}

impl Router {
    /// Installs a [`Cors`] policy as a middleware. Register it **before**
    /// authentication-related layers; preflight requests carry no
    /// credentials and must be answered without them.
    pub fn cors(mut self, cors: Cors) -> Self {
        if cors.any_origin && cors.credentials {
            self.push_error(RouteError::Config(
                "CORS: allow_any_origin cannot be combined with allow_credentials; \
                 credentials are not allowed for the wildcard origin"
                    .to_string(),
            ));
        }
        let cors = Arc::new(cors);
        self.layer(move |ctx, next| {
            let cors = cors.clone();
            async move {
                let Some(origin) = ctx.header("origin").map(str::to_string) else {
                    // Not a cross-origin request.
                    let resp = next.run(ctx).await;
                    return if cors.varies() {
                        add_vary(resp, "Origin")
                    } else {
                        resp
                    };
                };
                let decision = cors.decide(&origin);
                let preflight = *ctx.method() == Method::Options
                    && ctx.header("access-control-request-method").is_some();
                if preflight {
                    return match &decision {
                        Some((allow, credentials)) => cors.preflight(&ctx, allow, *credentials),
                        None => add_vary(
                            HttpResponse::text(
                                StatusCode::FORBIDDEN,
                                "403 CORS origin not allowed",
                            ),
                            "Origin",
                        ),
                    };
                }
                let mut resp = next.run(ctx).await;
                if let Some((allow, credentials)) = &decision {
                    resp = cors.apply_origin(resp, allow, *credentials);
                    if !cors.expose.is_empty() {
                        resp.headers
                            .set("Access-Control-Expose-Headers", cors.expose.join(", "));
                    }
                }
                if cors.varies() {
                    resp = add_vary(resp, "Origin");
                }
                resp
            }
        })
    }
}

// ---- security headers ----------------------------------------------------------

/// The framing policy sent as `X-Frame-Options` and as the CSP
/// `frame-ancestors` directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOptions {
    /// The page may not be framed at all (`DENY` / `frame-ancestors 'none'`).
    Deny,
    /// Only pages of the same origin may frame it (`SAMEORIGIN` /
    /// `frame-ancestors 'self'`).
    SameOrigin,
    /// Send neither header.
    Unrestricted,
}

/// Security-related response headers; install with
/// [`Router::security_headers`].
///
/// | Header | Default | Change with |
/// |---|---|---|
/// | `Strict-Transport-Security` | `max-age=31536000; includeSubDomains`, on TLS requests only | [`hsts`](Self::hsts), [`hsts_preload`](Self::hsts_preload), [`no_hsts`](Self::no_hsts), [`hsts_behind_proxy`](Self::hsts_behind_proxy) |
/// | `X-Content-Type-Options` | `nosniff` | [`nosniff`](Self::nosniff) |
/// | `X-Frame-Options` | `DENY` | [`frame_options`](Self::frame_options) |
/// | `Content-Security-Policy` | `frame-ancestors 'none'` | [`content_security_policy`](Self::content_security_policy), [`frame_options`](Self::frame_options) |
/// | `Referrer-Policy` | `no-referrer` | [`referrer_policy`](Self::referrer_policy) |
///
/// A header the handler already set is left alone.
///
/// ```
/// use std::time::Duration;
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{FrameOptions, Router, SecurityHeaders};
///
/// let headers = SecurityHeaders::new()
///     .hsts(Duration::from_secs(63_072_000), true)
///     .frame_options(FrameOptions::SameOrigin)
///     .content_security_policy("default-src 'self'")
///     .referrer_policy("strict-origin-when-cross-origin");
///
/// let _router = Router::new()
///     .security_headers(headers)
///     .get("/", |_| async { HttpResponse::text(StatusCode::OK, "ok") });
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct SecurityHeaders {
    hsts: Option<(Duration, bool)>,
    hsts_preload: bool,
    hsts_always: bool,
    nosniff: bool,
    frame: FrameOptions,
    referrer_policy: Option<String>,
    csp: Option<String>,
    extra: Vec<(String, String)>,
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        SecurityHeaders::new()
    }
}

impl SecurityHeaders {
    /// The defaults in the table above.
    pub fn new() -> Self {
        SecurityHeaders {
            hsts: Some((Duration::from_secs(31_536_000), true)),
            hsts_preload: false,
            hsts_always: false,
            nosniff: true,
            frame: FrameOptions::Deny,
            referrer_policy: Some("no-referrer".to_string()),
            csp: None,
            extra: Vec::new(),
        }
    }

    /// Sets the HSTS lifetime and whether it covers subdomains.
    pub fn hsts(mut self, max_age: Duration, include_subdomains: bool) -> Self {
        self.hsts = Some((max_age, include_subdomains));
        self
    }

    /// Adds the `preload` token to HSTS. Only do this for a domain you intend
    /// to submit to the browser preload lists; it is hard to undo.
    pub fn hsts_preload(mut self, preload: bool) -> Self {
        self.hsts_preload = preload;
        self
    }

    /// Never send `Strict-Transport-Security`.
    pub fn no_hsts(mut self) -> Self {
        self.hsts = None;
        self
    }

    /// Send HSTS on every response, not only on requests that arrived over
    /// TLS at this server. Turn it on when TLS is terminated by a reverse
    /// proxy in front (this server then only ever sees plain HTTP).
    pub fn hsts_behind_proxy(mut self, always: bool) -> Self {
        self.hsts_always = always;
        self
    }

    /// Whether to send `X-Content-Type-Options: nosniff`.
    pub fn nosniff(mut self, enabled: bool) -> Self {
        self.nosniff = enabled;
        self
    }

    /// Sets the framing policy (clickjacking protection).
    pub fn frame_options(mut self, frame: FrameOptions) -> Self {
        self.frame = frame;
        self
    }

    /// Sets the `Referrer-Policy` value.
    pub fn referrer_policy(mut self, policy: impl Into<String>) -> Self {
        self.referrer_policy = Some(policy.into());
        self
    }

    /// Do not send `Referrer-Policy`.
    pub fn no_referrer_policy(mut self) -> Self {
        self.referrer_policy = None;
        self
    }

    /// Sets the `Content-Security-Policy`. If it has no `frame-ancestors`
    /// directive, one matching [`frame_options`](Self::frame_options) is
    /// appended.
    pub fn content_security_policy(mut self, policy: impl Into<String>) -> Self {
        self.csp = Some(policy.into());
        self
    }

    /// Adds any other fixed response header (`Permissions-Policy`,
    /// `Cross-Origin-Opener-Policy`, …).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.push((name.into(), value.into()));
        self
    }

    fn csp_value(&self) -> Option<String> {
        let ancestors = match self.frame {
            FrameOptions::Deny => Some("frame-ancestors 'none'"),
            FrameOptions::SameOrigin => Some("frame-ancestors 'self'"),
            FrameOptions::Unrestricted => None,
        };
        match (&self.csp, ancestors) {
            (Some(csp), Some(ancestors))
                if !csp.to_ascii_lowercase().contains("frame-ancestors") =>
            {
                let csp = csp.trim().trim_end_matches(';');
                Some(format!("{csp}; {ancestors}"))
            }
            (Some(csp), _) => Some(csp.clone()),
            (None, Some(ancestors)) => Some(ancestors.to_string()),
            (None, None) => None,
        }
    }

    /// The headers to add to a response, given whether the request used TLS.
    fn headers_for(&self, tls: bool) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some((max_age, subdomains)) = self.hsts {
            if tls || self.hsts_always {
                let mut value = format!("max-age={}", max_age.as_secs());
                if subdomains {
                    value.push_str("; includeSubDomains");
                }
                if self.hsts_preload {
                    value.push_str("; preload");
                }
                out.push(("Strict-Transport-Security".to_string(), value));
            }
        }
        if self.nosniff {
            out.push(("X-Content-Type-Options".to_string(), "nosniff".to_string()));
        }
        match self.frame {
            FrameOptions::Deny => out.push(("X-Frame-Options".to_string(), "DENY".to_string())),
            FrameOptions::SameOrigin => {
                out.push(("X-Frame-Options".to_string(), "SAMEORIGIN".to_string()))
            }
            FrameOptions::Unrestricted => {}
        }
        if let Some(csp) = self.csp_value() {
            out.push(("Content-Security-Policy".to_string(), csp));
        }
        if let Some(policy) = &self.referrer_policy {
            out.push(("Referrer-Policy".to_string(), policy.clone()));
        }
        out.extend(self.extra.iter().cloned());
        out
    }
}

impl Router {
    /// Adds the [`SecurityHeaders`] to every response (including error
    /// responses produced by the router itself).
    pub fn security_headers(self, headers: SecurityHeaders) -> Self {
        let headers = Arc::new(headers);
        self.layer(move |ctx, next| {
            let headers = headers.clone();
            async move {
                let tls = ctx.is_tls();
                let mut resp = next.run(ctx).await;
                for (name, value) in headers.headers_for(tls) {
                    if !resp.headers.contains(&name) {
                        resp.headers.set(name, value);
                    }
                }
                resp
            }
        })
    }
}

// ---- request id ----------------------------------------------------------------

/// The header a request id is read from and echoed in.
pub const REQUEST_ID_HEADER: &str = "X-Request-Id";

/// The id of the current request, stored in the request's
/// [`Extensions`](super::Extensions) by [`Router::request_id`] and
/// `Router::trace_context`. Read it with [`Ctx::request_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(pub String);

/// Whether `value` is acceptable as an inbound request id: 1 to 128
/// characters from `A-Z a-z 0-9 . _ -`.
///
/// The check is deliberately strict. A request id ends up in logs, response
/// headers and downstream requests, so nothing that could break out of those
/// contexts (spaces, quotes, separators, control or non-ASCII characters) is
/// accepted from a client.
pub fn is_valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 64 bits that are unpredictable to a client. Used for request ids only —
/// not for anything whose secrecy matters.
fn random_u64() -> u64 {
    #[cfg(feature = "hash")]
    {
        if let Ok(value) = ferroly::hash::random::u64() {
            return value;
        }
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.finish()
}

/// A fresh request id: 32 lower-case hex characters.
pub(crate) fn generate_request_id() -> String {
    format!("{:016x}{:016x}", random_u64(), random_u64())
}

/// The request's id: the one already stored on the context, else a valid
/// inbound `X-Request-Id`, else a new one. Stores it and queues the response
/// header.
pub(crate) fn ensure_request_id(ctx: &Ctx) -> String {
    if let Some(existing) = ctx.get::<RequestId>() {
        return existing.0.clone();
    }
    let id = match ctx.header(REQUEST_ID_HEADER) {
        Some(inbound) if is_valid_request_id(inbound) => inbound.to_string(),
        _ => generate_request_id(),
    };
    ctx.set(RequestId(id.clone()));
    ctx.set_response_header(REQUEST_ID_HEADER, id.clone());
    id
}

impl Ctx {
    /// The request id assigned by [`Router::request_id`] or
    /// `Router::trace_context`, if one of them is installed.
    pub fn request_id(&self) -> Option<String> {
        self.get::<RequestId>().map(|id| id.0.clone())
    }
}

impl Router {
    /// Gives every request an id, available as [`Ctx::request_id`] and echoed
    /// in the `X-Request-Id` response header.
    ///
    /// An inbound `X-Request-Id` is kept only if it passes
    /// [`is_valid_request_id`]; anything else is replaced by a generated id
    /// (32 hex characters). Register it early so later layers — the access
    /// log, for one — can include the id.
    pub fn request_id(self) -> Self {
        self.layer(|ctx, next| {
            let id = ensure_request_id(&ctx);
            async move {
                let mut resp = next.run(ctx).await;
                if !resp.headers.contains(REQUEST_ID_HEADER) {
                    resp.headers.set(REQUEST_ID_HEADER, id);
                }
                resp
            }
        })
    }
}

// ---- CSRF ----------------------------------------------------------------------

#[cfg(feature = "hash")]
pub use csrf::{Csrf, CsrfToken};

#[cfg(feature = "hash")]
mod csrf {
    use std::sync::Arc;
    use std::time::Duration;

    use ferroly::http::{HttpResponse, StatusCode};

    use super::super::cookie::{SameSite, SetCookie};
    use super::super::ctx::Ctx;
    use super::super::error::RouteError;
    use super::super::router::Router;

    /// The CSRF token of the current request, stored in the request's
    /// [`Extensions`](crate::turbo::Extensions) by [`Router::csrf`]. Render
    /// it into pages (a `<meta>` tag or a hidden field) so scripts can send
    /// it back.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CsrfToken(pub String);

    /// Cross-site request forgery protection for applications that
    /// authenticate with **cookies**, using the double-submit pattern.
    /// Requires the `hash` feature (for the token's randomness); install with
    /// [`Router::csrf`].
    ///
    /// - On a *safe* request (`GET`, `HEAD`, `OPTIONS`, `TRACE`) without a
    ///   token cookie, a random 256-bit token is issued in a cookie.
    /// - An *unsafe* request (`POST`, `PUT`, `PATCH`, `DELETE`, …) must send
    ///   the same token twice: in the cookie and in a request header
    ///   (`X-CSRF-Token` by default). The two are compared in constant time;
    ///   a missing or different value is answered `403`.
    ///
    /// A page on another site can make the browser send the cookie but can
    /// neither read it nor set the header, so it cannot produce a matching
    /// pair.
    ///
    /// The cookie is named `__Host-csrf` by default. The `__Host-` prefix
    /// makes browsers refuse a cookie of that name set by a subdomain or over
    /// plain HTTP — the known weakness of double-submit — and requires
    /// `Secure`. For local development over plain HTTP use
    /// `Csrf::new().cookie_name("csrf").secure(false)`.
    ///
    /// APIs authenticated by an `Authorization` header do not need this.
    ///
    /// ```
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::turbo::{Csrf, CsrfToken, Router};
    ///
    /// let _router = Router::new()
    ///     .csrf(Csrf::new())
    ///     .get("/form", |ctx| async move {
    ///         let token = ctx.get::<CsrfToken>().map(|t| t.0.clone()).unwrap_or_default();
    ///         HttpResponse::text(StatusCode::OK, format!("<meta name=\"csrf\" content=\"{token}\">"))
    ///     })
    ///     .post("/transfer", |_| async { HttpResponse::text(StatusCode::OK, "done") });
    /// ```
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[must_use]
    pub struct Csrf {
        cookie_name: String,
        header_name: String,
        secure: bool,
        same_site: SameSite,
        max_age: Option<Duration>,
    }

    impl Default for Csrf {
        fn default() -> Self {
            Csrf::new()
        }
    }

    impl Csrf {
        /// Cookie `__Host-csrf` (`Secure`, `SameSite=Strict`, readable by
        /// scripts), header `X-CSRF-Token`.
        pub fn new() -> Self {
            Csrf {
                cookie_name: "__Host-csrf".to_string(),
                header_name: "X-CSRF-Token".to_string(),
                secure: true,
                same_site: SameSite::Strict,
                max_age: None,
            }
        }

        /// Sets the cookie name.
        pub fn cookie_name(mut self, name: impl Into<String>) -> Self {
            self.cookie_name = name.into();
            self
        }

        /// Sets the request header the token is expected in.
        pub fn header_name(mut self, name: impl Into<String>) -> Self {
            self.header_name = name.into();
            self
        }

        /// Whether the cookie is `Secure` (default `true`).
        pub fn secure(mut self, secure: bool) -> Self {
            self.secure = secure;
            self
        }

        /// Sets the cookie's `SameSite` attribute (default `Strict`).
        pub fn same_site(mut self, same_site: SameSite) -> Self {
            self.same_site = same_site;
            self
        }

        /// Sets the cookie's `Max-Age` (default: a session cookie).
        pub fn max_age(mut self, age: Duration) -> Self {
            self.max_age = Some(age);
            self
        }

        fn cookie(&self, token: &str) -> SetCookie {
            let cookie = SetCookie::new(self.cookie_name.clone(), token)
                .http_only(false) // scripts must read it to echo it
                .secure(self.secure)
                .same_site(self.same_site);
            match self.max_age {
                Some(age) => cookie.max_age(age),
                None => cookie,
            }
        }

        /// The token from the request cookie, if it looks like one of ours.
        fn cookie_token(&self, ctx: &Ctx) -> Option<String> {
            ctx.cookie(&self.cookie_name).filter(|t| {
                (32..=128).contains(&t.len())
                    && t.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
        }
    }

    fn forbidden() -> HttpResponse {
        HttpResponse::text(StatusCode::FORBIDDEN, "403 CSRF token missing or incorrect")
    }

    impl Router {
        /// Installs [`Csrf`] protection as a middleware. Requires the `hash`
        /// feature.
        pub fn csrf(mut self, csrf: Csrf) -> Self {
            if let Err(error) = csrf.cookie("probe").to_header_value() {
                self.push_error(RouteError::Config(format!(
                    "CSRF cookie '{}' cannot be set: {error}",
                    csrf.cookie_name
                )));
            }
            let csrf = Arc::new(csrf);
            self.layer(move |ctx, next| {
                let csrf = csrf.clone();
                async move {
                    let cookie = csrf.cookie_token(&ctx);
                    if ctx.method().is_safe() {
                        let (token, fresh) = match cookie {
                            Some(token) => (token, false),
                            None => match ferroly::hash::random::token_base64url(32) {
                                Ok(token) => (token, true),
                                Err(_) => {
                                    return HttpResponse::text(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        "500 Internal Server Error",
                                    )
                                }
                            },
                        };
                        ctx.set(CsrfToken(token.clone()));
                        let resp = next.run(ctx).await;
                        if !fresh {
                            return resp;
                        }
                        // An unusable cookie configuration was reported by
                        // `validate`; without the cookie, unsafe requests
                        // are refused, which is the safe outcome.
                        match csrf.cookie(&token).to_header_value() {
                            Ok(value) => {
                                let mut resp = resp;
                                resp.headers.append("Set-Cookie", value);
                                resp
                            }
                            Err(_) => resp,
                        }
                    } else {
                        let header = ctx.header(&csrf.header_name).map(str::to_string);
                        match (cookie, header) {
                            (Some(cookie), Some(header))
                                if ferroly::hash::ct_eq(cookie.as_bytes(), header.as_bytes()) =>
                            {
                                ctx.set(CsrfToken(cookie));
                                next.run(ctx).await
                            }
                            _ => forbidden(),
                        }
                    }
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_charset_is_strict() {
        assert!(is_valid_request_id("req-42"));
        assert!(is_valid_request_id("a.b_c-D9"));
        assert!(!is_valid_request_id(""));
        assert!(!is_valid_request_id("has space"));
        assert!(!is_valid_request_id("quote\""));
        assert!(!is_valid_request_id("semi;colon"));
        assert!(!is_valid_request_id("ünï"));
        assert!(!is_valid_request_id(&"a".repeat(129)));
        assert!(is_valid_request_id(&"a".repeat(128)));
    }

    #[test]
    fn generated_ids_are_valid_and_distinct() {
        let a = generate_request_id();
        let b = generate_request_id();
        assert_eq!(a.len(), 32);
        assert!(is_valid_request_id(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn cors_never_pairs_wildcard_with_credentials() {
        let cors = Cors::new().allow_any_origin().allow_credentials(true);
        assert_eq!(
            cors.decide("https://evil.test"),
            Some(("*".to_string(), false))
        );
        let cors = cors.allow_origin("https://app.test/");
        assert_eq!(
            cors.decide("https://app.test"),
            Some(("https://app.test".to_string(), true))
        );
        assert_eq!(Cors::new().decide("https://app.test"), None);
    }

    #[test]
    fn security_headers_defaults() {
        let headers = SecurityHeaders::new();
        let plain = headers.headers_for(false);
        assert!(!plain.iter().any(|(k, _)| k == "Strict-Transport-Security"));
        assert!(plain.contains(&("X-Content-Type-Options".into(), "nosniff".into())));
        assert!(plain.contains(&("X-Frame-Options".into(), "DENY".into())));
        assert!(plain.contains(&(
            "Content-Security-Policy".into(),
            "frame-ancestors 'none'".into()
        )));
        assert!(plain.contains(&("Referrer-Policy".into(), "no-referrer".into())));

        let tls = headers.headers_for(true);
        assert!(tls.contains(&(
            "Strict-Transport-Security".into(),
            "max-age=31536000; includeSubDomains".into()
        )));
    }

    #[test]
    fn csp_gets_matching_frame_ancestors() {
        let headers = SecurityHeaders::new()
            .frame_options(FrameOptions::SameOrigin)
            .content_security_policy("default-src 'self';");
        assert_eq!(
            headers.csp_value().as_deref(),
            Some("default-src 'self'; frame-ancestors 'self'")
        );
        let own = SecurityHeaders::new().content_security_policy("frame-ancestors https://a.test");
        assert_eq!(
            own.csp_value().as_deref(),
            Some("frame-ancestors https://a.test")
        );
    }

    #[test]
    fn vary_is_not_duplicated() {
        let resp = add_vary(
            HttpResponse::ok().header("Vary", "Accept, origin"),
            "Origin",
        );
        assert_eq!(resp.headers.get_all("vary").count(), 1);
        let resp = add_vary(HttpResponse::ok().header("Vary", "Accept"), "Origin");
        assert_eq!(resp.headers.get_all("vary").count(), 2);
    }
}
