#![cfg(all(feature = "auth", feature = "http", feature = "tls-ring"))]
//! JWT verification follows the **installed** rustls `CryptoProvider`, even
//! one installed after the first verification, and reaches algorithms
//! `ring` lacks (ES512 / P-521) through a provider that has them.
//!
//! Installing a process-default provider is global, so this file holds a
//! single test and runs in its own process.

use std::sync::Mutex;

use ferroly::auth::{Algorithm, Jwk, JwtError, KeyType, Validation};
use ferroly::hash::base64;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls_pki_types::{
    alg_id, AlgorithmIdentifier, InvalidSignature, SignatureVerificationAlgorithm,
};

/// What a fake verifier was asked to check.
type Call = (Vec<u8>, Vec<u8>, Vec<u8>);

/// A verification algorithm that records its inputs and accepts everything.
#[derive(Debug)]
struct Recorder {
    public_key: AlgorithmIdentifier,
    signature: AlgorithmIdentifier,
    calls: Mutex<Vec<Call>>,
}

impl SignatureVerificationAlgorithm for Recorder {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        self.calls.lock().unwrap().push((
            public_key.to_vec(),
            message.to_vec(),
            signature.to_vec(),
        ));
        Ok(())
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.public_key
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.signature
    }
}

static P521: Recorder = Recorder {
    public_key: alg_id::ECDSA_P521,
    signature: alg_id::ECDSA_SHA512,
    calls: Mutex::new(Vec::new()),
};
static PS256: Recorder = Recorder {
    public_key: alg_id::RSA_ENCRYPTION,
    signature: alg_id::RSA_PSS_SHA256,
    calls: Mutex::new(Vec::new()),
};
static ALL: [&dyn SignatureVerificationAlgorithm; 2] = [&P521, &PS256];

/// RFC 7515 A.3 (ES256) under its P-256 key, verified by ring.
const ES256_TOKEN: &str = "eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODA\
     sDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.DtEhU3ljbEg8\
     L38VWAfUAqOyKAM6-Xx-F4GawxaepmXFCgfTjDxw5djxLa8ISlSApmWQxfKTUJqP\
     P3-Kg6NU1Q";

#[test]
fn verification_follows_the_provider_installed_later() {
    let validation = Validation::new().leeway(0).current_time(1_300_819_320);
    let ec = Jwk::from_json(
        r#"{"kty":"EC","crv":"P-256","x":"f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU","y":"x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"}"#,
    )
    .unwrap();
    let p521 = Jwk::from_ec_coordinates(KeyType::EcP521, &[0x11; 66], &[0x22; 66]).unwrap();

    // 1. Nothing installed: ring (the `tls-ring` fallback) verifies ES256 and
    //    has no P-521.
    assert!(ec.verify(ES256_TOKEN, &validation).is_ok());
    assert_eq!(
        p521.verify_signature(b"msg", &[0xab; 132]),
        Err(JwtError::UnsupportedAlg)
    );

    // 2. The application installs its own provider afterwards (here: one
    //    whose only verifiers are the two recorders).
    CryptoProvider {
        signature_verification_algorithms: WebPkiSupportedAlgorithms {
            all: &ALL,
            mapping: &[],
        },
        ..rustls::crypto::ring::default_provider()
    }
    .install_default()
    .unwrap();

    // 3. It is used from the next verification on: ES256 is gone (it has no
    //    such verifier), so nothing was cached from step 1...
    assert_eq!(
        ec.verify(ES256_TOKEN, &validation),
        Err(JwtError::UnsupportedAlg)
    );

    // ...and ES512 now reaches its P-521 verifier, with the JOSE encodings
    //    converted: the SEC1 point, and r || s as a DER ECDSA-Sig-Value.
    let mut sig = vec![0x00; 66];
    sig[1] = 0x01; // r = 0x0001.. (a leading zero byte to drop)
    sig.extend(std::iter::repeat(0x80).take(66)); // s with the top bit set
    assert_eq!(p521.verify_signature(b"msg", &sig), Ok(()));
    let calls = P521.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let (point, message, der) = &calls[0];
    assert_eq!(message, b"msg");
    assert_eq!(point.len(), 133);
    assert_eq!(point[0], 0x04);
    // SEQUENCE (long form, 136 bytes) { INTEGER 65 bytes, INTEGER 67 bytes }.
    assert_eq!(&der[..3], [0x30, 0x81, 2 + 65 + 2 + 67]);
    assert_eq!(&der[3..5], [0x02, 65]);
    assert_eq!(der[5], 0x01);
    assert_eq!(&der[3 + 2 + 65..3 + 2 + 65 + 3], [0x02, 67, 0x00]);
    assert_eq!(der.len(), 3 + 136);

    // PS256 reaches the provider's RSA-PSS verifier with the DER RSAPublicKey.
    let n = {
        let mut n = vec![0xc5u8; 256];
        n[255] = 0x01;
        n
    };
    let rsa = Jwk::from_rsa_components(&n, &[1, 0, 1]).unwrap();
    let header = base64::encode_url_no_pad(br#"{"alg":"PS256"}"#);
    let payload = base64::encode_url_no_pad(br#"{"exp":4102444800}"#);
    let token = format!(
        "{header}.{payload}.{}",
        base64::encode_url_no_pad(&[7; 256])
    );
    assert!(rsa.verify(&token, &Validation::new()).is_ok());
    let calls = PS256.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(&calls[0].0[..4], [0x30, 0x82, 0x01, 0x0a]);
    assert_eq!(calls[0].1, format!("{header}.{payload}").as_bytes());
    assert_eq!(calls[0].2, vec![7; 256]);
    // The installed provider lacks RS256: it is not silently served by ring.
    assert_eq!(
        rsa.verify_signature_with(Algorithm::Rs256, b"m", &[7; 256]),
        Err(JwtError::UnsupportedAlg)
    );
}
