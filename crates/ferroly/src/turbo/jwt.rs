//! Bearer-token (JWT) authentication and scope/role guards. Requires the
//! `auth` feature.

use std::sync::Arc;

use ferroly::auth::{
    Algorithm, Jwks, JwtError, RemoteJwks, Scopes, Secret, Validation, MIN_SECRET_LEN,
};
use ferroly::codec::Value;
use ferroly::http::{BoxFuture, HttpResponse, StatusCode};

use super::ctx::Ctx;
use super::error::RouteError;
use super::router::{Group, Router};

/// The verified claims of the request's bearer token, stored in the request's
/// [`Extensions`](super::Extensions) by the JWT authenticators. Read them
/// with [`Ctx::claims`].
///
/// They are present only after the token's signature and claims have been
/// verified; a handler never sees the claims of a rejected token.
#[derive(Debug, Clone, PartialEq)]
pub struct Claims(pub Value);

impl Claims {
    /// The claim called `name`.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    /// The `sub` (subject) claim.
    pub fn subject(&self) -> Option<&str> {
        self.0.get("sub").and_then(Value::as_str)
    }

    /// The token's scopes (the `scope` / `scp` claims); see
    /// [`Scopes::from_claims`].
    pub fn scopes(&self) -> Scopes {
        Scopes::from_claims(&self.0)
    }

    /// The token's roles (the `roles` claim); see [`Scopes::roles`].
    pub fn roles(&self) -> Scopes {
        Scopes::roles(&self.0)
    }

    /// All claims.
    pub fn value(&self) -> &Value {
        &self.0
    }
}

impl Ctx {
    /// The verified claims of the request's bearer token, when a JWT
    /// authenticator ([`Router::jwt`] and friends) accepted it.
    pub fn claims(&self) -> Option<Arc<Claims>> {
        self.get::<Claims>()
    }
}

/// The bearer token of the request: the credentials of an
/// `Authorization: Bearer <token>` header (scheme matched
/// case-insensitively).
pub fn bearer_token(ctx: &Ctx) -> Option<&str> {
    let header = ctx.header("authorization")?;
    let (scheme, token) = header.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

enum Verifier {
    /// The original `jwt_auth` behaviour: HS256, `exp` checked only when
    /// present, any non-empty secret.
    LegacyHs256(Secret),
    Hmac {
        alg: Algorithm,
        secret: Secret,
        validation: Validation,
    },
    Keys {
        keys: Jwks,
        validation: Validation,
    },
    Remote {
        keys: Arc<RemoteJwks>,
        validation: Validation,
    },
}

/// A JWT bearer-token authenticator: how tokens are verified, and what
/// happens without one. Install it router-wide with [`Router::jwt`] or for a
/// group with [`Group::jwt`].
///
/// On success the verified claims are stored for the handler
/// ([`Ctx::claims`]). A missing or invalid token is answered `401` with a
/// `WWW-Authenticate: Bearer` challenge; the response never says *why* a
/// token was rejected.
///
/// ```
/// use ferroly::auth::Validation;
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{require_scopes, JwtAuth, Router};
///
/// let auth = JwtAuth::hs256(
///     "a-shared-secret-of-at-least-16-bytes",
///     Validation::new().issuer("https://issuer.example").audience("orders-api"),
/// );
///
/// let _router = Router::new().group("/orders", |g| {
///     g.jwt(auth)
///         .filter(require_scopes(&["orders:read"]))
///         .get("/", |ctx| async move {
///             let who = ctx.claims().and_then(|c| c.subject().map(str::to_string));
///             HttpResponse::text(StatusCode::OK, who.unwrap_or_default())
///         })
/// });
/// ```
#[derive(Clone)]
#[must_use]
pub struct JwtAuth {
    verifier: Arc<Verifier>,
    optional: bool,
}

impl std::fmt::Debug for JwtAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &*self.verifier {
            Verifier::LegacyHs256(_) => "HS256 (legacy validation)",
            Verifier::Hmac { .. } => "HMAC",
            Verifier::Keys { .. } => "JWKS",
            Verifier::Remote { .. } => "remote JWKS",
        };
        f.debug_struct("JwtAuth")
            .field("verifier", &kind)
            .field("optional", &self.optional)
            .finish()
    }
}

