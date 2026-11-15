//! JWT (RFC 7519) / JWS compact (RFC 7515) parsing, signing and validation.

use std::sync::Arc;

use ferroly::codec::{json, Value};
use ferroly::hash::{base64, ct_eq, hmac, Sha256, Sha384, Sha512};
use ferroly_derive::FerrolyError;

use super::replay::ReplayCache;

/// The smallest HMAC secret the validating API accepts, in bytes: the
/// RFC 7518 §3.2 minimum for `HS256`.
///
/// The minimum is **per algorithm** — a key at least as long as the hash
/// output: 32 bytes for HS256, 48 for HS384, 64 for HS512
/// ([`Algorithm::min_secret_len`]). [`encode_hmac`], [`encode_with_header`],
/// [`decode_hs256_with`] and [`decode_hmac_with`] reject anything shorter with
/// [`JwtError::WeakSecret`]. Generate secrets with
/// [`ferroly::hash::random`](crate::hash::random), never from a passphrase.
pub const MIN_SECRET_LEN: usize = 32;

/// Tokens longer than this are rejected before any decoding work.
pub const MAX_TOKEN_LEN: usize = 64 * 1024;

/// Errors from JWT signing and verification.
#[derive(Debug, Clone, FerrolyError, PartialEq, Eq)]
#[non_exhaustive]
pub enum JwtError {
    /// The token is not a well-formed `header.payload.signature`: wrong number
    /// of parts, non-canonical base64url, invalid JSON, a header or claim set
    /// that is not a JSON object, duplicate member names, or a registered
    /// claim of the wrong JSON type.
    #[error("malformed JWT")]
    Malformed,
    /// The `alg` header is missing, unknown, not the algorithm the caller
    /// asked for, or not valid for the key type.
    #[error("unsupported or disallowed JWT algorithm")]
    UnsupportedAlg,
    /// The signature did not verify.
    #[error("invalid JWT signature")]
    InvalidSignature,
    /// The token's `exp` claim is in the past.
    #[error("JWT expired")]
    Expired,
    /// The token's `nbf` (not-before) claim is in the future.
    #[error("JWT not yet valid")]
    NotYetValid,
    /// The HMAC secret is empty or shorter than the algorithm's RFC 7518
    /// minimum ([`Algorithm::min_secret_len`]: 32 / 48 / 64 bytes).
    #[error("HMAC secret is empty or too short")]
    WeakSecret,
    /// The key cannot be used: an asymmetric public key or certificate passed
    /// as an HMAC secret, or an invalid / unsupported JWK.
    #[error("invalid or unsuitable key")]
    InvalidKey,
    /// A claim required by the [`Validation`] is absent.
    #[error("missing required claim `{0}`")]
    MissingClaim(String),
    /// The `aud` claim does not contain any expected audience.
    #[error("JWT audience not accepted")]
    InvalidAudience,
    /// The `iss` claim is not an expected issuer.
    #[error("JWT issuer not accepted")]
    InvalidIssuer,
    /// The `sub` claim is not the expected subject.
    #[error("JWT subject not accepted")]
    InvalidSubject,
    /// The `iat` (issued-at) claim is in the future.
    #[error("JWT issued in the future")]
    IssuedInFuture,
    /// The token is older than the configured maximum age.
    #[error("JWT too old")]
    TooOld,
    /// The `typ` header does not match the expected type.
    #[error("unexpected JWT type")]
    InvalidType,
    /// The header carries a `crit` parameter; no critical extensions are
    /// understood, so the token must be rejected (RFC 7515 §4.1.11).
    #[error("unsupported critical JWT header parameter")]
    UnsupportedCritical,
    /// No key in the key set matches the token's `kid` / algorithm.
    #[error("no matching verification key")]
    UnknownKey,
    /// A remote key set could not be fetched or parsed.
    #[error("fetching the key set failed: {0}")]
    KeyFetch(String),
    /// RS\* / PS\* / ES\* verification needs a rustls crypto provider and
    /// none is available: the `tls-ring` feature is off and the application
    /// has not installed a process default (e.g.
    /// `rustls::crypto::aws_lc_rs::default_provider().install_default()`).
    /// EdDSA needs no provider.
    #[error(
        "no rustls CryptoProvider is installed: install one before verifying RS*/PS*/ES* \
         (e.g. `rustls::crypto::aws_lc_rs::default_provider().install_default()`) \
         or enable ferroly's `tls-ring` feature"
    )]
    NoCryptoProvider,
    /// The token's `jti` was already used: the [`ReplayCache`] configured with
    /// [`Validation::replay_cache`] has seen it before.
    #[error("JWT replay detected")]
    Replayed,
}

