//! Verify-only asymmetric JWTs (RS256, ES256) from JWK / JWKS public keys.
//!
//! The signature mathematics is **not** implemented here. It is delegated to
//! the verification algorithms of the `rustls` crypto provider (`ring`) that
//! the `http` feature already links for TLS; this module only converts JOSE
//! key and signature encodings into the DER forms those verifiers expect.
//! That is why it needs the `http` feature in addition to `auth`.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rustls_pki_types::{alg_id, AlgorithmIdentifier, SignatureVerificationAlgorithm};

use ferroly::codec::{json, Value};
use ferroly::hash::base64;
use ferroly::http::{Client, Method, Request};

use super::jwt::{Algorithm, JwtError, Parts, Validation};

type Verifier = &'static dyn SignatureVerificationAlgorithm;

/// Looks up a verification algorithm of the rustls `ring` provider by the
/// X.509 algorithm identifiers of the key and of the signature.
fn find_verifier(
    public_key: AlgorithmIdentifier,
    signature: AlgorithmIdentifier,
) -> Option<Verifier> {
    rustls::crypto::ring::default_provider()
        .signature_verification_algorithms
        .all
        .iter()
        .copied()
        .find(|alg| alg.public_key_alg_id() == public_key && alg.signature_alg_id() == signature)
}

/// RSASSA-PKCS1-v1_5 with SHA-256, RSA keys of 2048–8192 bits.
fn rs256_verifier() -> Option<Verifier> {
    static VERIFIER: OnceLock<Option<Verifier>> = OnceLock::new();
    *VERIFIER.get_or_init(|| find_verifier(alg_id::RSA_ENCRYPTION, alg_id::RSA_PKCS1_SHA256))
}

/// ECDSA over P-256 with SHA-256, ASN.1 DER signatures.
fn es256_verifier() -> Option<Verifier> {
    static VERIFIER: OnceLock<Option<Verifier>> = OnceLock::new();
    *VERIFIER.get_or_init(|| find_verifier(alg_id::ECDSA_P256, alg_id::ECDSA_SHA256))
}

// ---- minimal DER encoding --------------------------------------------------

fn der_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let skip = bytes.iter().take_while(|&&b| b == 0).count();
    &bytes[skip..]
}

/// Appends a DER `INTEGER` holding the unsigned big-endian number `digits`:
/// minimal length, with a `0x00` prefix when the top bit is set so it stays
/// non-negative.
fn der_unsigned_integer(digits: &[u8], out: &mut Vec<u8>) {
    let digits = strip_leading_zeros(digits);
    out.push(0x02);
    if digits.is_empty() {
        out.extend_from_slice(&[0x01, 0x00]);
        return;
    }
    let pad = digits[0] & 0x80 != 0;
    der_length(digits.len() + usize::from(pad), out);
    if pad {
        out.push(0x00);
    }
    out.extend_from_slice(digits);
}

/// A DER `SEQUENCE` of two unsigned integers — the shape of both an
/// `RSAPublicKey` (`n`, `e`) and an `ECDSA-Sig-Value` (`r`, `s`).
fn der_integer_pair(first: &[u8], second: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(first.len() + second.len() + 8);
    der_unsigned_integer(first, &mut body);
    der_unsigned_integer(second, &mut body);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(0x30);
    der_length(body.len(), &mut out);
    out.extend_from_slice(&body);
    out
}

// ---- keys ------------------------------------------------------------------

/// The kind of public key a [`Jwk`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyType {
    /// An RSA public key (`"kty": "RSA"`), 2048–8192 bits. Verifies `RS256`.
    Rsa,
    /// An elliptic-curve public key on P-256 (`"kty": "EC"`, `"crv":
    /// "P-256"`). Verifies `ES256`.
    EcP256,
}

#[derive(Clone, PartialEq, Eq)]
enum PublicKey {
    /// DER `RSAPublicKey ::= SEQUENCE { modulus, publicExponent }`.
    Rsa(Vec<u8>),
    /// The uncompressed SEC1 point `0x04 || X || Y`.
    EcP256([u8; 65]),
}