impl JwtAuth {
    fn with(verifier: Verifier) -> Self {
        JwtAuth {
            verifier: Arc::new(verifier),
            optional: false,
        }
    }

    /// HS256 tokens under a shared `secret`, validated against `validation`
    /// ([`decode_hs256_with`](ferroly::auth::decode_hs256_with)).
    /// [`Validation::new`] requires `exp`, and the secret must be at least
    /// [`MIN_SECRET_LEN`] bytes — a shorter one rejects every token and is
    /// reported by [`Router::validate`].
    pub fn hs256(secret: impl Into<Vec<u8>>, validation: Validation) -> Self {
        JwtAuth::hmac(Algorithm::Hs256, secret, validation)
    }

    /// Tokens signed with the given HMAC algorithm (`HS256`, `HS384`,
    /// `HS512`) under a shared `secret`
    /// ([`decode_hmac_with`](ferroly::auth::decode_hmac_with)). The token's
    /// `alg` header must equal `alg`; it never selects the algorithm.
    pub fn hmac(alg: Algorithm, secret: impl Into<Vec<u8>>, validation: Validation) -> Self {
        JwtAuth::with(Verifier::Hmac {
            alg,
            secret: Secret::new(secret),
            validation,
        })
    }

    /// Tokens signed by one of the public keys in a fixed key set
    /// ([`Jwks::verify`]) — RS256 / ES256 and the other algorithms the keys
    /// support.
    pub fn jwks(keys: Jwks, validation: Validation) -> Self {
        JwtAuth::with(Verifier::Keys { keys, validation })
    }

    /// Tokens signed by a key from a remote JWKS endpoint, fetched and
    /// cached by [`RemoteJwks`] ([`RemoteJwks::verify`]). Verification may
    /// await a key fetch, so this authenticator is asynchronous.
    pub fn remote_jwks(keys: Arc<RemoteJwks>, validation: Validation) -> Self {
        JwtAuth::with(Verifier::Remote { keys, validation })
    }

    /// HS256 with the validation of the original
    /// [`decode_hs256`](ferroly::auth::decode_hs256): `exp` and `nbf` are
    /// checked only when present, and any non-empty secret is accepted. Kept
    /// for [`Router::jwt_auth`]; prefer [`hs256`](Self::hs256).
    pub fn legacy_hs256(secret: impl Into<Vec<u8>>) -> Self {
        JwtAuth::with(Verifier::LegacyHs256(Secret::new(secret)))
    }

    /// Whether a request **without** a token is let through (anonymous, no
    /// [`Claims`]) instead of being answered `401`. A request that presents a
    /// token is always verified, and rejected if the token is invalid.
    /// Default `false`.
    pub fn optional(mut self, optional: bool) -> Self {
        self.optional = optional;
        self
    }

    /// A configuration that cannot accept any token, if there is one.
    fn config_problem(&self) -> Option<String> {
        match &*self.verifier {
            Verifier::Hmac { alg, secret, .. } if secret.len() < MIN_SECRET_LEN => Some(format!(
                "JWT {} secret is shorter than {MIN_SECRET_LEN} bytes; every token will be rejected",
                alg.as_str()
            )),
            Verifier::Hmac { alg, .. } if !alg.is_hmac() => Some(format!(
                "JWT algorithm {} is not an HMAC algorithm; every token will be rejected",
                alg.as_str()
            )),
            Verifier::LegacyHs256(secret) if secret.is_empty() => {
                Some("JWT secret is empty; every token will be rejected".to_string())
            }
            Verifier::Keys { keys, .. } if keys.is_empty() => {
                Some("JWT key set is empty; every token will be rejected".to_string())
            }
            _ => None,
        }
    }