/// A JWS signature algorithm (RFC 7518 §3, RFC 8037).
///
/// Only the HMAC algorithms can sign; every other one is verify-only, through
/// a JWK (`Jwk`, `Jwks`, `RemoteJwks`; feature `http`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Algorithm {
    /// HMAC with SHA-256 (symmetric).
    Hs256,
    /// HMAC with SHA-384 (symmetric).
    Hs384,
    /// HMAC with SHA-512 (symmetric).
    Hs512,
    /// RSASSA-PKCS1-v1_5 with SHA-256 (verify-only; rustls provider).
    Rs256,
    /// RSASSA-PKCS1-v1_5 with SHA-384 (verify-only; rustls provider).
    Rs384,
    /// RSASSA-PKCS1-v1_5 with SHA-512 (verify-only; rustls provider).
    Rs512,
    /// RSASSA-PSS with SHA-256 and MGF1-SHA-256, 32-byte salt (verify-only;
    /// rustls provider).
    Ps256,
    /// RSASSA-PSS with SHA-384 and MGF1-SHA-384, 48-byte salt (verify-only;
    /// rustls provider).
    Ps384,
    /// RSASSA-PSS with SHA-512 and MGF1-SHA-512, 64-byte salt (verify-only;
    /// rustls provider).
    Ps512,
    /// ECDSA with P-256 and SHA-256 (verify-only; rustls provider).
    Es256,
    /// ECDSA with P-384 and SHA-384 (verify-only; rustls provider).
    Es384,
    /// ECDSA with P-521 and SHA-512 (verify-only; needs a provider that has
    /// it, such as `aws-lc-rs` — `ring` does not).
    Es512,
    /// EdDSA with Ed25519 (RFC 8037; verify-only; [`ferroly::sig`](crate::sig),
    /// no provider needed).
    EdDsa,
}

impl Algorithm {
    /// The `alg` header value (`"HS256"`, …, `"EdDSA"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Algorithm::Hs256 => "HS256",
            Algorithm::Hs384 => "HS384",
            Algorithm::Hs512 => "HS512",
            Algorithm::Rs256 => "RS256",
            Algorithm::Rs384 => "RS384",
            Algorithm::Rs512 => "RS512",
            Algorithm::Ps256 => "PS256",
            Algorithm::Ps384 => "PS384",
            Algorithm::Ps512 => "PS512",
            Algorithm::Es256 => "ES256",
            Algorithm::Es384 => "ES384",
            Algorithm::Es512 => "ES512",
            Algorithm::EdDsa => "EdDSA",
        }
    }

    /// Parses an `alg` header value. Names are case-sensitive; `"none"` and
    /// anything unknown yield `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "HS256" => Algorithm::Hs256,
            "HS384" => Algorithm::Hs384,
            "HS512" => Algorithm::Hs512,
            "RS256" => Algorithm::Rs256,
            "RS384" => Algorithm::Rs384,
            "RS512" => Algorithm::Rs512,
            "PS256" => Algorithm::Ps256,
            "PS384" => Algorithm::Ps384,
            "PS512" => Algorithm::Ps512,
            "ES256" => Algorithm::Es256,
            "ES384" => Algorithm::Es384,
            "ES512" => Algorithm::Es512,
            "EdDSA" => Algorithm::EdDsa,
            _ => return None,
        })
    }

    /// Whether this is a symmetric (shared-secret) algorithm.
    pub fn is_hmac(self) -> bool {
        matches!(self, Algorithm::Hs256 | Algorithm::Hs384 | Algorithm::Hs512)
    }

    /// The shortest secret RFC 7518 §3.2 allows for an HMAC algorithm (the
    /// hash output size: 32 / 48 / 64 bytes), or `None` for asymmetric
    /// algorithms. Enforced by every validating HMAC function
    /// ([`JwtError::WeakSecret`]).
    pub fn min_secret_len(self) -> Option<usize> {
        match self {
            Algorithm::Hs256 => Some(32),
            Algorithm::Hs384 => Some(48),
            Algorithm::Hs512 => Some(64),
            _ => None,
        }
    }

    /// The same as [`min_secret_len`](Self::min_secret_len): RFC 7518 makes
    /// the recommended length the minimum, and so does this crate.
    pub fn recommended_secret_len(self) -> Option<usize> {
        self.min_secret_len()
    }
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The protected header of a token.
///
/// Build one to mint a token with a `kid` or a non-default `typ`
/// ([`encode_with_header`]); [`decode_header`] returns one for an incoming
/// token (unverified — use it only to pick a key).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Header {
    /// The signature algorithm.
    pub alg: Algorithm,
    /// The `typ` parameter (`"JWT"` by default when minting).
    pub typ: Option<String>,
    /// The `kid` (key id) parameter.
    pub kid: Option<String>,
    /// The `cty` (content type) parameter.
    pub cty: Option<String>,
}

impl Header {
    /// A header for `alg` with `typ: "JWT"` and no `kid`.
    pub fn new(alg: Algorithm) -> Self {
        Header {
            alg,
            typ: Some("JWT".to_string()),
            kid: None,
            cty: None,
        }
    }

