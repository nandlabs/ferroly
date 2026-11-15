//! Verify-only asymmetric JWTs from JWK / JWKS public keys: RS256/384/512,
//! PS256/384/512, ES256/384/512 and EdDSA (Ed25519), plus OIDC discovery.
//!
//! The RSA and ECDSA mathematics is **not** implemented here. It is delegated
//! to the signature-verification algorithms of the `rustls` crypto provider
//! that the `http` feature already uses for TLS — the process-default
//! provider if the application installed one, else `ring` with the
//! `tls-ring` feature (on by default); this module only converts JOSE key and
//! signature encodings into the DER forms those verifiers expect. That is why
//! it needs the `http` feature in addition to `auth`. EdDSA is verified by
//! [`ferroly::sig`](crate::sig) and needs no provider.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls_pki_types::{alg_id, AlgorithmIdentifier, SignatureVerificationAlgorithm};

use ferroly::codec::{json, Value};
use ferroly::hash::base64;
use ferroly::http::transport::crypto_provider;
use ferroly::http::{Client, Method, Request};
use ferroly::sig::{Ed25519PublicKey, Ed25519Signature};

use super::jwt::{Algorithm, JwtError, Parts, Validation};

type Verifier = &'static dyn SignatureVerificationAlgorithm;

/// Looks up a verification algorithm of the rustls crypto provider — the
/// same one TLS uses: the process default if the application installed one,
/// else `ring` with the `tls-ring` feature — by the X.509 algorithm
/// identifiers of the key and of the signature.
///
/// The lookup runs on every verification (a scan of the provider's dozen or
/// so algorithms), never cached, so a provider installed after the first
/// verification is used from then on.
///
/// No provider at all is [`JwtError::NoCryptoProvider`]; a provider without
/// the algorithm is [`JwtError::UnsupportedAlg`].
fn find_verifier(
    public_key: AlgorithmIdentifier,
    signature: AlgorithmIdentifier,
) -> Result<Verifier, JwtError> {
    let provider = crypto_provider().ok_or(JwtError::NoCryptoProvider)?;
    provider
        .signature_verification_algorithms
        .all
        .iter()
        .copied()
        .find(|alg| alg.public_key_alg_id() == public_key && alg.signature_alg_id() == signature)
        .ok_or(JwtError::UnsupportedAlg)
}

// The verifier is looked up on every verification, never cached: a provider
// the application installs later (a FIPS one, say) must take over from the
// `ring` fallback at once. The lookup is a scan of about a dozen entries,
// negligible next to the signature check itself.

/// The provider algorithm identifiers (key, signature) of a JOSE algorithm
/// verified through the provider.
fn provider_ids(alg: Algorithm) -> Option<(AlgorithmIdentifier, AlgorithmIdentifier)> {
    Some(match alg {
        Algorithm::Rs256 => (alg_id::RSA_ENCRYPTION, alg_id::RSA_PKCS1_SHA256),
        Algorithm::Rs384 => (alg_id::RSA_ENCRYPTION, alg_id::RSA_PKCS1_SHA384),
        Algorithm::Rs512 => (alg_id::RSA_ENCRYPTION, alg_id::RSA_PKCS1_SHA512),
        // RSASSA-PSS with MGF1 over the same hash and a salt as long as the
        // hash: exactly what RFC 7518 §3.5 requires of PS256/384/512.
        Algorithm::Ps256 => (alg_id::RSA_ENCRYPTION, alg_id::RSA_PSS_SHA256),
        Algorithm::Ps384 => (alg_id::RSA_ENCRYPTION, alg_id::RSA_PSS_SHA384),
        Algorithm::Ps512 => (alg_id::RSA_ENCRYPTION, alg_id::RSA_PSS_SHA512),
        Algorithm::Es256 => (alg_id::ECDSA_P256, alg_id::ECDSA_SHA256),
        Algorithm::Es384 => (alg_id::ECDSA_P384, alg_id::ECDSA_SHA384),
        Algorithm::Es512 => (alg_id::ECDSA_P521, alg_id::ECDSA_SHA512),
        _ => return None,
    })
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
    /// An RSA public key (`"kty": "RSA"`), 2048–8192 bits. Verifies `RS256`,
    /// `RS384`, `RS512`, `PS256`, `PS384` and `PS512`.
    Rsa,
    /// An elliptic-curve public key on P-256 (`"kty": "EC"`, `"crv":
    /// "P-256"`). Verifies `ES256`.
    EcP256,
    /// An elliptic-curve public key on P-384 (`"crv": "P-384"`). Verifies
    /// `ES384`.
    EcP384,
    /// An elliptic-curve public key on P-521 (`"crv": "P-521"`). Verifies
    /// `ES512` with a provider that supports it (`ring` does not).
    EcP521,
    /// An Ed25519 public key (`"kty": "OKP"`, `"crv": "Ed25519"`, RFC 8037).
    /// Verifies `EdDSA`.
    Ed25519,
}