/// A public verification key in JSON Web Key form (RFC 7517).
///
/// A `Jwk` can verify exactly one algorithm, fixed by its key type: an RSA
/// key verifies `RS256`, a P-256 key verifies `ES256`. The token's `alg`
/// header must match; it never chooses the algorithm. There is deliberately no
/// accessor for the raw key bytes and no way to use a `Jwk` with the HMAC
/// functions, so a public key cannot be turned into an `HS256` secret
/// (algorithm confusion).
///
/// ```
/// use ferroly::auth::{Jwk, KeyType};
///
/// let jwk = Jwk::from_json(r#"{
///     "kty": "EC", "crv": "P-256", "kid": "2024-06",
///     "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
///     "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"
/// }"#).unwrap();
/// assert_eq!(jwk.key_type(), KeyType::EcP256);
/// assert_eq!(jwk.kid(), Some("2024-06"));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Jwk {
    kid: Option<String>,
    key: PublicKey,
}

impl std::fmt::Debug for Jwk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jwk")
            .field("kid", &self.kid)
            .field("key_type", &self.key_type())
            .finish()
    }
}

/// Decodes a base64url JWK member (strict, unpadded).
fn b64_member(jwk: &Value, name: &str) -> Result<Vec<u8>, JwtError> {
    match jwk.get(name) {
        Some(Value::Str(text)) => base64::decode_url_no_pad(text).map_err(|_| JwtError::InvalidKey),
        _ => Err(JwtError::InvalidKey),
    }
}

impl Jwk {
    /// Parses a single JWK JSON object.
    pub fn from_json(text: &str) -> Result<Jwk, JwtError> {
        let value = json::from_str(text).map_err(|_| JwtError::InvalidKey)?;
        Jwk::from_value(&value)
    }

    /// Builds a key from an already-parsed JWK object.
    ///
    /// Supported: `"kty": "RSA"` (members `n`, `e`) and `"kty": "EC"` with
    /// `"crv": "P-256"` (members `x`, `y`). Rejected with
    /// [`JwtError::UnsupportedAlg`]: any other key type or curve, and an
    /// `alg` member other than the one the key type implies. Rejected with
    /// [`JwtError::InvalidKey`]: malformed members, a key that carries
    /// private material (`d`), a `use` other than `"sig"`, `key_ops` without
    /// `"verify"`, an RSA modulus outside 2048–8192 bits.
    pub fn from_value(jwk: &Value) -> Result<Jwk, JwtError> {
        if jwk.as_object().is_none() {
            return Err(JwtError::InvalidKey);
        }
        // A verifier has no business holding a private key.
        if jwk.get("d").is_some() {
            return Err(JwtError::InvalidKey);
        }
        match jwk.get("use") {
            None => {}
            Some(Value::Str(usage)) if usage == "sig" => {}
            Some(_) => return Err(JwtError::InvalidKey),
        }
        match jwk.get("key_ops") {
            None => {}
            Some(Value::Array(ops)) if ops.iter().any(|op| op.as_str() == Some("verify")) => {}
            Some(_) => return Err(JwtError::InvalidKey),
        }
        let kid = match jwk.get("kid") {
            None => None,
            Some(Value::Str(kid)) => Some(kid.clone()),
            Some(_) => return Err(JwtError::InvalidKey),
        };

        let key = match jwk.get("kty") {
            Some(Value::Str(kty)) if kty == "RSA" => {
                Jwk::from_rsa_components(&b64_member(jwk, "n")?, &b64_member(jwk, "e")?)?
            }
            Some(Value::Str(kty)) if kty == "EC" => match jwk.get("crv") {
                Some(Value::Str(crv)) if crv == "P-256" => {
                    Jwk::from_ec_p256_coordinates(&b64_member(jwk, "x")?, &b64_member(jwk, "y")?)?
                }
                Some(Value::Str(_)) => return Err(JwtError::UnsupportedAlg),
                _ => return Err(JwtError::InvalidKey),
            },
            Some(Value::Str(_)) => return Err(JwtError::UnsupportedAlg),
            _ => return Err(JwtError::InvalidKey),
        };

        // A declared `alg` must be the one this key type verifies; that rules
        // out e.g. an RSA key published for RSA-OAEP or PS256.
        match jwk.get("alg") {
            None => {}
            Some(Value::Str(name)) if Algorithm::from_name(name) == Some(key.algorithm()) => {}
            Some(Value::Str(_)) => return Err(JwtError::UnsupportedAlg),
            Some(_) => return Err(JwtError::InvalidKey),
        }
        Ok(Jwk { kid, ..key })
    }