    /// Sets the `kid` (key id) parameter.
    pub fn with_kid(mut self, kid: impl Into<String>) -> Self {
        self.kid = Some(kid.into());
        self
    }

    /// Sets the `typ` parameter (for example `"at+jwt"`).
    pub fn with_typ(mut self, typ: impl Into<String>) -> Self {
        self.typ = Some(typ.into());
        self
    }

    fn to_json(&self) -> String {
        let mut members = vec![("alg".to_string(), Value::from(self.alg.as_str()))];
        for (name, value) in [("typ", &self.typ), ("kid", &self.kid), ("cty", &self.cty)] {
            if let Some(value) = value {
                members.push((name.to_string(), Value::from(value.as_str())));
            }
        }
        json::to_string(&Value::Object(members))
    }
}

/// What to check, beyond the signature, before a token's claims are trusted.
///
/// The defaults are deliberately strict about expiry:
///
/// | Check | Default |
/// |---|---|
/// | `exp` present | **required** ([`require_exp`](Self::require_exp)) |
/// | `exp` / `nbf` / `iat` vs. the clock | checked when present, with [`leeway`](Self::leeway) |
/// | leeway | 60 seconds |
/// | `aud`, `iss`, `sub` | not checked until configured |
/// | `typ` header | not checked until configured |
/// | maximum age | none |
/// | `jti` replay | not checked until a [`replay_cache`](Self::replay_cache) is set |
///
/// ```
/// use ferroly::auth::Validation;
///
/// let validation = Validation::new()
///     .issuer("https://issuer.example")
///     .audience("orders-api")
///     .leeway(30)
///     .require_claim("sub");
/// # let _ = validation;
/// ```
#[derive(Debug, Clone)]
pub struct Validation {
    leeway: u64,
    require_exp: bool,
    validate_iat: bool,
    max_age: Option<u64>,
    audiences: Option<Vec<String>>,
    issuers: Option<Vec<String>>,
    subject: Option<String>,
    required: Vec<String>,
    typ: Option<String>,
    algorithms: Option<Vec<Algorithm>>,
    now: Option<u64>,
    /// `None`: the per-algorithm RFC 7518 minimum. `Some(n)`: the legacy API's
    /// own floor.
    min_secret_len: Option<usize>,
    replay: Option<ReplayHandle>,
    /// An issuer `iss` must equal, set internally (OIDC discovery) on top of
    /// whatever [`issuer`](Self::issuer) allows.
    required_issuer: Option<String>,
}

/// A shared [`ReplayCache`], printable.
#[derive(Clone)]
struct ReplayHandle(Arc<dyn ReplayCache>);

impl std::fmt::Debug for ReplayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplayCache")
    }
}

impl Default for Validation {
    fn default() -> Self {
        Self::new()
    }
}

impl Validation {
    /// The default policy: `exp` required, 60 s leeway, `nbf`/`iat` checked
    /// when present, no audience/issuer/subject constraint.
    pub fn new() -> Self {
        Validation {
            leeway: 60,
            require_exp: true,
            validate_iat: true,
            max_age: None,
            audiences: None,
            issuers: None,
            subject: None,
            required: Vec::new(),
            typ: None,
            algorithms: None,
            now: None,
            min_secret_len: None,
            replay: None,
            required_issuer: None,
        }
    }

    /// The behaviour of the original `decode_hs256`: no leeway, `exp` optional,
    /// `iat` ignored, any non-empty secret.
    pub(crate) fn legacy() -> Self {
        Validation {
            leeway: 0,
            require_exp: false,
            validate_iat: false,
            min_secret_len: Some(1),
            ..Validation::new()
        }
    }

    /// This policy plus the requirement that `iss` equals `issuer` (OIDC
    /// discovery pins the issuer whose metadata named the key set).
    #[cfg(feature = "http")]
    pub(crate) fn with_required_issuer(&self, issuer: &str) -> Self {
        Validation {
            required_issuer: Some(issuer.to_string()),
            ..self.clone()
        }
    }

    /// The secret floor for `alg` under this policy.
    fn secret_floor(&self, alg: Algorithm) -> usize {
        self.min_secret_len
            .or_else(|| alg.min_secret_len())
            .unwrap_or(1)
    }

    /// Clock-skew tolerance, in seconds, applied to `exp`, `nbf`, `iat` and
    /// the maximum age. Default 60; use 0 for exact comparison.
    pub fn leeway(mut self, seconds: u64) -> Self {
        self.leeway = seconds;
        self
    }

    /// Whether a token without an `exp` claim is rejected
    /// ([`JwtError::MissingClaim`]). Default `true`. An `exp` that *is*
    /// present is always enforced.
    pub fn require_exp(mut self, required: bool) -> Self {
        self.require_exp = required;
        self
    }

    /// Whether an `iat` claim in the future is rejected
    /// ([`JwtError::IssuedInFuture`]). Default `true`.
    pub fn validate_iat(mut self, validate: bool) -> Self {
        self.validate_iat = validate;
        self
    }

