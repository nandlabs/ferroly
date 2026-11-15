#![cfg(all(feature = "http", feature = "hash", feature = "tls-ring"))]
//! The rustls crypto provider the application installs as the process default
//! wins over the `ring` fallback, for the TLS server, the TLS client and JWK
//! verification alike.
//!
//! Installing a process default is global and permanent, so these tests live
//! in their own integration-test binary (their own process). The installed
//! provider is `ring` cut down to something recognisable: the single TLS 1.3
//! suite `TLS13_CHACHA20_POLY1305_SHA256`, and no RSA signature verification.

use std::net::SocketAddr;
use std::sync::{Arc, Once};
use std::time::Duration;

use ferroly::http::{
    serve_tls, BoxFuture, Client, ClientTlsConfig, HttpError, HttpHandler, HttpResponse, Method,
    Request, StatusCode, TlsConfig,
};
use rustls::crypto::{ring, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::{CipherSuite, SignatureScheme};
use rustls_pki_types::{alg_id, CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");

/// `ring`, restricted to one TLS 1.3 suite and to non-RSA signatures.
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
                SignatureScheme::ECDSA_NISTP256_SHA256
                    | SignatureScheme::ECDSA_NISTP384_SHA384
                    | SignatureScheme::ED25519
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

fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        restricted_ring()
            .install_default()
            .expect("no other process default in this test binary");
    });
}

/// A full `ring` provider limited to `suites`, named explicitly so it does
/// not depend on the process default.
fn explicit_ring(suites: Vec<rustls::SupportedCipherSuite>) -> Arc<CryptoProvider> {
    let mut provider = ring::default_provider();
    if !suites.is_empty() {
        provider.cipher_suites = suites;
    }
    Arc::new(provider)
}

fn pem_section(pem: &[u8], label: &str) -> Vec<u8> {
    let text = std::str::from_utf8(pem).unwrap();
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin).unwrap() + begin.len();
    let stop = text[start..].find(&end).unwrap() + start;
    let b64: String = text[start..stop].split_whitespace().collect();
    ferroly::hash::base64::decode(&b64).unwrap()
}

fn server_chain() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let cert = CertificateDer::from(pem_section(SERVER_CERT, "CERTIFICATE"));
    let key = PrivateKeyDer::try_from(pem_section(SERVER_KEY, "PRIVATE KEY")).unwrap();
    (vec![cert], key)
}

fn ca_roots() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(pem_section(CA, "CERTIFICATE")))
        .unwrap();
    roots
}

struct Ok200;
impl HttpHandler for Ok200 {
    fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async { HttpResponse::text(StatusCode::OK, "ok").header("connection", "close") })
    }
}

async fn spawn_ferroly_server() -> SocketAddr {
    let tls = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_tls(listener, Arc::new(Ok200), std::future::pending::<()>(), tls).await;
    });
    addr
}

/// A raw rustls HTTPS server on an explicit provider: answers one request
/// per connection with `ok` and records nothing else.
async fn spawn_raw_server(provider: Arc<CryptoProvider>) -> SocketAddr {
    let (chain, key) = server_chain();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

fn ferroly_client() -> Client {
    let tls = ClientTlsConfig::new()
        .without_web_pki_roots()
        .add_root_pem(CA)
        .unwrap();
    Client::builder()
        .timeout(Some(Duration::from_secs(5)))
        .connect_timeout(Some(Duration::from_secs(5)))
        .tls(tls)
        .build()
        .unwrap()
}

async fn fetch(client: &Client, url: &str) -> Result<String, HttpError> {
    let req = Request::builder(Method::Get, url)?.build();
    client.send(req).await?.text().await
}

#[tokio::test]
async fn the_server_uses_the_installed_default_provider() {
    install();
    let addr = spawn_ferroly_server().await;

    // A client offering every ring suite (AES-256-GCM first) still ends up on
    // ChaCha20: the only suite the server's provider knows.
    let config = rustls::ClientConfig::builder_with_provider(explicit_ring(Vec::new()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(ca_roots())
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let stream = connector.connect(name, tcp).await.unwrap();
    let suite = stream
        .get_ref()
        .1
        .negotiated_cipher_suite()
        .unwrap()
        .suite();
    assert_eq!(suite, CipherSuite::TLS13_CHACHA20_POLY1305_SHA256);
}

#[tokio::test]
async fn the_client_uses_the_installed_default_provider() {
    install();
    let client = ferroly_client();

    // ferroly client -> ferroly server: both on the installed provider.
    let addr = spawn_ferroly_server().await;
    let body = fetch(&client, &format!("https://localhost:{}/", addr.port()))
        .await
        .unwrap();
    assert_eq!(body, "ok");

    // A server that speaks every ring suite accepts the client...
    let addr = spawn_raw_server(explicit_ring(Vec::new())).await;
    let body = fetch(&client, &format!("https://localhost:{}/", addr.port()))
        .await
        .unwrap();
    assert_eq!(body, "ok");

    // ...but one limited to AES-128-GCM shares no suite with it: the client
    // offered only what the installed provider has.
    let aes_only = explicit_ring(vec![ring::cipher_suite::TLS13_AES_128_GCM_SHA256]);
    let addr = spawn_raw_server(aes_only).await;
    let err = fetch(&client, &format!("https://localhost:{}/", addr.port()))
        .await
        .unwrap_err();
    assert!(matches!(err, HttpError::Tls(_)), "err={err:?}");
}

#[cfg(feature = "auth")]
#[test]
fn jwk_verification_uses_the_installed_default_provider() {
    use ferroly::auth::{Jwk, JwtError};

    install();
    // RFC 7515 appendix A.3 (ES256): verifies with the installed provider.
    const A3_TOKEN: &str = "eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODA\
         sDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.DtEhU3ljbEg8\
         L38VWAfUAqOyKAM6-Xx-F4GawxaepmXFCgfTjDxw5djxLa8ISlSApmWQxfKTUJqP\
         P3-Kg6NU1Q";
    let ec = Jwk::from_json(
        r#"{"kty":"EC","crv":"P-256","x":"f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU","y":"x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"}"#,
    )
    .unwrap();
    let (message, signature) = A3_TOKEN.rsplit_once('.').unwrap();
    let signature = ferroly::hash::base64::decode_url_no_pad(signature).unwrap();
    assert_eq!(ec.verify_signature(message.as_bytes(), &signature), Ok(()));

    // The installed provider has no RSA verifier, and the ring fallback is
    // not consulted: RS256 is unsupported.
    let rsa = Jwk::from_json(
        r#"{"kty":"RSA","e":"AQAB","n":"ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp-Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOWvG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5rexMdbBYUsLA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9kG4LT6_I5IhlJH7aGhyxXFvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2PAcDTW9gb54h4FRWyuXpoQ"}"#,
    )
    .unwrap();
    assert_eq!(
        rsa.verify_signature(b"message", &[0u8; 256]),
        Err(JwtError::UnsupportedAlg)
    );
}