    /// Builds an RSA key from the big-endian modulus `n` and public exponent
    /// `e` (leading zero bytes are tolerated). The modulus must be 2048–8192
    /// bits and the exponent an odd number in `3..2^33`.
    pub fn from_rsa_components(n: &[u8], e: &[u8]) -> Result<Jwk, JwtError> {
        let n = strip_leading_zeros(n);
        let e = strip_leading_zeros(e);
        let bits = match n.first() {
            Some(first) => n.len() * 8 - first.leading_zeros() as usize,
            None => 0,
        };
        if !(2048..=8192).contains(&bits) {
            return Err(JwtError::InvalidKey);
        }
        let exponent_ok = match e {
            [] => false,
            [only] => *only >= 3 && only % 2 == 1,
            [first, .., last] => (e.len() < 5 || (e.len() == 5 && *first <= 1)) && last % 2 == 1,
        };
        if !exponent_ok {
            return Err(JwtError::InvalidKey);
        }
        Ok(Jwk {
            kid: None,
            key: PublicKey::Rsa(der_integer_pair(n, e)),
        })
    }

    /// Builds a P-256 key from the affine coordinates `x` and `y`, each
    /// exactly 32 big-endian bytes. Whether the point is on the curve is
    /// checked when a signature is verified.
    pub fn from_ec_p256_coordinates(x: &[u8], y: &[u8]) -> Result<Jwk, JwtError> {
        if x.len() != 32 || y.len() != 32 {
            return Err(JwtError::InvalidKey);
        }
        let mut point = [0u8; 65];
        point[0] = 0x04;
        point[1..33].copy_from_slice(x);
        point[33..].copy_from_slice(y);
        Ok(Jwk {
            kid: None,
            key: PublicKey::EcP256(point),
        })
    }

    /// Sets the key id.
    pub fn with_kid(mut self, kid: impl Into<String>) -> Self {
        self.kid = Some(kid.into());
        self
    }

    /// The key id (`kid`), if the JWK has one.
    pub fn kid(&self) -> Option<&str> {
        self.kid.as_deref()
    }

    /// The key type.
    pub fn key_type(&self) -> KeyType {
        match self.key {
            PublicKey::Rsa(_) => KeyType::Rsa,
            PublicKey::EcP256(_) => KeyType::EcP256,
        }
    }

    /// The one signature algorithm this key verifies: [`Algorithm::Rs256`]
    /// for RSA, [`Algorithm::Es256`] for P-256.
    pub fn algorithm(&self) -> Algorithm {
        match self.key {
            PublicKey::Rsa(_) => Algorithm::Rs256,
            PublicKey::EcP256(_) => Algorithm::Es256,
        }
    }

    /// Verifies a detached signature in JWS format over `message`: the raw
    /// big-endian RSA signature for an RSA key (RSASSA-PKCS1-v1_5, SHA-256),
    /// or the 64-byte `r || s` concatenation for a P-256 key (ECDSA,
    /// SHA-256). The message is hashed internally.
    ///
    /// Fails closed: any problem — wrong key, altered message, malformed
    /// signature, a point that is not on the curve — is
    /// [`JwtError::InvalidSignature`].
    pub fn verify_signature(&self, message: &[u8], signature: &[u8]) -> Result<(), JwtError> {
        match &self.key {
            PublicKey::Rsa(public_key) => rs256_verifier()
                .ok_or(JwtError::UnsupportedAlg)?
                .verify_signature(public_key, message, signature)
                .map_err(|_| JwtError::InvalidSignature),
            PublicKey::EcP256(point) => {
                if signature.len() != 64 {
                    return Err(JwtError::InvalidSignature);
                }
                let der = der_integer_pair(&signature[..32], &signature[32..]);
                es256_verifier()
                    .ok_or(JwtError::UnsupportedAlg)?
                    .verify_signature(point, message, &der)
                    .map_err(|_| JwtError::InvalidSignature)
            }
        }
    }

    /// Verifies `token` with this key and validates its claims.
    ///
    /// The token's `alg` must be the algorithm of this key
    /// ([`algorithm`](Self::algorithm)); anything else — in particular any
    /// `HS*` value — is [`JwtError::UnsupportedAlg`]. The token's `kid` is
    /// not compared (use a [`Jwks`] to select by `kid`).
    pub fn verify(&self, token: &str, validation: &Validation) -> Result<Value, JwtError> {
        self.verify_parts(&Parts::parse(token)?, validation)
    }