    /// Rejects tokens issued more than `seconds` ago ([`JwtError::TooOld`]),
    /// regardless of their `exp`. Makes the `iat` claim required.
    pub fn max_age(mut self, seconds: u64) -> Self {
        self.max_age = Some(seconds);
        self
    }

    /// Requires the `aud` claim (a string or an array of strings) to contain
    /// `audience`. Calling this again adds an alternative; the token needs
    /// only one of them.
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audiences
            .get_or_insert_with(Vec::new)
            .push(audience.into());
        self
    }

    /// Requires the `iss` claim to equal `issuer`. Calling this again adds an
    /// alternative.
    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuers
            .get_or_insert_with(Vec::new)
            .push(issuer.into());
        self
    }

    /// Requires the `sub` claim to equal `subject`.
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Requires the named claim to be present and non-null.
    pub fn require_claim(mut self, name: impl Into<String>) -> Self {
        self.required.push(name.into());
        self
    }

    /// Requires every named claim to be present and non-null.
    pub fn require_claims<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.required.extend(names.into_iter().map(Into::into));
        self
    }

    /// Requires the `typ` header to equal `typ` (ASCII case-insensitive; an
    /// `application/` prefix is ignored, per RFC 7515 §4.1.9) — for example
    /// `"at+jwt"` to accept only RFC 9068 access tokens.
    pub fn typ(mut self, typ: impl Into<String>) -> Self {
        self.typ = Some(typ.into());
        self
    }

    /// Restricts the accepted algorithms further. The entry point already
    /// fixes the algorithm family (an HMAC function never accepts `RS256`, a
    /// JWK never accepts `HS256`); this is an additional allow-list, mainly
    /// useful with key sets that hold keys of several types.
    pub fn algorithms(mut self, algorithms: &[Algorithm]) -> Self {
        self.algorithms = Some(algorithms.to_vec());
        self
    }

    /// Validates against a fixed time (Unix seconds) instead of the system
    /// clock — for tests and for re-validating archived tokens.
    pub fn current_time(mut self, unix_seconds: u64) -> Self {
        self.now = Some(unix_seconds);
        self
    }

    /// Rejects a token whose `jti` was seen before ([`JwtError::Replayed`]),
    /// using `cache` to remember the identifiers of accepted tokens. Makes
    /// `jti` required ([`JwtError::MissingClaim`]); it must be a string
    /// ([`JwtError::Malformed`] otherwise). It also makes a bounded lifetime
    /// required: a token with neither an `exp` claim nor an `iat` checked by
    /// [`max_age`](Self::max_age) is [`JwtError::MissingClaim`]`("exp")`,
    /// even with [`require_exp(false)`](Self::require_exp), because its
    /// identifier would have to be remembered forever.
    ///
    /// The check runs **last**, after the signature and every other claim
    /// check passed, so a rejected token never consumes an identifier. Each
    /// identifier is remembered, keyed by `iss` and `jti`, until the token
    /// could no longer be accepted anyway: the first second at or after
    /// `exp + leeway`, or after `iat + max_age + leeway` if that is sooner.
    /// Clones of this `Validation` share the cache. [`MemoryReplayCache`](super::MemoryReplayCache) is a
    /// bounded in-process implementation; implement [`ReplayCache`] over a
    /// shared store (Redis `SET NX EX`, a unique database index) when several
    /// processes accept the same tokens.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use ferroly::auth::{
    ///     decode_hs256_with, encode_hmac, Algorithm, JwtError, MemoryReplayCache, Validation,
    /// };
    /// use ferroly::codec::Value;
    ///
    /// let secret = [7u8; 32];
    /// let claims = Value::Object(vec![
    ///     ("jti".into(), "4f1c9d2e".into()),
    ///     ("exp".into(), Value::Int(4_102_444_800)),
    /// ]);
    /// let token = encode_hmac(Algorithm::Hs256, &claims, &secret).unwrap();
    ///
    /// let validation = Validation::new().replay_cache(Arc::new(MemoryReplayCache::new(10_000)));
    /// assert!(decode_hs256_with(&token, &secret, &validation).is_ok());
    /// assert_eq!(decode_hs256_with(&token, &secret, &validation), Err(JwtError::Replayed));
    /// ```
    pub fn replay_cache(mut self, cache: Arc<dyn ReplayCache>) -> Self {
        self.replay = Some(ReplayHandle(cache));
        self
    }

    fn allows(&self, alg: Algorithm) -> bool {
        match &self.algorithms {
            Some(list) => list.contains(&alg),
            None => true,
        }
    }
}

// ---- token structure -------------------------------------------------------

