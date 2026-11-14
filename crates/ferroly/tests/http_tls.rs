#![cfg(feature = "http")]
//! TLS configuration tests: custom CA roots, mutual TLS (required and
//! optional), certificate reload, and HTTPS through a `CONNECT` proxy. All
//! certificates are test-only fixtures under `src/http/testdata`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ferroly::http::{
    serve_tls_reloadable, serve_tls_with_config, BoxFuture, Client, ClientTlsConfig, HttpError,
    HttpHandler, HttpResponse, Method, Proxy, Request, ServerConfig, StatusCode, TlsConfig,
    TlsVersion,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");
const SERVER2_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server2_cert.pem");
const SERVER2_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server2_key.pem");
const CLIENT_CERT: &[u8] = include_bytes!("../src/http/testdata/test_client_cert.pem");
const CLIENT_KEY: &[u8] = include_bytes!("../src/http/testdata/test_client_key.pem");
const ROGUE_CLIENT_CERT: &[u8] = include_bytes!("../src/http/testdata/rogue_client_cert.pem");
const ROGUE_CLIENT_KEY: &[u8] = include_bytes!("../src/http/testdata/rogue_client_key.pem");
/// A certificate the test CA did not issue.
const SELF_SIGNED_CERT: &[u8] = include_bytes!("../src/http/testdata/self_signed_cert.pem");
const SELF_SIGNED_KEY: &[u8] = include_bytes!("../src/http/testdata/self_signed_key.pem");

struct Describe;
impl HttpHandler for Describe {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            HttpResponse::text(
                StatusCode::OK,
                format!(
                    "tls={} certs={} peer={}",
                    req.is_tls(),
                    req.peer_certificates().len(),
                    req.peer_addr().is_some()
                ),
            )
            // One request per connection, so every request handshakes afresh.
            .header("connection", "close")
        })
    }
}

fn config() -> ServerConfig {
    ServerConfig {
        head_timeout: Duration::from_secs(5),
        ..ServerConfig::default()
    }
}

async fn spawn_tls(tls: TlsConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_tls_with_config(
            listener,
            Arc::new(Describe),
            std::future::pending::<()>(),
            config(),
            tls,
        )
        .await;
    });
    addr
}

/// TLS options trusting only the test CA.
fn test_ca_only() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .without_web_pki_roots()
        .add_root_pem(CA)
        .unwrap()
}