    fn verify_parts(&self, parts: &Parts<'_>, validation: &Validation) -> Result<Value, JwtError> {
        parts.check_header(self.algorithm(), validation)?;
        self.verify_signature(parts.signing_input.as_bytes(), &parts.signature)?;
        parts.claims(validation)
    }
}

// ---- key sets --------------------------------------------------------------

/// A set of verification keys (RFC 7517 §5), usually an issuer's published
/// `jwks.json`.
///
/// ```
/// use ferroly::auth::Jwks;
///
/// let jwks = Jwks::from_json(r#"{"keys":[
///     {"kty":"EC","crv":"P-256","kid":"a",
///      "x":"f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
///      "y":"x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"},
///     {"kty":"OKP","crv":"Ed25519","kid":"b","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}
/// ]}"#).unwrap();
/// assert_eq!(jwks.len(), 1);      // the P-256 key
/// assert_eq!(jwks.skipped(), 1);  // Ed25519 is not supported
/// assert!(jwks.find("a").is_some());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Jwks {
    keys: Vec<Jwk>,
    skipped: usize,
}

impl Jwks {
    /// Parses a JWK Set document (`{"keys": [...]}`).
    ///
    /// Entries that are unsupported or invalid (other key types and curves,
    /// encryption keys, private keys, undersized RSA keys) are skipped, as
    /// RFC 7517 §5 recommends, and counted in [`skipped`](Self::skipped).
    /// Only a document that is not a JWK Set at all is an error
    /// ([`JwtError::InvalidKey`]).
    pub fn from_json(text: &str) -> Result<Jwks, JwtError> {
        let value = json::from_str(text).map_err(|_| JwtError::InvalidKey)?;
        let entries = match value.get("keys") {
            Some(Value::Array(entries)) => entries,
            _ => return Err(JwtError::InvalidKey),
        };
        let mut jwks = Jwks::default();
        for entry in entries {
            match Jwk::from_value(entry) {
                Ok(key) => jwks.keys.push(key),
                Err(_) => jwks.skipped += 1,
            }
        }
        Ok(jwks)
    }

    /// A key set from keys built in code.
    pub fn from_keys(keys: Vec<Jwk>) -> Jwks {
        Jwks { keys, skipped: 0 }
    }

    /// The usable keys.
    pub fn keys(&self) -> &[Jwk] {
        &self.keys
    }

    /// The number of usable keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set has no usable key.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// How many entries of the source document were skipped as unsupported or
    /// invalid.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// The first key with the given `kid`.
    pub fn find(&self, kid: &str) -> Option<&Jwk> {
        self.keys.iter().find(|key| key.kid() == Some(kid))
    }

    /// Verifies `token` against the set and validates its claims.
    ///
    /// Key selection: a key is a candidate if its type matches the token's
    /// `alg` and — when the token has a `kid` — its `kid` is equal. A token
    /// without `kid` is tried against every key of the right type. No
    /// candidate is [`JwtError::UnknownKey`]; candidates that do not verify
    /// are [`JwtError::InvalidSignature`]. A token whose `alg` is an HMAC
    /// algorithm is always [`JwtError::UnsupportedAlg`].
    pub fn verify(&self, token: &str, validation: &Validation) -> Result<Value, JwtError> {
        let parts = Parts::parse(token)?;
        let alg = parts.algorithm()?;
        if alg.is_hmac() {
            return Err(JwtError::UnsupportedAlg);
        }
        let mut tried_any = false;
        for key in &self.keys {
            let kid_matches = match (&parts.kid, key.kid()) {
                (Some(want), Some(have)) => want == have,
                (Some(_), None) => false,
                (None, _) => true,
            };
            if key.algorithm() != alg || !kid_matches {
                continue;
            }
            tried_any = true;
            match key.verify_parts(&parts, validation) {
                Err(JwtError::InvalidSignature) => continue,
                outcome => return outcome,
            }
        }
        Err(if tried_any {
            JwtError::InvalidSignature
        } else {
            JwtError::UnknownKey
        })
    }
}

// ---- remote key sets -------------------------------------------------------

struct RemoteState {
    jwks: Option<Arc<Jwks>>,
    fetched_at: Option<Instant>,
    last_attempt: Option<Instant>,
}