/// A token split into its parts, with the header parsed but nothing verified.
pub(crate) struct Parts<'a> {
    /// `base64url(header) "." base64url(payload)` — the bytes that are signed.
    pub(crate) signing_input: &'a str,
    payload_b64: &'a str,
    /// The decoded signature bytes.
    pub(crate) signature: Vec<u8>,
    /// The `alg` header value, verbatim.
    alg: String,
    typ: Option<String>,
    pub(crate) kid: Option<String>,
    cty: Option<String>,
}

fn string_member(object: &Value, name: &str) -> Result<Option<String>, JwtError> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(_) => Err(JwtError::Malformed),
    }
}

/// Parses strict base64url JSON that must be an object without duplicate
/// member names.
fn decode_object(segment: &str) -> Result<Value, JwtError> {
    let bytes = base64::decode_url_no_pad(segment).map_err(|_| JwtError::Malformed)?;
    let value = json::from_slice(&bytes).map_err(|_| JwtError::Malformed)?;
    let members = value.as_object().ok_or(JwtError::Malformed)?;
    for (i, (name, _)) in members.iter().enumerate() {
        if members[..i].iter().any(|(other, _)| other == name) {
            return Err(JwtError::Malformed);
        }
    }
    Ok(value)
}

impl<'a> Parts<'a> {
    pub(crate) fn parse(token: &'a str) -> Result<Self, JwtError> {
        if token.len() > MAX_TOKEN_LEN {
            return Err(JwtError::Malformed);
        }
        let mut pieces = token.split('.');
        let (h, p, s) = match (pieces.next(), pieces.next(), pieces.next(), pieces.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(JwtError::Malformed),
        };
        if h.is_empty() || p.is_empty() {
            return Err(JwtError::Malformed);
        }

        let header = decode_object(h)?;
        if header.get("crit").is_some() {
            return Err(JwtError::UnsupportedCritical);
        }
        let alg = match header.get("alg") {
            Some(Value::Str(alg)) => alg.clone(),
            // Absent or not a string: nothing we could verify with.
            _ => return Err(JwtError::UnsupportedAlg),
        };
        let typ = string_member(&header, "typ")?;
        let kid = string_member(&header, "kid")?;
        let cty = string_member(&header, "cty")?;
        let signature = base64::decode_url_no_pad(s).map_err(|_| JwtError::Malformed)?;

        Ok(Parts {
            signing_input: &token[..h.len() + 1 + p.len()],
            payload_b64: p,
            signature,
            alg,
            typ,
            kid,
            cty,
        })
    }

    /// The header's algorithm, if it is one this crate knows.
    pub(crate) fn algorithm(&self) -> Result<Algorithm, JwtError> {
        Algorithm::from_name(&self.alg).ok_or(JwtError::UnsupportedAlg)
    }

    /// Header checks that do not depend on the key: the `typ` expectation and
    /// the caller's algorithm allow-list.
    pub(crate) fn check_header(
        &self,
        expected: Algorithm,
        validation: &Validation,
    ) -> Result<(), JwtError> {
        if self.algorithm()? != expected || !validation.allows(expected) {
            return Err(JwtError::UnsupportedAlg);
        }
        if let Some(want) = &validation.typ {
            let got = self.typ.as_deref().ok_or(JwtError::InvalidType)?;
            if !normalized_typ(got).eq_ignore_ascii_case(normalized_typ(want)) {
                return Err(JwtError::InvalidType);
            }
        }
        Ok(())
    }

    /// Decodes and validates the claims. Call only after the signature has
    /// been verified.
    pub(crate) fn claims(&self, validation: &Validation) -> Result<Value, JwtError> {
        let claims = decode_object(self.payload_b64)?;
        validate_claims(&claims, validation)?;
        Ok(claims)
    }
}

/// RFC 7515 §4.1.9: `application/` may be omitted from a media type that has
/// no other `/`.
fn normalized_typ(typ: &str) -> &str {
    match typ.get(..12) {
        Some(prefix) if prefix.eq_ignore_ascii_case("application/") => &typ[12..],
        _ => typ,
    }
}

// ---- claims ----------------------------------------------------------------