fn client(tls: ClientTlsConfig) -> Client {
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
async fn custom_ca_roots_are_trusted_and_verification_stays_on() {
    let addr = spawn_tls(TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap()).await;
    let port = addr.port();

    // A private CA added as a root: both names in the certificate verify.
    let trusted = client(test_ca_only());
    for host in ["localhost", "127.0.0.1"] {
        let body = fetch(&trusted, &format!("https://{host}:{port}/"))
            .await
            .unwrap();
        assert_eq!(body, "tls=true certs=0 peer=true", "{host}");
    }
    // Adding to (rather than replacing) the bundled roots works too.
    let both = client(ClientTlsConfig::new().add_root_pem(CA).unwrap());
    assert!(fetch(&both, &format!("https://localhost:{port}/"))
        .await
        .is_ok());

    // The default client trusts the public web PKI only: unknown issuer.
    let err = fetch(&Client::new(), &format!("https://localhost:{port}/"))
        .await
        .unwrap_err();
    assert!(matches!(err, HttpError::Tls(_)), "{err:?}");

    // Trusting a different certificate does not help either.
    let other = client(
        ClientTlsConfig::new()
            .without_web_pki_roots()
            .add_root_pem(SELF_SIGNED_CERT)
            .unwrap(),
    );
    let err = fetch(&other, &format!("https://localhost:{port}/"))
        .await
        .unwrap_err();
    assert!(matches!(err, HttpError::Tls(_)), "{err:?}");
}

#[tokio::test]
async fn mutual_tls_required() {
    let tls = TlsConfig::builder()
        .cert_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .client_auth_required_pem(CA)
        .unwrap()
        .build()
        .unwrap();
    let addr = spawn_tls(tls).await;
    let url = format!("https://localhost:{}/", addr.port());

    // A client certificate issued by the configured CA is accepted, and the
    // handler can see it.
    let with_cert = client(
        test_ca_only()
            .identity_pem(CLIENT_CERT, CLIENT_KEY)
            .unwrap(),
    );
    assert_eq!(
        fetch(&with_cert, &url).await.unwrap(),
        "tls=true certs=1 peer=true"
    );

    // No certificate: refused.
    assert!(fetch(&client(test_ca_only()), &url).await.is_err());

    // A certificate from a CA the server does not trust: refused.
    let rogue = client(
        test_ca_only()
            .identity_pem(ROGUE_CLIENT_CERT, ROGUE_CLIENT_KEY)
            .unwrap(),
    );
    assert!(fetch(&rogue, &url).await.is_err());
}

#[tokio::test]
async fn mutual_tls_optional() {
    let tls = TlsConfig::builder()
        .cert_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .client_auth_optional_pem(CA)
        .unwrap()
        .min_version(TlsVersion::Tls13)
        .build()
        .unwrap();
    let addr = spawn_tls(tls).await;
    let url = format!("https://localhost:{}/", addr.port());

    // Anonymous clients are served, and the handler can tell.
    assert_eq!(
        fetch(&client(test_ca_only()), &url).await.unwrap(),
        "tls=true certs=0 peer=true"
    );
    let with_cert = client(
        test_ca_only()
            .identity_pem(CLIENT_CERT, CLIENT_KEY)
            .unwrap()
            .min_version(TlsVersion::Tls13),
    );
    assert_eq!(
        fetch(&with_cert, &url).await.unwrap(),
        "tls=true certs=1 peer=true"
    );

    // A certificate that is presented must still verify.
    let rogue = client(
        test_ca_only()
            .identity_pem(ROGUE_CLIENT_CERT, ROGUE_CLIENT_KEY)
            .unwrap(),
    );
    assert!(fetch(&rogue, &url).await.is_err());
}

#[test]
fn tls_configuration_errors_are_reported() {
    // Mismatched or missing pieces fail at build time, not at first use.
    assert!(TlsConfig::builder().build().is_err());
    assert!(TlsConfig::builder()
        .cert_pem(SERVER_CERT, b"no key")
        .is_err());
    assert!(TlsConfig::builder()
        .cert_pem(b"no cert", SERVER_KEY)
        .is_err());
    assert!(TlsConfig::builder()
        .cert_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .client_auth_required_pem(b"no ca")
        .is_err());
    assert!(TlsConfig::builder()
        .cert_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .client_auth_required_der(Vec::new())
        .build()
        .is_err());
    // A key that does not belong to the certificate.
    assert!(TlsConfig::from_pem(SERVER_CERT, CLIENT_KEY).is_err());

    assert!(ClientTlsConfig::new()
        .identity_pem(CLIENT_CERT, b"no key")
        .is_err());
    assert!(Client::builder()
        .tls(ClientTlsConfig::new().without_web_pki_roots())
        .build()
        .is_err());
    // Key material never shows up in Debug output.
    let debug = format!(
        "{:?}",
        test_ca_only()
            .identity_pem(CLIENT_CERT, CLIENT_KEY)
            .unwrap()
    );
    assert!(
        debug.contains("identity: true") && !debug.contains("["),
        "{debug}"
    );
}

#[tokio::test]
async fn certificate_can_be_reloaded_for_new_connections() {
    // Start with a certificate the client does not trust.
    let tls = TlsConfig::from_pem(SELF_SIGNED_CERT, SELF_SIGNED_KEY)
        .unwrap()
        .reloadable();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://localhost:{}/",
        listener.local_addr().unwrap().port()
    );
    let handle = tls.clone();
    tokio::spawn(async move {
        let _ = serve_tls_reloadable(
            listener,
            Arc::new(Describe),
            std::future::pending::<()>(),
            config(),
            tls,
        )
        .await;
    });

    let client = client(test_ca_only());
    let err = fetch(&client, &url).await.unwrap_err();
    assert!(matches!(err, HttpError::Tls(_)), "{err:?}");

    // Swap in a certificate from the trusted CA: the very next connection
    // gets it, with no restart.
    handle.reload(TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap());
    assert_eq!(
        fetch(&client, &url).await.unwrap(),
        "tls=true certs=0 peer=true"
    );

    // Rotate again, to another valid certificate and to mutual TLS.
    handle.reload(
        TlsConfig::builder()
            .cert_pem(SERVER2_CERT, SERVER2_KEY)
            .unwrap()
            .client_auth_required_pem(CA)
            .unwrap()
            .build()
            .unwrap(),
    );
    assert!(fetch(&client, &url).await.is_err());
    let with_cert = self::client(
        test_ca_only()
            .identity_pem(CLIENT_CERT, CLIENT_KEY)
            .unwrap(),
    );
    assert_eq!(
        fetch(&with_cert, &url).await.unwrap(),
        "tls=true certs=1 peer=true"
    );

    // And back to the untrusted one.
    handle.reload(TlsConfig::from_pem(SELF_SIGNED_CERT, SELF_SIGNED_KEY).unwrap());
    assert!(fetch(&client, &url).await.is_err());
}