/// A JWKS fetched over HTTP and cached, for verifying tokens from an issuer
/// that rotates its keys.
///
/// - The set is fetched on first use and re-fetched once it is older than
///   the [refresh interval](Self::refresh_interval) (default 1 hour).
/// - A token whose `kid` is not in the cached set triggers one immediate
///   re-fetch (the issuer may have rotated), but never more often than the
///   [minimum refresh interval](Self::min_refresh_interval) (default 30 s),
///   so unknown `kid`s cannot be used to make this service hammer the issuer.
/// - Concurrent callers share one fetch.
/// - If a refresh fails while a previous set is cached, the cached set keeps
///   being served (and the fetch is retried after the minimum refresh
///   interval). Only a failed *first* fetch is an error.
///
/// Use an `https://` URL: the key set is the root of trust for every token.
///
/// ```no_run
/// use ferroly::auth::{RemoteJwks, Validation};
///
/// # async fn example(token: &str) -> Result<(), ferroly::auth::JwtError> {
/// let keys = RemoteJwks::new("https://issuer.example/.well-known/jwks.json");
/// let validation = Validation::new()
///     .issuer("https://issuer.example")
///     .audience("orders-api");
/// let claims = keys.verify(token, &validation).await?;
/// # let _ = claims;
/// # Ok(())
/// # }
/// ```
pub struct RemoteJwks {
    url: String,
    client: Client,
    refresh_interval: Duration,
    min_refresh_interval: Duration,
    max_bytes: usize,
    state: tokio::sync::Mutex<RemoteState>,
}

impl std::fmt::Debug for RemoteJwks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteJwks")
            .field("url", &self.url)
            .field("refresh_interval", &self.refresh_interval)
            .field("min_refresh_interval", &self.min_refresh_interval)
            .finish_non_exhaustive()
    }
}

impl RemoteJwks {
    /// A cache for the JWKS at `url`, using a client with a 10-second
    /// request timeout.
    pub fn new(url: impl Into<String>) -> Self {
        RemoteJwks {
            url: url.into(),
            client: Client::new().with_timeout(Some(Duration::from_secs(10))),
            refresh_interval: Duration::from_secs(3600),
            min_refresh_interval: Duration::from_secs(30),
            max_bytes: 1024 * 1024,
            state: tokio::sync::Mutex::new(RemoteState {
                jwks: None,
                fetched_at: None,
                last_attempt: None,
            }),
        }
    }

    /// Uses `client` for the fetches (custom timeout, shared pool).
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// How long a fetched set is used before it is re-fetched. Default 1 hour.
    pub fn refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// The minimum spacing between fetch attempts triggered by an unknown
    /// `kid` or by a failing endpoint. Default 30 seconds.
    pub fn min_refresh_interval(mut self, interval: Duration) -> Self {
        self.min_refresh_interval = interval;
        self
    }

    /// The largest response body accepted, in bytes. Default 1 MiB.
    pub fn max_response_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The current key set, fetching or refreshing it if needed.
    pub async fn get(&self) -> Result<Arc<Jwks>, JwtError> {
        let mut state = self.state.lock().await;
        if let (Some(jwks), Some(fetched_at)) = (&state.jwks, state.fetched_at) {
            let fresh = fetched_at.elapsed() < self.refresh_interval;
            let backing_off = state
                .last_attempt
                .is_some_and(|at| at.elapsed() < self.min_refresh_interval);
            if fresh || backing_off {
                return Ok(jwks.clone());
            }
        }
        match self.fetch_into(&mut state).await {
            Ok(jwks) => Ok(jwks),
            // Keep serving the previous set while the endpoint is unavailable.
            Err(error) => state.jwks.clone().ok_or(error),
        }
    }

    /// Fetches the key set now, regardless of its age.
    pub async fn refresh(&self) -> Result<Arc<Jwks>, JwtError> {
        let mut state = self.state.lock().await;
        self.fetch_into(&mut state).await
    }