/// A `NumericDate` claim (RFC 7519 allows it to be fractional). `Ok(None)` if
/// absent; `Err(Malformed)` if present but not a finite number — never
/// silently ignored.
fn numeric_date(claims: &Value, name: &str) -> Result<Option<f64>, JwtError> {
    let value = match claims.get(name) {
        None => return Ok(None),
        Some(Value::Int(i)) => *i as f64,
        Some(Value::UInt(u)) => *u as f64,
        Some(Value::Float(f)) if f.is_finite() => *f,
        Some(_) => return Err(JwtError::Malformed),
    };
    Ok(Some(value))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn validate_claims(claims: &Value, v: &Validation) -> Result<(), JwtError> {
    let now = v.now.unwrap_or_else(now_unix) as f64;
    let leeway = v.leeway as f64;

    match numeric_date(claims, "exp")? {
        Some(exp) if now >= exp + leeway => return Err(JwtError::Expired),
        Some(_) => {}
        None if v.require_exp => return Err(JwtError::MissingClaim("exp".into())),
        None => {}
    }
    if let Some(nbf) = numeric_date(claims, "nbf")? {
        if now + leeway < nbf {
            return Err(JwtError::NotYetValid);
        }
    }
    if v.validate_iat || v.max_age.is_some() {
        let iat = numeric_date(claims, "iat")?;
        if let (true, Some(iat)) = (v.validate_iat, iat) {
            if iat > now + leeway {
                return Err(JwtError::IssuedInFuture);
            }
        }
        if let Some(max_age) = v.max_age {
            let iat = iat.ok_or_else(|| JwtError::MissingClaim("iat".into()))?;
            if now - iat > max_age as f64 + leeway {
                return Err(JwtError::TooOld);
            }
        }
    }

    for name in &v.required {
        match claims.get(name) {
            None | Some(Value::Null) => return Err(JwtError::MissingClaim(name.clone())),
            Some(_) => {}
        }
    }

    if let Some(required) = &v.required_issuer {
        match claims.get("iss") {
            None => return Err(JwtError::MissingClaim("iss".into())),
            Some(Value::Str(iss)) if iss == required => {}
            Some(_) => return Err(JwtError::InvalidIssuer),
        }
    }
    if let Some(issuers) = &v.issuers {
        match claims.get("iss") {
            None => return Err(JwtError::MissingClaim("iss".into())),
            Some(Value::Str(iss)) if issuers.iter().any(|want| want == iss) => {}
            Some(_) => return Err(JwtError::InvalidIssuer),
        }
    }
    if let Some(subject) = &v.subject {
        match claims.get("sub") {
            None => return Err(JwtError::MissingClaim("sub".into())),
            Some(Value::Str(sub)) if sub == subject => {}
            Some(_) => return Err(JwtError::InvalidSubject),
        }
    }
    if let Some(audiences) = &v.audiences {
        let accepted = |aud: &str| audiences.iter().any(|want| want == aud);
        let ok = match claims.get("aud") {
            None => return Err(JwtError::MissingClaim("aud".into())),
            Some(Value::Str(aud)) => accepted(aud),
            Some(Value::Array(items)) => {
                // Every element must be a string; one of them must match.
                let mut any = false;
                for item in items {
                    match item {
                        Value::Str(aud) => any |= accepted(aud),
                        _ => return Err(JwtError::Malformed),
                    }
                }
                any
            }
            Some(_) => return Err(JwtError::Malformed),
        };
        if !ok {
            return Err(JwtError::InvalidAudience);
        }
    }

    // Last: only a token that passed every other check may consume its jti.
    if let Some(ReplayHandle(cache)) = &v.replay {
        let jti = match claims.get("jti") {
            None | Some(Value::Null) => return Err(JwtError::MissingClaim("jti".into())),
            Some(Value::Str(jti)) => jti,
            Some(_) => return Err(JwtError::Malformed),
        };
        let issuer = match claims.get("iss") {
            Some(Value::Str(iss)) => Some(iss.as_str()),
            _ => None,
        };
        // `expires_at` is the first whole second at which the token is no
        // longer accepted, so the entry outlives every instant it is valid.
        // The `exp` bound is exclusive (`now < exp + leeway`): the first such
        // second is `ceil(exp + leeway)`. The maximum-age bound is inclusive
        // (`now - iat <= max_age + leeway`): it is `floor(..) + 1`.
        let mut until = f64::INFINITY;
        if let Some(exp) = numeric_date(claims, "exp")? {
            until = until.min((exp + leeway).ceil());
        }
        if let (Some(max_age), Some(iat)) = (v.max_age, numeric_date(claims, "iat")?) {
            until = until.min((iat + max_age as f64 + leeway).floor() + 1.0);
        }
        if !until.is_finite() {
            // Nothing bounds the token's lifetime, so its identifier would
            // have to be remembered forever; a bounded cache cannot, and a
            // full one would then protect almost nothing. Refuse it.
            return Err(JwtError::MissingClaim("exp".into()));
        }
        let now_secs = now as u64;
        // Saturating float-to-int conversion; always after `now`, since an
        // accepted token is valid at `now`.
        let expires_at = (until as u64).max(now_secs.saturating_add(1));
        if !cache.check_and_record(issuer, jti, expires_at, now_secs) {
            return Err(JwtError::Replayed);
        }
    }
    Ok(())
}

// ---- HMAC ------------------------------------------------------------------

fn skip_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Whether `secret` is recognisably an asymmetric public key, certificate or
/// JWK document. Using one as an HMAC secret is the classic algorithm-
/// confusion mistake: the "secret" is public, so anyone can forge tokens.
fn looks_like_public_key(secret: &[u8]) -> bool {
    let text = skip_ascii_whitespace(secret);
    if text.starts_with(b"-----BEGIN ") {
        let first_line = text.split(|&b| b == b'\n').next().unwrap_or(text);
        return contains(first_line, b"PUBLIC KEY") || contains(first_line, b"CERTIFICATE");
    }
    if text.starts_with(b"ssh-rsa ")
        || text.starts_with(b"ssh-ed25519 ")
        || text.starts_with(b"ecdsa-sha2-")
    {
        return true;
    }
    text.starts_with(b"{") && contains(text, b"\"kty\"")
}

fn check_secret(secret: &[u8], min_len: usize) -> Result<(), JwtError> {
    if secret.len() < min_len.max(1) {
        return Err(JwtError::WeakSecret);
    }
    if looks_like_public_key(secret) {
        return Err(JwtError::InvalidKey);
    }
    Ok(())
}

fn hmac_tag(alg: Algorithm, secret: &[u8], message: &[u8]) -> Result<Vec<u8>, JwtError> {
    Ok(match alg {
        Algorithm::Hs256 => hmac::<Sha256>(secret, message).as_bytes().to_vec(),
        Algorithm::Hs384 => hmac::<Sha384>(secret, message).as_bytes().to_vec(),
        Algorithm::Hs512 => hmac::<Sha512>(secret, message).as_bytes().to_vec(),
        _ => return Err(JwtError::UnsupportedAlg),
    })
}

fn sign(
    header_json: &str,
    alg: Algorithm,
    claims: &Value,
    secret: &[u8],
) -> Result<String, JwtError> {
    let header = base64::encode_url_no_pad(header_json.as_bytes());
    let payload = base64::encode_url_no_pad(json::to_string(claims).as_bytes());
    let signing_input = format!("{header}.{payload}");
    let tag = hmac_tag(alg, secret, signing_input.as_bytes())?;
    Ok(format!(
        "{signing_input}.{}",
        base64::encode_url_no_pad(&tag)
    ))
}

/// Mints an HS256 JWT for `claims` signed with `secret`.
///
/// This is the original, infallible entry point. Apart from refusing an
/// **empty** secret it performs **no** checks: it signs with a short secret
/// and encodes claims that are not a JSON object (which no verifier here
/// accepts). Because it cannot return an error, an empty secret produces an
/// empty string — not a token, so every verifier rejects it as
/// [`JwtError::Malformed`] — instead of a token anyone could forge.
#[deprecated(
    since = "0.6.0",
    note = "use `encode_hmac(Algorithm::Hs256, ..)`, which enforces the RFC 7518 secret length \
            and reports errors instead of minting a forgeable token"
)]
pub fn encode_hs256(claims: &Value, secret: &[u8]) -> String {
    if secret.is_empty() {
        return String::new();
    }
    sign(
        r#"{"alg":"HS256","typ":"JWT"}"#,
        Algorithm::Hs256,
        claims,
        secret,
    )
    .unwrap_or_default()
}

