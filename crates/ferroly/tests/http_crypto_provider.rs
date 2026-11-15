#![cfg(feature = "http")]
//! rustls crypto-provider resolution when the application installs none:
//! with `tls-ring`, ferroly falls back to `ring` without installing it as the
//! process default; without `tls-ring`, every TLS entry point fails with a
//! typed error naming the fix, and plain-text HTTP keeps working.
//!
//! Nothing here installs a process default (that case is in
//! `http_crypto_provider_installed.rs`, its own process).

use std::sync::Arc;
use std::time::Duration;

use ferroly::http::{
    BoxFuture, Client, ClientTlsConfig, HttpError, HttpHandler, HttpResponse, Method, Request,
    StatusCode, TlsConfig,
};
use tokio::net::TcpListener;

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");

struct Ok200;
impl HttpHandler for Ok200 {
    fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async { HttpResponse::text(StatusCode::OK, "ok").header("connection", "close") })
    }
}

fn test_ca_only() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .without_web_pki_roots()
        .add_root_pem(CA)
        .unwrap()
}

async fn fetch(client: &Client, url: &str) -> Result<String, HttpError> {
    let req = Request::builder(Method::Get, url)?.build();
    client.send(req).await?.text().await
}

#[cfg(not(feature = "tls-ring"))]
async fn spawn_plain() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = ferroly::http::serve(listener, Arc::new(Ok200), std::future::pending::<()>()).await;
    });
    addr
}

#[cfg(feature = "tls-ring")]
#[tokio::test]
async fn ring_is_the_fallback_and_is_not_installed_globally() {
    use ferroly::http::serve_tls;

    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    let tls = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_tls(listener, Arc::new(Ok200), std::future::pending::<()>(), tls).await;
    });
    let client = Client::builder()
        .connect_timeout(Some(Duration::from_secs(5)))
        .tls(test_ca_only())
        .build()
        .unwrap();
    let body = fetch(&client, &format!("https://localhost:{}/", addr.port()))
        .await
        .unwrap();
    assert_eq!(body, "ok");
    // The fallback is used explicitly; the application's choice stays open.
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}

#[cfg(not(feature = "tls-ring"))]
fn assert_no_provider(err: HttpError) {
    match err {
        HttpError::Tls(message) => {
            assert!(message.contains("no rustls CryptoProvider"), "{message}");
            assert!(message.contains("install_default"), "{message}");
            assert!(message.contains("tls-ring"), "{message}");
        }
        other => panic!("expected HttpError::Tls, got {other:?}"),
    }
}

#[cfg(not(feature = "tls-ring"))]
#[test]
fn without_a_provider_tls_configuration_fails_with_a_typed_error() {
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    assert_no_provider(TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap_err());
    let mtls = TlsConfig::builder()
        .cert_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .client_auth_required_pem(CA)
        .unwrap()
        .build();
    assert_no_provider(mtls.unwrap_err());
    assert_no_provider(Client::builder().tls(test_ca_only()).build().unwrap_err());
}

#[cfg(not(feature = "tls-ring"))]
#[tokio::test]
async fn without_a_provider_plain_http_works_and_https_fails_cleanly() {
    // Building a default client needs no provider...
    let client = Client::builder()
        .connect_timeout(Some(Duration::from_secs(5)))
        .build()
        .unwrap();
    // ...so plain-text HTTP is unaffected...
    let addr = spawn_plain().await;
    let body = fetch(&client, &format!("http://127.0.0.1:{}/", addr.port()))
        .await
        .unwrap();
    assert_eq!(body, "ok");
    // ...and an https request reports the missing provider instead of
    // panicking (a TCP listener is enough: no handshake is attempted).
    let err = fetch(&client, &format!("https://127.0.0.1:{}/", addr.port()))
        .await
        .unwrap_err();
    assert_no_provider(err);
}

#[cfg(all(feature = "auth", not(feature = "tls-ring")))]
#[test]
fn without_a_provider_jwk_verification_fails_with_a_typed_error() {
    use ferroly::auth::{Jwk, JwtError};

    let ec = Jwk::from_json(
        r#"{"kty":"EC","crv":"P-256","x":"f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU","y":"x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"}"#,
    )
    .unwrap();
    assert_eq!(
        ec.verify_signature(b"message", &[1u8; 64]),
        Err(JwtError::NoCryptoProvider)
    );
    assert!(JwtError::NoCryptoProvider
        .to_string()
        .contains("no rustls CryptoProvider"));
}