/// A minimal forward proxy: answers `CONNECT host:port` by connecting there
/// and relaying bytes both ways. Records the CONNECT request it received.
async fn connect_proxy() -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            let record = record.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0u8; 1];
                    if client.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let target = head.split(' ').nth(1).unwrap_or_default().to_string();
                record.lock().unwrap().push(head);
                let Ok(mut upstream) = TcpStream::connect(&target).await else {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                if client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn https_through_a_connect_tunnel() {
    let server = spawn_tls(TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap()).await;
    let (proxy_addr, seen) = connect_proxy().await;

    let proxy = Proxy::all(&format!("http://{proxy_addr}"))
        .unwrap()
        .basic_auth("user", "pass");
    let client = Client::builder()
        .timeout(Some(Duration::from_secs(5)))
        .tls(test_ca_only())
        .proxy(proxy)
        .build()
        .unwrap();
    let url = format!("https://127.0.0.1:{}/secret/path?token=abc", server.port());
    let mut req = Request::builder(Method::Get, &url).unwrap().build();
    req.headers
        .set("authorization", "Bearer for-the-origin-only");
    let body = client.send(req).await.unwrap().text().await.unwrap();
    // TLS is end to end: the origin server saw a TLS connection.
    assert_eq!(body, "tls=true certs=0 peer=true");

    let connects = seen.lock().unwrap().clone();
    assert_eq!(connects.len(), 1);
    let connect = &connects[0];
    let authority = format!("127.0.0.1:{}", server.port());
    assert!(
        connect.starts_with(&format!(
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n"
        )),
        "{connect:?}"
    );
    assert!(
        connect.contains("\r\nProxy-Authorization: Basic dXNlcjpwYXNz\r\n"),
        "{connect:?}"
    );
    // The proxy learns the host and port, and nothing else.
    assert!(
        !connect.contains("secret") && !connect.contains("token"),
        "{connect:?}"
    );
    assert!(!connect.contains("for-the-origin-only"), "{connect:?}");

    // A tunnel the proxy cannot establish surfaces as a proxy error.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let req = Request::builder(Method::Get, &format!("https://127.0.0.1:{dead}/"))
        .unwrap()
        .build();
    let err = client.send(req).await.unwrap_err();
    assert!(
        matches!(&err, HttpError::Proxy(m) if m.contains("502")),
        "{err:?}"
    );
}