impl KeyType {
    /// The algorithms a key of this type can verify; the first is its
    /// default.
    fn algorithms(self) -> &'static [Algorithm] {
        match self {
            KeyType::Rsa => &[
                Algorithm::Rs256,
                Algorithm::Rs384,
                Algorithm::Rs512,
                Algorithm::Ps256,
                Algorithm::Ps384,
                Algorithm::Ps512,
            ],
            KeyType::EcP256 => &[Algorithm::Es256],
            KeyType::EcP384 => &[Algorithm::Es384],
            KeyType::EcP521 => &[Algorithm::Es512],
            KeyType::Ed25519 => &[Algorithm::EdDsa],
        }
    }

    /// The byte length of one EC coordinate (and of `r` and `s`).
    fn ec_coordinate_len(self) -> Option<usize> {
        match self {
            KeyType::EcP256 => Some(32),
            KeyType::EcP384 => Some(48),
            KeyType::EcP521 => Some(66),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum PublicKey {
    /// DER `RSAPublicKey ::= SEQUENCE { modulus, publicExponent }`.
    Rsa(Vec<u8>),
    /// The uncompressed SEC1 point `0x04 || X || Y` on `curve`.
    Ec { curve: KeyType, point: Vec<u8> },
    /// A strictly decoded Ed25519 point.
    Ed25519(Ed25519PublicKey),
}

/// A public verification key in JSON Web Key form (RFC 7517, RFC 8037).
///
/// The key type fixes the family of algorithms a `Jwk` can verify: an RSA
/// key verifies `RS*` and `PS*`, an EC key the one `ES*` of its curve, an
/// Ed25519 key `EdDSA`. A JWK that declares an `alg` member (or a key pinned
/// with [`with_algorithm`](Self::with_algorithm)) verifies **only** that
/// algorithm. The token's `alg` header must be one the key accepts; it never
/// chooses the key type. There is deliberately no accessor for the raw key
/// bytes and no way to use a `Jwk` with the HMAC functions, so a public key
/// cannot be turned into an `HS256` secret (algorithm confusion).
///
/// ```
/// use ferroly::auth::{Algorithm, Jwk, KeyType};
///
/// let jwk = Jwk::from_json(r#"{
///     "kty": "EC", "crv": "P-256", "kid": "2024-06",
///     "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
///     "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"
/// }"#).unwrap();
/// assert_eq!(jwk.key_type(), KeyType::EcP256);
/// assert_eq!(jwk.kid(), Some("2024-06"));
///
/// let ed = Jwk::from_json(r#"{"kty":"OKP","crv":"Ed25519",
///     "x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}"#).unwrap();
/// assert_eq!(ed.algorithms(), &[Algorithm::EdDsa]);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Jwk {
    kid: Option<String>,
    key: PublicKey,
    /// The one algorithm this key may verify, when the JWK declared it.
    pinned: Option<Algorithm>,
}

impl std::fmt::Debug for Jwk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jwk")
            .field("kid", &self.kid)
            .field("key_type", &self.key_type())
            .field("algorithms", &self.algorithms())
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
    fn unpinned(key: PublicKey) -> Jwk {
        Jwk {
            kid: None,
            key,
            pinned: None,
        }
    }

    /// Parses a single JWK JSON object.
    pub fn from_json(text: &str) -> Result<Jwk, JwtError> {
        let value = json::from_str(text).map_err(|_| JwtError::InvalidKey)?;
        Jwk::from_value(&value)
    }

    /// Builds a key from an already-parsed JWK object.
    ///
    /// Supported: `"kty": "RSA"` (members `n`, `e`), `"kty": "EC"` with
    /// `"crv"` `"P-256"`, `"P-384"` or `"P-521"` (members `x`, `y`), and
    /// `"kty": "OKP"` with `"crv": "Ed25519"` (member `x`). Rejected with
    /// [`JwtError::UnsupportedAlg`]: any other key type or curve (`oct`,
    /// `Ed448`, `X25519`, …), and an `alg` member the key type cannot verify.
    /// Rejected with [`JwtError::InvalidKey`]: malformed members, a key that
    /// carries private material (`d`), a `use` other than `"sig"`, `key_ops`
    /// without `"verify"`, an RSA modulus outside 2048–8192 bits, coordinates
    /// of the wrong length, an Ed25519 key that is not a canonical,
    /// non-small-order point.
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

        let crv = || match jwk.get("crv") {
            Some(Value::Str(crv)) => Ok(crv.as_str()),
            _ => Err(JwtError::InvalidKey),
        };
        let key = match jwk.get("kty") {
            Some(Value::Str(kty)) if kty == "RSA" => {
                Jwk::from_rsa_components(&b64_member(jwk, "n")?, &b64_member(jwk, "e")?)?
            }
            Some(Value::Str(kty)) if kty == "EC" => {
                let curve = match crv()? {
                    "P-256" => KeyType::EcP256,
                    "P-384" => KeyType::EcP384,
                    "P-521" => KeyType::EcP521,
                    _ => return Err(JwtError::UnsupportedAlg),
                };
                Jwk::from_ec_coordinates(curve, &b64_member(jwk, "x")?, &b64_member(jwk, "y")?)?
            }
            Some(Value::Str(kty)) if kty == "OKP" => match crv()? {
                "Ed25519" => Jwk::from_ed25519_bytes(&b64_member(jwk, "x")?)?,
                _ => return Err(JwtError::UnsupportedAlg),
            },
            Some(Value::Str(_)) => return Err(JwtError::UnsupportedAlg),
            _ => return Err(JwtError::InvalidKey),
        };

        // A declared `alg` pins the key to that one algorithm, which must be
        // one its type verifies; that rules out e.g. an RSA key published for
        // RSA-OAEP, or an EC key labelled RS256.
        let key = match jwk.get("alg") {
            None => key,
            Some(Value::Str(name)) => match Algorithm::from_name(name) {
                Some(alg) => key.with_algorithm(alg)?,
                None => return Err(JwtError::UnsupportedAlg),
            },
            Some(_) => return Err(JwtError::InvalidKey),
        };
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
        Ok(Jwk::unpinned(PublicKey::Rsa(der_integer_pair(n, e))))
    }

    /// Builds a P-256 key from the affine coordinates `x` and `y`, each
    /// exactly 32 big-endian bytes. Whether the point is on the curve is
    /// checked when a signature is verified.
    pub fn from_ec_p256_coordinates(x: &[u8], y: &[u8]) -> Result<Jwk, JwtError> {
        Jwk::from_ec_coordinates(KeyType::EcP256, x, y)
    }

    /// Builds an EC key on `curve` ([`KeyType::EcP256`], [`KeyType::EcP384`]
    /// or [`KeyType::EcP521`]) from the big-endian affine coordinates `x` and
    /// `y`, each exactly 32, 48 or 66 bytes. Whether the point is on the curve
    /// is checked when a signature is verified. Another key type is
    /// [`JwtError::UnsupportedAlg`].
    pub fn from_ec_coordinates(curve: KeyType, x: &[u8], y: &[u8]) -> Result<Jwk, JwtError> {
        let len = curve.ec_coordinate_len().ok_or(JwtError::UnsupportedAlg)?;
        if x.len() != len || y.len() != len {
            return Err(JwtError::InvalidKey);
        }
        let mut point = Vec::with_capacity(1 + 2 * len);
        point.push(0x04);
        point.extend_from_slice(x);
        point.extend_from_slice(y);
        Ok(Jwk::unpinned(PublicKey::Ec { curve, point }))
    }

    /// Builds an Ed25519 key (RFC 8037) from its 32-byte encoding, strictly
    /// decoded by [`ferroly::sig`](crate::sig): a non-canonical or
    /// small-order point is [`JwtError::InvalidKey`].
    pub fn from_ed25519_bytes(x: &[u8]) -> Result<Jwk, JwtError> {
        let key = Ed25519PublicKey::from_slice(x).map_err(|_| JwtError::InvalidKey)?;
        Ok(Jwk::from(key))
    }

    /// Pins the key to `alg`: from then on it verifies only that algorithm.
    /// `alg` must be one the key type verifies ([`JwtError::UnsupportedAlg`]
    /// otherwise). This is what a JWK's `alg` member does.
    pub fn with_algorithm(mut self, alg: Algorithm) -> Result<Self, JwtError> {
        if !self.key_type().algorithms().contains(&alg) {
            return Err(JwtError::UnsupportedAlg);
        }
        self.pinned = Some(alg);
        Ok(self)
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
        match &self.key {
            PublicKey::Rsa(_) => KeyType::Rsa,
            PublicKey::Ec { curve, .. } => *curve,
            PublicKey::Ed25519(_) => KeyType::Ed25519,
        }
    }

    /// The algorithm this key verifies by default — the pinned one if the
    /// JWK declared an `alg`, else [`Algorithm::Rs256`] for RSA,
    /// [`Algorithm::Es256`] / [`Es384`](Algorithm::Es384) /
    /// [`Es512`](Algorithm::Es512) for the curve, [`Algorithm::EdDsa`] for
    /// Ed25519. [`verify_signature`](Self::verify_signature) uses it.
    pub fn algorithm(&self) -> Algorithm {
        self.algorithms()[0]
    }

    /// Every algorithm this key accepts in a token's `alg` header: the pinned
    /// one alone, or all of its key type's (`RS*` and `PS*` for RSA).
    pub fn algorithms(&self) -> &[Algorithm] {
        match &self.pinned {
            Some(alg) => std::slice::from_ref(alg),
            None => self.key_type().algorithms(),
        }
    }

    /// Whether a token signed with `alg` may be verified with this key.
    pub fn accepts(&self, alg: Algorithm) -> bool {
        self.algorithms().contains(&alg)
    }

    /// Verifies a detached signature in JWS format over `message` with the
    /// key's default [`algorithm`](Self::algorithm); see
    /// [`verify_signature_with`](Self::verify_signature_with).
    pub fn verify_signature(&self, message: &[u8], signature: &[u8]) -> Result<(), JwtError> {
        self.verify_signature_with(self.algorithm(), message, signature)
    }

    /// Verifies a detached signature in JWS format over `message` with `alg`:
    /// the raw big-endian RSA signature for `RS*` / `PS*`, the `r || s`
    /// concatenation (64, 96 or 132 bytes) for `ES*`, the 64-byte signature
    /// for `EdDSA`. The message is hashed internally.
    ///
    /// Fails closed: any problem — wrong key, altered message, malformed
    /// signature, a point that is not on the curve — is
    /// [`JwtError::InvalidSignature`]. An `alg` the key does not
    /// [`accept`](Self::accepts), or one the crypto provider lacks, is
    /// [`JwtError::UnsupportedAlg`]. Without a rustls crypto provider (the
    /// `tls-ring` feature is off and the application installed none) `RS*` /
    /// `PS*` / `ES*` are [`JwtError::NoCryptoProvider`]; `EdDSA` needs none.
    pub fn verify_signature_with(
        &self,
        alg: Algorithm,
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), JwtError> {
        if !self.accepts(alg) {
            return Err(JwtError::UnsupportedAlg);
        }
        let der;
        let (public_key, signature): (&[u8], &[u8]) = match &self.key {
            PublicKey::Ed25519(key) => {
                let signature = Ed25519Signature::from_slice(signature)
                    .map_err(|_| JwtError::InvalidSignature)?;
                return key
                    .verify(message, &signature)
                    .map_err(|_| JwtError::InvalidSignature);
            }
            PublicKey::Rsa(key) => (key, signature),
            PublicKey::Ec { curve, point } => {
                let len = curve.ec_coordinate_len().unwrap_or(0);
                if signature.len() != 2 * len {
                    return Err(JwtError::InvalidSignature);
                }
                der = der_integer_pair(&signature[..len], &signature[len..]);
                (point.as_slice(), der.as_slice())
            }
        };
        let (key_id, signature_id) = provider_ids(alg).ok_or(JwtError::UnsupportedAlg)?;
        find_verifier(key_id, signature_id)?
            .verify_signature(public_key, message, signature)
            .map_err(|_| JwtError::InvalidSignature)
    }

    /// Verifies `token` with this key and validates its claims.
    ///
    /// The token's `alg` must be one this key [`accepts`](Self::accepts);
    /// anything else — in particular any `HS*` value — is
    /// [`JwtError::UnsupportedAlg`]. The token's `kid` is not compared (use a
    /// [`Jwks`] to select by `kid`).
    pub fn verify(&self, token: &str, validation: &Validation) -> Result<Value, JwtError> {
        self.verify_parts(&Parts::parse(token)?, validation)
    }

    fn verify_parts(&self, parts: &Parts<'_>, validation: &Validation) -> Result<Value, JwtError> {
        let alg = parts.algorithm()?;
        if !self.accepts(alg) {
            return Err(JwtError::UnsupportedAlg);
        }
        parts.check_header(alg, validation)?;
        self.verify_signature_with(alg, parts.signing_input.as_bytes(), &parts.signature)?;
        parts.claims(validation)
    }
}