/// Mints a JWT signed with an HMAC algorithm (`HS256`, `HS384` or `HS512`).
///
/// Fails with [`JwtError::UnsupportedAlg`] for an asymmetric algorithm
/// (signing with RSA, ECDSA or EdDSA is not supported),
/// [`JwtError::WeakSecret`] if the secret is shorter than the algorithm's
/// RFC 7518 minimum ([`Algorithm::min_secret_len`]: 32 / 48 / 64 bytes),
/// [`JwtError::InvalidKey`] if the
/// secret is recognisably a public key, and [`JwtError::Malformed`] if
/// `claims` is not an object or repeats a member name.
///
/// ```
/// use ferroly::auth::{decode_hmac_with, encode_hmac, Algorithm, Validation};
/// use ferroly::codec::Value;
///
/// let secret = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// let claims = Value::Object(vec![
///     ("sub".into(), "u123".into()),
///     ("exp".into(), Value::Int(4_102_444_800)), // 2100-01-01
/// ]);
/// let token = encode_hmac(Algorithm::Hs512, &claims, secret).unwrap();
/// let back = decode_hmac_with(Algorithm::Hs512, &token, secret, &Validation::new()).unwrap();
/// assert_eq!(back.get("sub").and_then(Value::as_str), Some("u123"));
/// ```
pub fn encode_hmac(alg: Algorithm, claims: &Value, secret: &[u8]) -> Result<String, JwtError> {
    encode_with_header(&Header::new(alg), claims, secret)
}

/// Like [`encode_hmac`], with full control of the header (`kid`, `typ`,
/// `cty`). `header.alg` must be an HMAC algorithm.
pub fn encode_with_header(
    header: &Header,
    claims: &Value,
    secret: &[u8],
) -> Result<String, JwtError> {
    if !header.alg.is_hmac() {
        return Err(JwtError::UnsupportedAlg);
    }
    check_secret(secret, Validation::new().secret_floor(header.alg))?;
    let members = claims.as_object().ok_or(JwtError::Malformed)?;
    for (i, (name, _)) in members.iter().enumerate() {
        if members[..i].iter().any(|(other, _)| other == name) {
            return Err(JwtError::Malformed);
        }
    }
    sign(&header.to_json(), header.alg, claims, secret)
}

