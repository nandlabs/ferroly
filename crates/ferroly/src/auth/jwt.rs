//! JWT (RFC 7519) / JWS compact (RFC 7515) parsing, signing and validation.

use ferroly::codec::{json, Value};
use ferroly::hash::{base64, ct_eq, hmac, Sha256, Sha384, Sha512};
use ferroly_derive::FerrolyError;

/// The hard minimum HMAC secret length, in bytes, enforced by the validating
/// API ([`encode_hmac`], [`decode_hs256_with`], [`decode_hmac_with`]).
///
/// This is a floor, not a recommendation: RFC 7518 §3.2 requires a key at
/// least as long as the hash output — see
/// [`Algorithm::recommended_secret_len`] (32 bytes for HS256, 48 for HS384, 64
/// for HS512). Generate secrets with
/// [`ferroly::hash::random`](crate::hash::random), never from a passphrase.
pub const MIN_SECRET_LEN: usize = 16;

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
    /// The HMAC secret is empty or shorter than [`MIN_SECRET_LEN`].
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
}

/// A JWS signature algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Algorithm {
    /// HMAC with SHA-256 (symmetric).
    Hs256,
    /// HMAC with SHA-384 (symmetric).
    Hs384,
    /// HMAC with SHA-512 (symmetric).
    Hs512,
    /// RSASSA-PKCS1-v1_5 with SHA-256 (asymmetric; verify-only, via a JWK).
    Rs256,
    /// ECDSA with P-256 and SHA-256 (asymmetric; verify-only, via a JWK).
    Es256,
}

impl Algorithm {
    /// The `alg` header value (`"HS256"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Algorithm::Hs256 => "HS256",
            Algorithm::Hs384 => "HS384",
            Algorithm::Hs512 => "HS512",
            Algorithm::Rs256 => "RS256",
            Algorithm::Es256 => "ES256",
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
            "ES256" => Algorithm::Es256,
            _ => return None,
        })
    }

    /// Whether this is a symmetric (shared-secret) algorithm.
    pub fn is_hmac(self) -> bool {
        matches!(self, Algorithm::Hs256 | Algorithm::Hs384 | Algorithm::Hs512)
    }

    /// The secret length RFC 7518 §3.2 requires for an HMAC algorithm (the
    /// hash output size: 32 / 48 / 64 bytes), or `None` for asymmetric
    /// algorithms.
    pub fn recommended_secret_len(self) -> Option<usize> {
        match self {
            Algorithm::Hs256 => Some(32),
            Algorithm::Hs384 => Some(48),
            Algorithm::Hs512 => Some(64),
            Algorithm::Rs256 | Algorithm::Es256 => None,
        }
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
    min_secret_len: usize,
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
            min_secret_len: MIN_SECRET_LEN,
        }
    }

    /// The behaviour of the original `decode_hs256`: no leeway, `exp` optional,
    /// `iat` ignored, any non-empty secret.
    pub(crate) fn legacy() -> Self {
        Validation {
            leeway: 0,
            require_exp: false,
            validate_iat: false,
            min_secret_len: 1,
            ..Validation::new()
        }
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
        Algorithm::Rs256 | Algorithm::Es256 => return Err(JwtError::UnsupportedAlg),
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
/// This is the original, infallible entry point and performs **no** checks:
/// it will sign with an empty or short secret and will encode claims that are
/// not a JSON object (which no verifier here accepts). Prefer
/// [`encode_hmac`], which rejects both.
pub fn encode_hs256(claims: &Value, secret: &[u8]) -> String {
    sign(
        r#"{"alg":"HS256","typ":"JWT"}"#,
        Algorithm::Hs256,
        claims,
        secret,
    )
    .expect("HS256 is an HMAC algorithm")
}

/// Mints a JWT signed with an HMAC algorithm (`HS256`, `HS384` or `HS512`).
///
/// Fails with [`JwtError::UnsupportedAlg`] for an asymmetric algorithm
/// (signing with RSA/ECDSA is not supported), [`JwtError::WeakSecret`] if the
/// secret is shorter than [`MIN_SECRET_LEN`], [`JwtError::InvalidKey`] if the
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
    check_secret(secret, MIN_SECRET_LEN)?;
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
    check_secret(secret, validation.min_secret_len)?;
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
/// rejected; any other length is accepted. Prefer [`decode_hs256_with`],
/// which requires `exp` by default and takes a [`Validation`].
pub fn decode_hs256(token: &str, secret: &[u8]) -> Result<Value, JwtError> {
    decode_hmac(Algorithm::Hs256, token, secret, &Validation::legacy())
}

/// Verifies an HS256 JWT under `secret` and validates its claims against
/// `validation`. The preferred HS256 entry point.
///
/// Checks, in order: the secret (at least [`MIN_SECRET_LEN`] bytes, not a
/// public key), the token structure, the header (`alg` exactly `HS256`, no
/// `crit`, the expected `typ` if configured), the signature in constant
/// time, then the claims (`exp` required unless disabled, `nbf`, `iat`,
/// maximum age, required claims, `iss`, `sub`, `aud`). Claims are not parsed
/// until the signature has verified.
///
/// ```
/// use ferroly::auth::{decode_hs256_with, encode_hs256, JwtError, Validation};
/// use ferroly::codec::Value;
///
/// let secret = b"an-example-secret-of-32-bytes!!!";
/// let claims = Value::Object(vec![
///     ("iss".into(), "https://issuer.example".into()),
///     ("aud".into(), "orders-api".into()),
///     ("exp".into(), Value::Int(4_102_444_800)), // 2100-01-01
/// ]);
/// let token = encode_hs256(&claims, secret);
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
/// [`JwtError::UnsupportedAlg`].
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