impl From<Ed25519PublicKey> for Jwk {
    /// An Ed25519 key (verifies `EdDSA`), without a `kid`.
    fn from(key: Ed25519PublicKey) -> Jwk {
        Jwk::unpinned(PublicKey::Ed25519(key))
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
///     {"kty":"OKP","crv":"Ed25519","kid":"b","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"},
///     {"kty":"OKP","crv":"X25519","kid":"c","x":"hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo"}
/// ]}"#).unwrap();
/// assert_eq!(jwks.len(), 2);      // the P-256 and Ed25519 keys
/// assert_eq!(jwks.skipped(), 1);  // X25519 is a key-agreement key
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
    /// Key selection: a key is a candidate if it [accepts](Jwk::accepts) the
    /// token's `alg` and — when the token has a `kid` — its `kid` is equal. A
    /// token without `kid` is tried against every accepting key. No
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
            if !key.accepts(alg) || !kid_matches {
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
    /// Why the last attempt failed, served while backing off.
    last_error: Option<JwtError>,
    /// The key-set URL: given, or found by OIDC discovery.
    jwks_uri: Option<String>,
}

/// What `RemoteJwks::lookup` found in the cache.
enum Lookup {
    /// A set to use as it is.
    Serve(Arc<Jwks>),
    /// No usable set, and the last attempt failed too recently to retry.
    Fail(JwtError),
    /// Time to fetch; `stale` is a set that may be used meanwhile.
    Fetch { stale: Option<Arc<Jwks>> },
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
/// - Concurrent callers share one fetch, and a fetch never blocks the
///   callers that can use the cached set: while a refresh is in flight they
///   verify against the set they have. Only a caller with no usable set, or
///   one whose token names a `kid` the set lacks, waits for the fetch.
/// - If a refresh fails while a previous set is cached, the cached set keeps
///   being served (and the fetch is retried after the minimum refresh
///   interval) — but for at most [`max_stale`](Self::max_stale) past its
///   refresh interval (default 4 hours), after which verification fails
///   with the fetch error, so blocking the key-set endpoint cannot keep a
///   rotated-out key trusted indefinitely. A failed *first* fetch is an
///   error, and is retried at most once per minimum refresh interval too
///   (callers in between get the same error without a request).
///
/// Build it from the key-set URL ([`new`](Self::new)) or from the issuer
/// with OpenID Connect discovery ([`from_issuer`](Self::from_issuer)). Both
/// must be `https://` URLs — the key set is the root of trust for every
/// token — unless [`allow_insecure_http(true)`](Self::allow_insecure_http)
/// is set (a local test identity provider); otherwise every fetch fails
/// with [`JwtError::KeyFetch`].
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
    /// The issuer, for a set found by discovery.
    issuer: Option<String>,
    client: Client,
    refresh_interval: Duration,
    min_refresh_interval: Duration,
    max_stale: Duration,
    max_bytes: usize,
    allow_insecure_http: bool,
    /// The cache. Held only for short, synchronous sections, never across a
    /// request.
    state: std::sync::Mutex<RemoteState>,
    /// Single-flight guard for fetches (discovery and key set).
    fetching: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for RemoteJwks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let jwks_uri = self
            .state
            .try_lock()
            .ok()
            .and_then(|state| state.jwks_uri.clone());
        f.debug_struct("RemoteJwks")
            .field("issuer", &self.issuer)
            .field("jwks_uri", &jwks_uri)
            .field("refresh_interval", &self.refresh_interval)
            .field("min_refresh_interval", &self.min_refresh_interval)
            .field("max_stale", &self.max_stale)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish_non_exhaustive()
    }
}

/// `{issuer}/.well-known/openid-configuration` (OpenID Connect Discovery 1.0
/// §4: any trailing `/` of the issuer is dropped first).
fn discovery_url(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

/// Whether `url` has the `https` scheme (case-insensitive).
fn is_https(url: &str) -> bool {
    url.get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

/// Checks OpenID provider metadata against the `issuer` it was fetched for
/// and returns its `jwks_uri`.
fn jwks_uri_from_metadata(issuer: &str, text: &str) -> Result<String, JwtError> {
    let fail = |reason: &str| JwtError::KeyFetch(format!("OIDC discovery: {reason}"));
    let metadata = json::from_str(text).map_err(|_| fail("metadata is not JSON"))?;
    if metadata.as_object().is_none() {
        return Err(fail("metadata is not a JSON object"));
    }
    match metadata.get("issuer") {
        Some(Value::Str(found)) if found == issuer => {}
        Some(Value::Str(_)) => return Err(fail("the metadata names a different issuer")),
        _ => return Err(fail("the metadata has no issuer")),
    }
    let uri = match metadata.get("jwks_uri") {
        Some(Value::Str(uri)) if !uri.is_empty() => uri.clone(),
        _ => return Err(fail("the metadata has no jwks_uri")),
    };
    if is_https(issuer) && !is_https(&uri) {
        return Err(fail("jwks_uri is not https"));
    }
    Ok(uri)
}

impl RemoteJwks {
    fn with_source(issuer: Option<String>, jwks_uri: Option<String>) -> Self {
        RemoteJwks {
            issuer,
            client: Client::new().with_timeout(Some(Duration::from_secs(10))),
            refresh_interval: Duration::from_secs(3600),
            min_refresh_interval: Duration::from_secs(30),
            max_stale: Duration::from_secs(4 * 3600),
            max_bytes: 1024 * 1024,
            allow_insecure_http: false,
            state: std::sync::Mutex::new(RemoteState {
                jwks: None,
                fetched_at: None,
                last_attempt: None,
                last_error: None,
                jwks_uri,
            }),
            fetching: tokio::sync::Mutex::new(()),
        }
    }

    /// A cache for the JWKS at `url` (an `https://` URL; see
    /// [`allow_insecure_http`](Self::allow_insecure_http)), using a client
    /// with a 10-second request timeout.
    pub fn new(url: impl Into<String>) -> Self {
        RemoteJwks::with_source(None, Some(url.into()))
    }

    /// A cache for the key set of an OpenID Connect issuer, found by
    /// discovery (OpenID Connect Discovery 1.0).
    ///
    /// On first use the provider metadata is fetched from
    /// `{issuer}/.well-known/openid-configuration`. `issuer` must be an
    /// `https://` URL (see [`allow_insecure_http`](Self::allow_insecure_http)).
    /// The metadata must be a JSON object whose `issuer` is **exactly**
    /// `issuer` (§4.3; this stops a metadata document of one tenant or issuer
    /// from vouching for another) and whose `jwks_uri` is a string — an
    /// `https://` URL whenever `issuer` is one. The key set is then fetched
    /// from `jwks_uri` and cached as by [`new`](Self::new). A failure is
    /// [`JwtError::KeyFetch`] and is retried like a failed key-set fetch.
    ///
    /// The `jwks_uri` is **issuer-controlled**: it may name any host,
    /// internal addresses included, and this client will fetch it. Discover
    /// only from issuers you trust, or use [`new`](Self::new) with a fixed
    /// key-set URL. The discovered URL is kept, and discovery runs again
    /// when a fetch from it fails, so an issuer that moves its key set is
    /// followed.
    ///
    /// Every token verified through it must also carry `"iss": issuer`
    /// ([`JwtError::MissingClaim`] / [`JwtError::InvalidIssuer`]), on top of
    /// whatever the [`Validation`] requires.
    ///
    /// ```no_run
    /// use ferroly::auth::{RemoteJwks, Validation};
    ///
    /// # async fn example(token: &str) -> Result<(), ferroly::auth::JwtError> {
    /// let keys = RemoteJwks::from_issuer("https://login.example.com/tenant-1/v2.0");
    /// keys.discover().await?; // optional: fail fast at start-up
    /// let claims = keys.verify(token, &Validation::new().audience("orders-api")).await?;
    /// # let _ = claims;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_issuer(issuer: impl Into<String>) -> Self {
        RemoteJwks::with_source(Some(issuer.into()), None)
    }

    /// The issuer this key set was discovered from, if it was built with
    /// [`from_issuer`](Self::from_issuer).
    pub fn issuer(&self) -> Option<&str> {
        self.issuer.as_deref()
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

    /// How long past its [refresh interval](Self::refresh_interval) a set
    /// keeps being served while every refresh fails. Default 4 hours (four
    /// default refresh intervals). After that, verification fails with the
    /// fetch error ([`JwtError::KeyFetch`]) until a fetch succeeds, so an
    /// outage of — or an attack on — the key-set endpoint cannot keep a
    /// rotated-out key trusted for as long as it lasts.
    pub fn max_stale(mut self, max_stale: Duration) -> Self {
        self.max_stale = max_stale;
        self
    }

    /// Whether plain `http://` issuer and key-set URLs are allowed. Default
    /// `false`: anything but `https://` fails with [`JwtError::KeyFetch`],
    /// because whoever can alter the key set in transit can mint tokens.
    /// Enable it only for a local test identity provider.
    pub fn allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// The largest response body accepted, in bytes (the key set and the
    /// discovery metadata). Default 1 MiB.
    pub fn max_response_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RemoteState> {
        // Every critical section leaves the state consistent.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The key-set URL: the one given to [`new`](Self::new), or the
    /// `jwks_uri` found by discovery, which this fetches now if needed.
    pub async fn discover(&self) -> Result<String, JwtError> {
        let _fetching = self.fetching.lock().await;
        self.jwks_uri().await.map(|(uri, _)| uri)
    }

    /// The key-set URL and whether it came from an earlier discovery (so a
    /// failed fetch from it is a reason to discover again). Call with the
    /// fetch lock held.
    async fn jwks_uri(&self) -> Result<(String, bool), JwtError> {
        let known = self.lock_state().jwks_uri.clone();
        if let Some(uri) = known {
            return Ok((uri, self.issuer.is_some()));
        }
        let uri = self.run_discovery().await?;
        self.lock_state().jwks_uri = Some(uri.clone());
        Ok((uri, false))
    }

    async fn run_discovery(&self) -> Result<String, JwtError> {
        let issuer = self.issuer.as_deref().unwrap_or_default();
        let text = self.fetch_text(&discovery_url(issuer)).await?;
        jwks_uri_from_metadata(issuer, &text)
    }

    /// What the cache can answer without a request.
    fn lookup(&self) -> Lookup {
        let state = self.lock_state();
        let backing_off = state
            .last_attempt
            .is_some_and(|at| at.elapsed() < self.min_refresh_interval);
        let failure = || {
            state
                .last_error
                .clone()
                .map_or(Lookup::Fetch { stale: None }, Lookup::Fail)
        };
        match (&state.jwks, state.fetched_at) {
            (Some(jwks), Some(fetched_at)) => {
                let age = fetched_at.elapsed();
                if age < self.refresh_interval {
                    Lookup::Serve(jwks.clone())
                } else if age >= self.refresh_interval.saturating_add(self.max_stale) {
                    // Too old to trust any longer.
                    if backing_off {
                        failure()
                    } else {
                        Lookup::Fetch { stale: None }
                    }
                } else if backing_off {
                    Lookup::Serve(jwks.clone())
                } else {
                    Lookup::Fetch {
                        stale: Some(jwks.clone()),
                    }
                }
            }
            // Nothing cached and the last attempt failed recently: do not
            // send another request for every token while the issuer is down.
            _ if backing_off => failure(),
            _ => Lookup::Fetch { stale: None },
        }
    }

    /// The current key set, fetching or refreshing it if needed.
    pub async fn get(&self) -> Result<Arc<Jwks>, JwtError> {
        let stale = match self.lookup() {
            Lookup::Serve(jwks) => return Ok(jwks),
            Lookup::Fail(error) => return Err(error),
            Lookup::Fetch { stale } => stale,
        };
        let _fetching = match (self.fetching.try_lock(), stale) {
            (Ok(guard), _) => guard,
            // Another caller is refreshing: keep using the set we have.
            (Err(_), Some(jwks)) => return Ok(jwks),
            (Err(_), None) => self.fetching.lock().await,
        };
        // The caller that held the lock may have fetched meanwhile.
        let stale = match self.lookup() {
            Lookup::Serve(jwks) => return Ok(jwks),
            Lookup::Fail(error) => return Err(error),
            Lookup::Fetch { stale } => stale,
        };
        match self.fetch_now().await {
            Ok(jwks) => Ok(jwks),
            // Keep serving the previous set while the endpoint is
            // unavailable, within the staleness bound.
            Err(error) => stale.ok_or(error),
        }
    }

    /// Fetches the key set now, regardless of its age.
    pub async fn refresh(&self) -> Result<Arc<Jwks>, JwtError> {
        let _fetching = self.fetching.lock().await;
        self.fetch_now().await
    }

    /// Verifies `token` against the (cached) key set and validates its
    /// claims; see [`Jwks::verify`]. On an unknown `kid` the set is
    /// re-fetched once, rate-limited by the minimum refresh interval. A set
    /// found by discovery also requires `iss` to be its
    /// [`issuer`](Self::issuer).
    pub async fn verify(&self, token: &str, validation: &Validation) -> Result<Value, JwtError> {
        let pinned;
        let validation = match &self.issuer {
            Some(issuer) => {
                pinned = validation.with_required_issuer(issuer);
                &pinned
            }
            None => validation,
        };
        // A token that no key set could verify is refused before any fetch,
        // so garbage neither waits for nor triggers a request.
        let parts = Parts::parse(token)?;
        if parts.algorithm()?.is_hmac() {
            return Err(JwtError::UnsupportedAlg);
        }
        let jwks = self.get().await?;
        match jwks.verify(token, validation) {
            Err(JwtError::UnknownKey) => {}
            outcome => return outcome,
        }
        let refreshed = {
            let _fetching = self.fetching.lock().await;
            let (current, recently) = {
                let state = self.lock_state();
                let recently = state
                    .last_attempt
                    .is_some_and(|at| at.elapsed() < self.min_refresh_interval);
                (state.jwks.clone(), recently)
            };
            match current {
                // Another caller refreshed while we were verifying.
                Some(current) if !Arc::ptr_eq(&current, &jwks) => Some(current),
                _ if recently => None,
                _ => self.fetch_now().await.ok(),
            }
        };
        match refreshed {
            Some(jwks) => jwks.verify(token, validation),
            None => Err(JwtError::UnknownKey),
        }
    }

    /// One fetch attempt, recorded in the cache. Call with the fetch lock
    /// held.
    async fn fetch_now(&self) -> Result<Arc<Jwks>, JwtError> {
        self.lock_state().last_attempt = Some(Instant::now());
        let fetched = self.fetch_set().await;
        let mut state = self.lock_state();
        match fetched {
            Ok(jwks) => {
                let jwks = Arc::new(jwks);
                state.jwks = Some(jwks.clone());
                state.fetched_at = Some(Instant::now());
                state.last_error = None;
                Ok(jwks)
            }
            Err(error) => {
                state.last_error = Some(error.clone());
                Err(error)
            }
        }
    }

    async fn fetch_set(&self) -> Result<Jwks, JwtError> {
        let (uri, discovered_earlier) = self.jwks_uri().await?;
        let error = match self.fetch_jwks(&uri).await {
            Ok(jwks) => return Ok(jwks),
            Err(error) if discovered_earlier => error,
            Err(error) => return Err(error),
        };
        // The issuer may have moved its key set: discover again, and try
        // the new location if there is one.
        match self.run_discovery().await {
            Ok(moved) if moved != uri => {
                self.lock_state().jwks_uri = Some(moved.clone());
                self.fetch_jwks(&moved).await
            }
            _ => Err(error),
        }
    }

    async fn fetch_jwks(&self, url: &str) -> Result<Jwks, JwtError> {
        let fail = |reason: &str| JwtError::KeyFetch(reason.to_string());
        let text = self.fetch_text(url).await?;
        let jwks = Jwks::from_json(&text).map_err(|_| fail("response is not a JWK Set"))?;
        if jwks.is_empty() {
            // Never replace a working set with one we cannot use.
            return Err(fail("key set contains no supported key"));
        }
        Ok(jwks)
    }

    /// GETs `url` as JSON text, bounded by the size limit.
    async fn fetch_text(&self, url: &str) -> Result<String, JwtError> {
        let fail = |reason: String| JwtError::KeyFetch(reason);
        if !self.allow_insecure_http && !is_https(url) {
            return Err(fail(format!(
                "{url} is not an https:// URL (RemoteJwks::allow_insecure_http permits plain http)"
            )));
        }
        let request = Request::builder(Method::Get, url)
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
        String::from_utf8(body).map_err(|_| fail("response is not UTF-8".to_string()))
    }
}

/// Whether some key in `jwks` needs a rustls crypto provider and none is
/// available, so tokens for it can never verify.
#[cfg(feature = "turbo")]
pub(crate) fn lacks_crypto_provider(jwks: &Jwks) -> bool {
    jwks.keys()
        .iter()
        .any(|key| key.key_type() != KeyType::Ed25519)
        && crypto_provider().is_none()
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
        // A P-521 signature: two 66-byte values, the body over 127 bytes.
        let der = der_integer_pair(&[0x01; 66], &[0x80; 66]);
        assert_eq!(&der[..3], [0x30, 0x81, 66 + 2 + 67 + 2]);
    }

    #[test]
    fn discovery_urls_drop_a_trailing_slash() {
        assert_eq!(
            discovery_url("https://issuer.example"),
            "https://issuer.example/.well-known/openid-configuration"
        );
        assert_eq!(
            discovery_url("https://login.example.com/tenant/v2.0/"),
            "https://login.example.com/tenant/v2.0/.well-known/openid-configuration"
        );
    }

    #[test]
    fn discovery_metadata_is_checked_against_the_issuer() {
        let issuer = "https://login.example.com/t1/v2.0";
        let doc = |iss: &str, jwks: &str| {
            format!(r#"{{"issuer":"{iss}","jwks_uri":"{jwks}","token_endpoint":"x"}}"#)
        };
        assert_eq!(
            jwks_uri_from_metadata(issuer, &doc(issuer, "https://login.example.com/keys")),
            Ok("https://login.example.com/keys".to_string())
        );
        let rejected = [
            // Another tenant's metadata cannot vouch for this issuer.
            doc(
                "https://login.example.com/t2/v2.0",
                "https://login.example.com/keys",
            ),
            // Not even a trailing-slash difference (OIDC Discovery §4.3).
            doc(
                "https://login.example.com/t1/v2.0/",
                "https://login.example.com/keys",
            ),
            // A downgrade to plain http.
            doc(issuer, "http://login.example.com/keys"),
            doc(issuer, ""),
            format!(r#"{{"issuer":"{issuer}"}}"#),
            format!(r#"{{"issuer":"{issuer}","jwks_uri":7}}"#),
            r#"{"jwks_uri":"https://login.example.com/keys"}"#.to_string(),
            "[]".to_string(),
            "not json".to_string(),
        ];
        for text in rejected {
            assert!(
                matches!(
                    jwks_uri_from_metadata(issuer, &text),
                    Err(JwtError::KeyFetch(_))
                ),
                "{text}"
            );
        }
        // An http issuer (a local test IdP) may name an http key set.
        let local = "http://127.0.0.1:8080";
        assert!(jwks_uri_from_metadata(local, &doc(local, "http://127.0.0.1:8080/k")).is_ok());
    }

    /// Mints an EdDSA token with the test-only Ed25519 signer.
    fn eddsa_token(key: &crate::sig::tests::TestKey, kid: &str, claims: &str) -> String {
        let header =
            base64::encode_url_no_pad(format!(r#"{{"alg":"EdDSA","kid":"{kid}"}}"#).as_bytes());
        let input = format!("{header}.{}", base64::encode_url_no_pad(claims.as_bytes()));
        let sig = crate::sig::tests::sign(key, input.as_bytes());
        format!("{input}.{}", base64::encode_url_no_pad(&sig))
    }

    /// OIDC discovery end to end: metadata -> jwks_uri -> key set -> token,
    /// with the discovered issuer pinned on every token.
    #[tokio::test]
    async fn discovery_finds_the_key_set_and_pins_the_issuer() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;

        use crate::http::{serve, BoxFuture, HttpHandler, HttpResponse, StatusCode};

        struct Idp {
            issuer: Mutex<String>,
            jwks: String,
            metadata_hits: AtomicUsize,
            jwks_hits: AtomicUsize,
        }
        impl HttpHandler for Idp {
            fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
                Box::pin(async move {
                    let issuer = self.issuer.lock().unwrap().clone();
                    let body = match req.uri.path.as_str() {
                        "/tenant/.well-known/openid-configuration" => {
                            self.metadata_hits.fetch_add(1, Ordering::SeqCst);
                            format!(r#"{{"issuer":"{issuer}","jwks_uri":"{issuer}/keys"}}"#)
                        }
                        "/tenant/keys" => {
                            self.jwks_hits.fetch_add(1, Ordering::SeqCst);
                            self.jwks.clone()
                        }
                        _ => return HttpResponse::new(StatusCode(404)),
                    };
                    HttpResponse::new(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(body)
                })
            }
        }

        let key = crate::sig::tests::test_key(&[11; 32]);
        let x = base64::encode_url_no_pad(&key.public);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}/tenant", listener.local_addr().unwrap());
        let idp = Arc::new(Idp {
            issuer: Mutex::new(issuer.clone()),
            jwks: format!(r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","kid":"ed","x":"{x}"}}]}}"#),
            metadata_hits: AtomicUsize::new(0),
            jwks_hits: AtomicUsize::new(0),
        });
        let handler: Arc<dyn HttpHandler> = idp.clone();
        tokio::spawn(async move {
            let _ = serve(listener, handler, std::future::pending::<()>()).await;
        });

        // A plain-http issuer needs the explicit opt-in (SEC-5).
        let refused = RemoteJwks::from_issuer(issuer.clone());
        assert!(matches!(
            refused.discover().await,
            Err(JwtError::KeyFetch(reason)) if reason.contains("https")
        ));
        assert_eq!(idp.metadata_hits.load(Ordering::SeqCst), 0);

        let remote = RemoteJwks::from_issuer(issuer.clone()).allow_insecure_http(true);
        assert_eq!(remote.issuer(), Some(issuer.as_str()));
        assert_eq!(remote.discover().await, Ok(format!("{issuer}/keys")));

        let good = eddsa_token(
            &key,
            "ed",
            &format!(r#"{{"iss":"{issuer}","sub":"u1","exp":4102444800}}"#),
        );
        let claims = remote.verify(&good, &Validation::new()).await.unwrap();
        assert_eq!(claims.get("sub").and_then(Value::as_str), Some("u1"));

        // Signed by the right key, but for another issuer or for none.
        let other = eddsa_token(
            &key,
            "ed",
            r#"{"iss":"https://evil.example","exp":4102444800}"#,
        );
        assert_eq!(
            remote.verify(&other, &Validation::new()).await,
            Err(JwtError::InvalidIssuer)
        );
        let none = eddsa_token(&key, "ed", r#"{"exp":4102444800}"#);
        assert_eq!(
            remote.verify(&none, &Validation::new()).await,
            Err(JwtError::MissingClaim("iss".into()))
        );
        // Metadata and key set were each fetched once.
        assert_eq!(idp.metadata_hits.load(Ordering::SeqCst), 1);
        assert_eq!(idp.jwks_hits.load(Ordering::SeqCst), 1);

        // Metadata that names another issuer is refused.
        *idp.issuer.lock().unwrap() = "https://other.example".to_string();
        let fresh = RemoteJwks::from_issuer(issuer.clone())
            .allow_insecure_http(true)
            .min_refresh_interval(Duration::ZERO);
        assert!(matches!(
            fresh.verify(&good, &Validation::new()).await,
            Err(JwtError::KeyFetch(reason)) if reason.contains("different issuer")
        ));
    }

    /// SEC-5: an issuer that moves its key set is followed: when a fetch
    /// from the discovered `jwks_uri` fails, discovery runs again.
    #[tokio::test]
    async fn a_moved_key_set_is_rediscovered() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;

        use crate::http::{serve, BoxFuture, HttpHandler, HttpResponse, StatusCode};

        struct Idp {
            issuer: String,
            /// The path the metadata names and the only one that serves keys.
            keys_path: Mutex<String>,
            jwks: String,
            metadata_hits: AtomicUsize,
        }
        impl HttpHandler for Idp {
            fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
                Box::pin(async move {
                    let keys_path = self.keys_path.lock().unwrap().clone();
                    let path = req.uri.path.as_str();
                    let body = if path == "/.well-known/openid-configuration" {
                        self.metadata_hits.fetch_add(1, Ordering::SeqCst);
                        format!(
                            r#"{{"issuer":"{}","jwks_uri":"{}{keys_path}"}}"#,
                            self.issuer, self.issuer
                        )
                    } else if path == keys_path {
                        self.jwks.clone()
                    } else {
                        return HttpResponse::new(StatusCode(404));
                    };
                    HttpResponse::new(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(body)
                })
            }
        }

        let key = crate::sig::tests::test_key(&[12; 32]);
        let x = base64::encode_url_no_pad(&key.public);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let idp = Arc::new(Idp {
            issuer: issuer.clone(),
            keys_path: Mutex::new("/keys-v1".to_string()),
            jwks: format!(r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","kid":"ed","x":"{x}"}}]}}"#),
            metadata_hits: AtomicUsize::new(0),
        });
        let handler: Arc<dyn HttpHandler> = idp.clone();
        tokio::spawn(async move {
            let _ = serve(listener, handler, std::future::pending::<()>()).await;
        });

        let remote = RemoteJwks::from_issuer(issuer.clone())
            .allow_insecure_http(true)
            .min_refresh_interval(Duration::ZERO);
        assert!(remote.get().await.is_ok());
        assert_eq!(remote.discover().await, Ok(format!("{issuer}/keys-v1")));
        assert_eq!(idp.metadata_hits.load(Ordering::SeqCst), 1);

        // The issuer moves its key set; the old location now answers 404.
        *idp.keys_path.lock().unwrap() = "/keys-v2".to_string();
        assert_eq!(remote.refresh().await.unwrap().len(), 1);
        assert_eq!(remote.discover().await, Ok(format!("{issuer}/keys-v2")));
        assert_eq!(idp.metadata_hits.load(Ordering::SeqCst), 2);

        // A URL given to `new` is never rediscovered: its failure is final.
        let fixed = RemoteJwks::new(format!("{issuer}/keys-v1"))
            .allow_insecure_http(true)
            .min_refresh_interval(Duration::ZERO);
        assert!(matches!(fixed.refresh().await, Err(JwtError::KeyFetch(_))));
        assert_eq!(idp.metadata_hits.load(Ordering::SeqCst), 2);
    }

    /// A first fetch that fails is retried at most once per minimum refresh
    /// interval; callers in between get the error without a request.
    #[tokio::test]
    async fn a_failing_first_fetch_is_rate_limited() {
        let remote = RemoteJwks::new("http://127.0.0.1:1/jwks.json")
            .allow_insecure_http(true)
            .with_client(Client::new().with_timeout(Some(Duration::from_secs(5))))
            .min_refresh_interval(Duration::from_secs(3600));
        assert!(matches!(remote.get().await, Err(JwtError::KeyFetch(_))));
        let first_attempt = remote.lock_state().last_attempt;
        for _ in 0..3 {
            assert!(matches!(remote.get().await, Err(JwtError::KeyFetch(_))));
        }
        // No new attempt was made.
        assert_eq!(remote.lock_state().last_attempt, first_attempt);
    }

    fn provider_algorithms() -> Vec<Algorithm> {
        [
            Algorithm::Rs256,
            Algorithm::Rs384,
            Algorithm::Rs512,
            Algorithm::Ps256,
            Algorithm::Ps384,
            Algorithm::Ps512,
            Algorithm::Es256,
            Algorithm::Es384,
        ]
        .to_vec()
    }

    #[cfg(feature = "tls-ring")]
    #[test]
    fn ring_exposes_every_rsa_and_p256_p384_verifier() {
        for alg in provider_algorithms() {
            let (key, sig) = provider_ids(alg).unwrap();
            assert!(find_verifier(key, sig).is_ok(), "{alg}");
        }
        // ring has no P-521: ES512 needs a provider such as aws-lc-rs.
        let (key, sig) = provider_ids(Algorithm::Es512).unwrap();
        assert_eq!(
            find_verifier(key, sig).err(),
            Some(JwtError::UnsupportedAlg)
        );
        assert!(provider_ids(Algorithm::EdDsa).is_none());
        assert!(provider_ids(Algorithm::Hs256).is_none());
    }

    #[cfg(not(feature = "tls-ring"))]
    #[test]
    fn without_a_provider_verification_fails_with_a_typed_error() {
        // No provider is installed in this test process.
        for alg in provider_algorithms() {
            let (key, sig) = provider_ids(alg).unwrap();
            assert_eq!(
                find_verifier(key, sig).err(),
                Some(JwtError::NoCryptoProvider),
                "{alg}"
            );
        }
        // EdDSA needs no provider: a wrong signature is a plain failure.
        let key = Jwk::from_ed25519_bytes(
            &base64::decode_url_no_pad("11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo").unwrap(),
        )
        .unwrap();
        assert_eq!(
            key.verify_signature(b"m", &[0u8; 64]),
            Err(JwtError::InvalidSignature)
        );
    }
}
