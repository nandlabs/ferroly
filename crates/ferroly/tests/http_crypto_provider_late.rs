#![cfg(all(feature = "http", feature = "auth", feature = "tls-ring"))]
//! A process-default rustls provider installed **after** ferroly already used
//! the `ring` fallback — a first https request with the default client
//! configuration and a first RS256 verification — still takes over from then
//! on: nothing cached with `ring` keeps being used.
//!
//! Installing a process default is global and permanent, so this lives in its
//! own integration-test binary, as one test that orders the steps itself.

use std::net::SocketAddr;
use std::time::Duration;

use ferroly::auth::{Jwk, JwtError};
use ferroly::http::{Client, Method, Request};
use rustls::crypto::{ring, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls_pki_types::alg_id;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

// An RSA-2048 public key and an RS256 token signed with the OpenSSL
// command-line tool; the same fixture as in `auth_jwks.rs`.
const RSA_N: &str = "uOZtUwSf139eYpboPCSTbEcmsJhZkvFJJ3Zr2Ipn-1b71vaGZFfsld0ytcC0UHGG\
     dRsvSGw0_NFGAN3gR8w7CGA1MAMYvtpuZs12jRUzhlmD2-ud4ZcDkji9e6FkZ2e5\
     8FsjH5Bi6E_nQiPoMdg1LpjbPqcAaMpm92DDhMs1Qz53uoXsk7-rJCCWqW-T82lw\
     f4c3F59Di8uso3i8uPEdxcFhMiXmKWO-nd2d5gnv4qT9HkfwnJiCRyPP7frSdWMu\
     VT9EuOxU0EWhG3e3SHxLWQ9ivLFN_mO-H5VLEyachrcNIRi5rTwCFnsYSi-xMc5L\
     6sTAVzxMvQQb0ylk4wSEnQ";
const RSA_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0xIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjpbIm9yZGVycy1hcGkiLCJi\
     aWxsaW5nLWFwaSJdLCJzdWIiOiJ1c2VyLTEiLCJzY29wZSI6Im9yZGVyczpyZWFk\
     IG9yZGVyczp3cml0ZSIsImlhdCI6MTcwMDAwMDAwMCwiZXhwIjo0MTAyNDQ0ODAw\
     fQ.UoNJweNTPUYgO_s21uutCaPFrnkHC8ykGIgZ04bOR2-PAtDBZuDVJ9HoC0w1e\
     0HhvDlSsItePNe--p3BkKzLSr5YilqAkm-tJFOkcmduKLEQsrtPkWHQ_iskg3k2C\
     B8wZ57DbmmSGQLXxFUFyKhJTlXH3odT2FIjSzjZ5zj64FYtyYTAjmeaayHxB3tL8\
     yL_Q_nd_pt-IAPZbEPXyXm2X0r0oYzxA579kg5jKL81Vg5WjcJ9nmlRd-wgF2kTj\
     IxoRdRupJNweJOvaZSTHu6zge7ySGD85AyJIt1IAKl5onQlSSo_Nc0Ipe7jEQmPj\
     wkMgj37H450eRJK_tTyy01L3A";

/// TLS 1.3 cipher-suite ids as they appear in a ClientHello.
const AES_128_GCM: u16 = 0x1301;
const CHACHA20_POLY1305: u16 = 0x1303;

/// `ring`, restricted to one TLS 1.3 suite and to non-RSA signatures, so both
/// its use for TLS and for JWK verification are recognisable.
fn restricted_ring() -> CryptoProvider {
    let full = ring::default_provider();
    let all: Vec<_> = full
        .signature_verification_algorithms
        .all
        .iter()
        .copied()
        .filter(|alg| alg.public_key_alg_id() != alg_id::RSA_ENCRYPTION)
        .collect();
    let mapping: Vec<_> = full
        .signature_verification_algorithms
        .mapping
        .iter()
        .copied()
        .filter(|(scheme, _)| {
            matches!(
                scheme,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256
                    | rustls::SignatureScheme::ECDSA_NISTP384_SHA384
                    | rustls::SignatureScheme::ED25519
            )
        })
        .collect();
    CryptoProvider {
        cipher_suites: vec![ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256],
        signature_verification_algorithms: WebPkiSupportedAlgorithms {
            all: Box::leak(all.into_boxed_slice()),
            mapping: Box::leak(mapping.into_boxed_slice()),
        },
        ..full
    }
}

fn rs256_check() -> Result<(), JwtError> {
    let key = Jwk::from_json(&format!(
        r#"{{"kty":"RSA","kid":"rsa-1","e":"AQAB","n":"{RSA_N}"}}"#
    ))
    .unwrap();
    let (message, signature) = RSA_TOKEN.rsplit_once('.').unwrap();
    let signature = ferroly::hash::base64::decode_url_no_pad(signature).unwrap();
    key.verify_signature(message.as_bytes(), &signature)
}

/// Starts an https request with a default `Client` (so the shared default
/// TLS configuration) against a TCP listener that only reads the ClientHello,
/// and returns the cipher suites it offered.
async fn offered_suites() -> Vec<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let request = tokio::spawn(async move {
        let client = Client::builder()
            .timeout(Some(Duration::from_secs(5)))
            .build()
            .unwrap();
        let req = Request::builder(Method::Get, &format!("https://127.0.0.1:{}/", addr.port()))
            .unwrap()
            .build();
        let _ = client.send(req).await; // fails: nobody answers the hello
    });

    let (mut tcp, _) = listener.accept().await.unwrap();
    let mut hello = Vec::new();
    let mut buf = [0u8; 4096];
    let suites = loop {
        let n = tcp.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed before a full ClientHello");
        hello.extend_from_slice(&buf[..n]);
        if let Some(suites) = parse_suites(&hello) {
            break suites;
        }
    };
    drop(tcp);
    request.await.unwrap();
    suites
}

/// The cipher suites of a TLS record holding a ClientHello, once enough bytes
/// have arrived.
fn parse_suites(bytes: &[u8]) -> Option<Vec<u16>> {
    // record header (5) + handshake header (4) + version (2) + random (32)
    let at = 5 + 4 + 2 + 32;
    assert_eq!(bytes.first(), Some(&0x16), "a TLS handshake record");
    let session_len = *bytes.get(at)? as usize;
    let suites_at = at + 1 + session_len;
    let len = u16::from_be_bytes([*bytes.get(suites_at)?, *bytes.get(suites_at + 1)?]) as usize;
    let list = bytes.get(suites_at + 2..suites_at + 2 + len)?;
    Some(
        list.chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect(),
    )
}

#[tokio::test]
async fn a_provider_installed_after_first_use_takes_over() {
    // Nothing installed yet: the `ring` fallback verifies RS256 and offers
    // its full suite list.
    assert_eq!(rs256_check(), Ok(()));
    let before = offered_suites().await;
    assert!(before.contains(&AES_128_GCM), "{before:04x?}");

    restricted_ring()
        .install_default()
        .expect("no other process default in this test binary");

    // The installed provider has no RSA verifier: RS256 is now unsupported
    // instead of silently checked with the `ring` verifier found earlier.
    assert_eq!(rs256_check(), Err(JwtError::UnsupportedAlg));
    // And a new default-configured https connection offers only its suite.
    let after = offered_suites().await;
    assert!(after.contains(&CHACHA20_POLY1305), "{after:04x?}");
    assert!(!after.contains(&AES_128_GCM), "{after:04x?}");
}