    /// Verifies `token` against the (cached) key set and validates its
    /// claims; see [`Jwks::verify`]. On an unknown `kid` the set is
    /// re-fetched once, rate-limited by the minimum refresh interval.
    pub async fn verify(&self, token: &str, validation: &Validation) -> Result<Value, JwtError> {
        let jwks = self.get().await?;
        match jwks.verify(token, validation) {
            Err(JwtError::UnknownKey) => {}
            outcome => return outcome,
        }
        let refreshed = {
            let mut state = self.state.lock().await;
            let same_set = state
                .jwks
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &jwks));
            let recently = state
                .last_attempt
                .is_some_and(|at| at.elapsed() < self.min_refresh_interval);
            if !same_set {
                // Another caller refreshed while we were verifying.
                state.jwks.clone()
            } else if recently {
                None
            } else {
                self.fetch_into(&mut state).await.ok()
            }
        };
        match refreshed {
            Some(jwks) => jwks.verify(token, validation),
            None => Err(JwtError::UnknownKey),
        }
    }

    async fn fetch_into(&self, state: &mut RemoteState) -> Result<Arc<Jwks>, JwtError> {
        state.last_attempt = Some(Instant::now());
        let jwks = Arc::new(self.fetch().await?);
        state.jwks = Some(jwks.clone());
        state.fetched_at = Some(Instant::now());
        Ok(jwks)
    }

    async fn fetch(&self) -> Result<Jwks, JwtError> {
        let fail = |reason: String| JwtError::KeyFetch(reason);
        let request = Request::builder(Method::Get, &self.url)
            .map_err(|e| fail(e.to_string()))?
            .header("accept", "application/json")
            .build();
        let mut response = self
            .client
            .send(request)
            .await
            .map_err(|e| fail(e.to_string()))?;
        if !response.is_success() {
            return Err(fail(format!(
                "unexpected status {}",
                response.status().as_u16()
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| fail(e.to_string()))? {
            if body.len() + chunk.len() > self.max_bytes {
                return Err(fail("response too large".to_string()));
            }
            body.extend_from_slice(&chunk);
        }
        let text =
            String::from_utf8(body).map_err(|_| fail("response is not UTF-8".to_string()))?;
        let jwks =
            Jwks::from_json(&text).map_err(|_| fail("response is not a JWK Set".to_string()))?;
        if jwks.is_empty() {
            // Never replace a working set with one we cannot use.
            return Err(fail("key set contains no supported key".to_string()));
        }
        Ok(jwks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths_use_the_minimal_form() {
        let mut out = Vec::new();
        der_length(0, &mut out);
        der_length(0x7f, &mut out);
        der_length(0x80, &mut out);
        der_length(0xff, &mut out);
        der_length(0x100, &mut out);
        der_length(0xffff, &mut out);
        assert_eq!(
            out,
            [0x00, 0x7f, 0x81, 0x80, 0x81, 0xff, 0x82, 0x01, 0x00, 0x82, 0xff, 0xff]
        );
    }

    #[test]
    fn der_integers_are_minimal_and_non_negative() {
        let encode = |digits: &[u8]| {
            let mut out = Vec::new();
            der_unsigned_integer(digits, &mut out);
            out
        };
        assert_eq!(encode(&[]), [0x02, 0x01, 0x00]);
        assert_eq!(encode(&[0x00, 0x00]), [0x02, 0x01, 0x00]);
        assert_eq!(encode(&[0x01]), [0x02, 0x01, 0x01]);
        assert_eq!(encode(&[0x7f]), [0x02, 0x01, 0x7f]);
        // Top bit set: a zero byte keeps the integer positive.
        assert_eq!(encode(&[0x80]), [0x02, 0x02, 0x00, 0x80]);
        assert_eq!(encode(&[0x00, 0x80]), [0x02, 0x02, 0x00, 0x80]);
        // Redundant leading zeros are dropped.
        assert_eq!(encode(&[0x00, 0x00, 0x7f, 0x01]), [0x02, 0x02, 0x7f, 0x01]);
        assert_eq!(encode(&[0x01, 0x00, 0x01]), [0x02, 0x03, 0x01, 0x00, 0x01]);
    }

    #[test]
    fn der_integer_pair_wraps_in_a_sequence() {
        assert_eq!(
            der_integer_pair(&[0x81], &[0x01, 0x00, 0x01]),
            [0x30, 0x09, 0x02, 0x02, 0x00, 0x81, 0x02, 0x03, 0x01, 0x00, 0x01]
        );
        // A 256-byte modulus with the top bit set needs the long length form.
        let n = [0xffu8; 256];
        let der = der_integer_pair(&n, &[0x01, 0x00, 0x01]);
        assert_eq!(&der[..4], [0x30, 0x82, 0x01, 0x0a]);
        assert_eq!(&der[4..9], [0x02, 0x82, 0x01, 0x01, 0x00]);
        assert_eq!(der.len(), 4 + 0x010a);
    }

    #[test]
    fn the_provider_exposes_both_verifiers() {
        assert!(rs256_verifier().is_some());
        assert!(es256_verifier().is_some());
    }
}