    /// Verifies `token` and returns its claims.
    pub async fn verify(&self, token: &str) -> Result<Claims, JwtError> {
        let claims = match &*self.verifier {
            Verifier::LegacyHs256(secret) => ferroly::auth::decode_hs256(token, secret.expose())?,
            Verifier::Hmac {
                alg,
                secret,
                validation,
            } => ferroly::auth::decode_hmac_with(*alg, token, secret.expose(), validation)?,
            Verifier::Keys { keys, validation } => keys.verify(token, validation)?,
            Verifier::Remote { keys, validation } => keys.verify(token, validation).await?,
        };
        Ok(Claims(claims))
    }

    /// The authenticator as an asynchronous filter.
    fn into_filter(
        self,
    ) -> impl Fn(&Ctx) -> BoxFuture<'static, Option<HttpResponse>> + Send + Sync + 'static {
        move |ctx: &Ctx| {
            let token = bearer_token(ctx).map(str::to_string);
            let extensions = ctx.extensions();
            let auth = self.clone();
            Box::pin(async move {
                let Some(token) = token else {
                    return if auth.optional {
                        None
                    } else {
                        Some(unauthorized("401 missing bearer token", "Bearer"))
                    };
                };
                match auth.verify(&token).await {
                    Ok(claims) => {
                        extensions.insert(claims);
                        None
                    }
                    Err(_) => Some(unauthorized(
                        "401 invalid token",
                        "Bearer error=\"invalid_token\"",
                    )),
                }
            })
        }
    }
}

fn unauthorized(body: &'static str, challenge: &'static str) -> HttpResponse {
    HttpResponse::text(StatusCode::UNAUTHORIZED, body).header("WWW-Authenticate", challenge)
}

impl Router {
    /// Sets a [`JwtAuth`] as the router's authenticator (it runs before all
    /// filters, for every request).
    pub fn jwt(mut self, auth: JwtAuth) -> Self {
        if let Some(problem) = auth.config_problem() {
            self.push_error(RouteError::Config(problem));
        }
        self.auth_async(auth.into_filter())
    }

    /// Sets an HS256-JWT authenticator with the **original, lenient**
    /// validation: a valid `Authorization: Bearer <token>` signed with
    /// `secret` is required (else `401`), but `exp` is checked only when the
    /// token carries it, and a secret of any length is accepted.
    ///
    /// That behaviour is kept so existing deployments — and the tokens they
    /// have issued — keep working. For new code use
    /// [`jwt_auth_with`](Self::jwt_auth_with), which requires `exp` and a
    /// secret of at least 16 bytes by default. Requires the `auth` feature.
    ///
    /// The verified claims are available to handlers through
    /// [`Ctx::claims`].
    pub fn jwt_auth(self, secret: impl Into<Vec<u8>>) -> Self {
        self.jwt(JwtAuth::legacy_hs256(secret))
    }

    /// Sets an HS256-JWT authenticator that validates claims against
    /// `validation` — expiry (required by default), `nbf`, `iat`, and
    /// whatever issuer, audience, subject, required claims and leeway it
    /// names. Shorthand for [`jwt`](Self::jwt) with [`JwtAuth::hs256`].
    ///
    /// ```
    /// use ferroly::auth::Validation;
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::turbo::Router;
    ///
    /// let _router = Router::new()
    ///     .jwt_auth_with(
    ///         "a-shared-secret-of-at-least-16-bytes",
    ///         Validation::new().issuer("https://issuer.example").audience("orders-api"),
    ///     )
    ///     .get("/me", |ctx| async move {
    ///         let subject = ctx.claims().and_then(|c| c.subject().map(str::to_string));
    ///         HttpResponse::text(StatusCode::OK, subject.unwrap_or_default())
    ///     });
    /// ```
    pub fn jwt_auth_with(self, secret: impl Into<Vec<u8>>, validation: Validation) -> Self {
        self.jwt(JwtAuth::hs256(secret, validation))
    }

    /// Like [`jwt_auth_with`](Self::jwt_auth_with) for any HMAC algorithm
    /// (`HS256`, `HS384`, `HS512`). Shorthand for [`jwt`](Self::jwt) with
    /// [`JwtAuth::hmac`].
    pub fn jwt_auth_hmac_with(
        self,
        alg: Algorithm,
        secret: impl Into<Vec<u8>>,
        validation: Validation,
    ) -> Self {
        self.jwt(JwtAuth::hmac(alg, secret, validation))
    }