fn decode_hmac(
    alg: Algorithm,
    token: &str,
    secret: &[u8],
    validation: &Validation,
) -> Result<Value, JwtError> {
    if !alg.is_hmac() {
        return Err(JwtError::UnsupportedAlg);
    }
    check_secret(secret, validation.secret_floor(alg))?;
    let parts = Parts::parse(token)?;
    parts.check_header(alg, validation)?;
    let expected = hmac_tag(alg, secret, parts.signing_input.as_bytes())?;
    if !ct_eq(&expected, &parts.signature) {
        return Err(JwtError::InvalidSignature);
    }
    parts.claims(validation)
}

/// Verifies an HS256 JWT under `secret` and returns its claims.
///
/// This is the original entry point, kept for compatibility. It checks the
/// structure (strict, canonical base64url; header and claims must be JSON
/// objects without duplicate names; no `crit` header), that `alg` is exactly
/// `HS256`, the signature (in constant time), and — **only if present** —
/// `exp` and `nbf` against the current time with no leeway.
///
/// It does **not** require `exp`, and it checks no `aud`, `iss`, `sub` or
/// `iat`. An empty secret, or one that is recognisably a public key, is
/// rejected ([`JwtError::WeakSecret`], [`JwtError::InvalidKey`]); any other
/// length is accepted. Prefer [`decode_hs256_with`], which requires `exp` by
/// default, enforces the RFC 7518 secret length and takes a [`Validation`].
#[deprecated(
    since = "0.6.0",
    note = "use `decode_hs256_with(token, secret, &Validation::new())`, which requires `exp` \
            and the RFC 7518 secret length"
)]
pub fn decode_hs256(token: &str, secret: &[u8]) -> Result<Value, JwtError> {
    decode_hmac(Algorithm::Hs256, token, secret, &Validation::legacy())
}

/// Verifies an HS256 JWT under `secret` and validates its claims against
/// `validation`. The preferred HS256 entry point.
///
/// Checks, in order: the secret (at least 32 bytes, the RFC 7518 minimum for
/// HS256; not a public key), the token structure, the header (`alg` exactly `HS256`, no
/// `crit`, the expected `typ` if configured), the signature in constant
/// time, then the claims (`exp` required unless disabled, `nbf`, `iat`,
/// maximum age, required claims, `iss`, `sub`, `aud`). Claims are not parsed
/// until the signature has verified.
///
/// ```
/// use ferroly::auth::{decode_hs256_with, encode_hmac, Algorithm, JwtError, Validation};
/// use ferroly::codec::Value;
///
/// let secret = b"an-example-secret-of-32-bytes!!!";
/// let claims = Value::Object(vec![
///     ("iss".into(), "https://issuer.example".into()),
///     ("aud".into(), "orders-api".into()),
///     ("exp".into(), Value::Int(4_102_444_800)), // 2100-01-01
/// ]);
/// let token = encode_hmac(Algorithm::Hs256, &claims, secret).unwrap();
///
/// let ok = Validation::new().issuer("https://issuer.example").audience("orders-api");
/// assert!(decode_hs256_with(&token, secret, &ok).is_ok());
///
/// let wrong_audience = Validation::new().audience("billing-api");
/// assert_eq!(
///     decode_hs256_with(&token, secret, &wrong_audience),
///     Err(JwtError::InvalidAudience)
/// );
/// ```
pub fn decode_hs256_with(
    token: &str,
    secret: &[u8],
    validation: &Validation,
) -> Result<Value, JwtError> {
    decode_hmac(Algorithm::Hs256, token, secret, validation)
}

/// Verifies a JWT signed with the given HMAC algorithm (`HS256`, `HS384` or
/// `HS512`) and validates its claims — [`decode_hs256_with`] for the other
/// hash sizes.
///
/// The token's `alg` header must equal `alg` exactly; the header never
/// selects the algorithm. An asymmetric `alg` argument is
/// [`JwtError::UnsupportedAlg`]. The secret must be at least
/// [`Algorithm::min_secret_len`] bytes (32 / 48 / 64).
pub fn decode_hmac_with(
    alg: Algorithm,
    token: &str,
    secret: &[u8],
    validation: &Validation,
) -> Result<Value, JwtError> {
    decode_hmac(alg, token, secret, validation)
}

/// Reads a token's header **without verifying anything**.
///
/// Use it only to choose a verification key (by `kid`) or to route by
/// algorithm; never act on the result otherwise. Fails with
/// [`JwtError::UnsupportedAlg`] for an unknown `alg` (including `none`).
pub fn decode_header(token: &str) -> Result<Header, JwtError> {
    let parts = Parts::parse(token)?;
    Ok(Header {
        alg: parts.algorithm()?,
        typ: parts.typ,
        kid: parts.kid,
        cty: parts.cty,
    })
}