    /// Sets an authenticator that verifies tokens against a remote JWKS
    /// endpoint (RS256 / ES256 …), such as an identity provider's
    /// `/.well-known/jwks.json`. Shorthand for [`jwt`](Self::jwt) with
    /// [`JwtAuth::remote_jwks`].
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use ferroly::auth::{RemoteJwks, Validation};
    /// use ferroly::turbo::Router;
    ///
    /// let keys = Arc::new(RemoteJwks::new("https://issuer.example/.well-known/jwks.json"));
    /// let _router = Router::new().jwt_auth_jwks(
    ///     keys,
    ///     Validation::new().issuer("https://issuer.example").audience("orders-api"),
    /// );
    /// ```
    pub fn jwt_auth_jwks(self, keys: Arc<RemoteJwks>, validation: Validation) -> Self {
        self.jwt(JwtAuth::remote_jwks(keys, validation))
    }
}

impl Group {
    /// Requires a valid bearer token for requests under this group's prefix
    /// (as a group filter; see the
    /// [scoping guarantee](Group#scoping-guarantee)).
    pub fn jwt(mut self, auth: JwtAuth) -> Self {
        if let Some(problem) = auth.config_problem() {
            self.push_error(RouteError::Config(problem));
        }
        self.filter_async(auth.into_filter())
    }
}

/// Which claim a guard reads.
#[derive(Clone, Copy)]
enum Source {
    Scopes,
    Roles,
}

fn guard(
    source: Source,
    all: bool,
    wanted: &[&str],
) -> impl Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static {
    let wanted: Vec<String> = wanted.iter().map(|s| (*s).to_string()).collect();
    move |ctx: &Ctx| {
        let Some(claims) = ctx.claims() else {
            return Some(unauthorized("401 authentication required", "Bearer"));
        };
        let held = match source {
            Source::Scopes => claims.scopes(),
            Source::Roles => claims.roles(),
        };
        let wanted_refs: Vec<&str> = wanted.iter().map(String::as_str).collect();
        let ok = if all {
            held.has_all(&wanted_refs)
        } else {
            held.has_any(&wanted_refs)
        };
        if ok {
            return None;
        }
        let mut resp = HttpResponse::text(StatusCode::FORBIDDEN, "403 insufficient permissions");
        if matches!(source, Source::Scopes) {
            // Scope tokens cannot contain `"` or `\`, but be defensive about
            // what goes into a quoted-string.
            let scope: String = wanted
                .join(" ")
                .chars()
                .filter(|c| !matches!(c, '"' | '\\') && !c.is_control())
                .collect();
            resp.headers.set(
                "WWW-Authenticate",
                format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\""),
            );
        }
        Some(resp)
    }
}

/// A filter that requires the verified token to hold **all** of `required`
/// scopes ([`Scopes::has_all`]).
///
/// It answers `401` when the request has no verified [`Claims`] (no JWT
/// authenticator ran, or it is [`optional`](JwtAuth::optional) and the
/// request was anonymous) and `403` with an `insufficient_scope` challenge
/// when a scope is missing. Use it with [`Router::filter`], [`Group::filter`]
/// or [`Route::filter`](super::Route::filter).
pub fn require_scopes(
    required: &[&str],
) -> impl Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static {
    guard(Source::Scopes, true, required)
}

/// Like [`require_scopes`], satisfied by **any one** of `accepted`.
pub fn require_any_scope(
    accepted: &[&str],
) -> impl Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static {
    guard(Source::Scopes, false, accepted)
}

/// A filter that requires the verified token to hold **all** of `required`
/// roles (the `roles` claim; see [`Scopes::roles`]). `401` without verified
/// claims, `403` when a role is missing.
pub fn require_roles(
    required: &[&str],
) -> impl Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static {
    guard(Source::Roles, true, required)
}

/// Like [`require_roles`], satisfied by **any one** of `accepted`.
pub fn require_any_role(
    accepted: &[&str],
) -> impl Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static {
    guard(Source::Roles, false, accepted)
}
