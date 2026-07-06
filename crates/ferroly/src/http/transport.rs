//! Connection transport: plain TCP or TLS (via `tokio-rustls`/`ring`), behind a
//! boxed I/O object so the rest of the stack is transport-agnostic.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use super::{HttpError, Uri};

/// A duplex byte stream (TCP or TLS) usable by the HTTP codec.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// A boxed connection — the single place TLS-vs-plaintext is erased.
pub type Conn = Box<dyn Io>;

/// Builds a shared rustls client config trusting the webpki root store.
///
/// The crypto provider is named **explicitly** (`ring`) rather than inferred
/// from the process default. This makes TLS setup deterministic even when
/// another dependency's feature unification pulls a second provider
/// (`aws-lc-rs`) into the build — the ambiguous default would otherwise panic
/// `ClientConfig::builder()` at runtime.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring provider supports the default TLS protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// A server-side TLS configuration (certificate chain + private key), kept
/// opaque so `rustls` does not leak into the public API. Build one with
/// [`from_pem`](TlsConfig::from_pem) or [`from_der`](TlsConfig::from_der) and
/// pass it to [`serve_tls`](super::serve_tls).
#[derive(Clone)]
pub struct TlsConfig(pub(crate) Arc<rustls::ServerConfig>);

impl TlsConfig {
    /// Builds a config from a DER-encoded certificate chain (leaf first) and a
    /// DER-encoded private key (PKCS#8, PKCS#1, or SEC1).
    pub fn from_der(cert_chain: Vec<Vec<u8>>, key: Vec<u8>) -> Result<Self, HttpError> {
        use rustls_pki_types::{CertificateDer, PrivateKeyDer};
        if cert_chain.is_empty() {
            return Err(HttpError::Tls("empty certificate chain".into()));
        }
        let certs: Vec<CertificateDer<'static>> =
            cert_chain.into_iter().map(CertificateDer::from).collect();
        let key = PrivateKeyDer::try_from(key).map_err(|e| HttpError::Tls(e.to_string()))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| HttpError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| HttpError::Tls(e.to_string()))?;
        Ok(Self(Arc::new(cfg)))
    }

    /// Builds a config from PEM-encoded certificate and key data. `cert_pem` may
    /// contain one or more `CERTIFICATE` blocks (leaf first); `key_pem` must
    /// contain a `PRIVATE KEY`, `RSA PRIVATE KEY`, or `EC PRIVATE KEY` block.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, HttpError> {
        let certs: Vec<Vec<u8>> = pem_blocks(cert_pem)
            .into_iter()
            .filter(|(label, _)| label == "CERTIFICATE")
            .map(|(_, der)| der)
            .collect();
        let key = pem_blocks(key_pem)
            .into_iter()
            .find(|(label, _)| {
                matches!(
                    label.as_str(),
                    "PRIVATE KEY" | "RSA PRIVATE KEY" | "EC PRIVATE KEY"
                )
            })
            .map(|(_, der)| der)
            .ok_or_else(|| HttpError::Tls("no private-key PEM block found".into()))?;
        Self::from_der(certs, key)
    }
}

/// Splits PEM input into `(label, der_bytes)` pairs (one per `-----BEGIN …-----`
/// block), decoding the standard-base64 body. Dependency-free.
pub(crate) fn pem_blocks(pem: &[u8]) -> Vec<(String, Vec<u8>)> {
    let text = String::from_utf8_lossy(pem);
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            let label = rest.trim_end_matches('-').trim().to_string();
            let mut body = String::new();
            for l in lines.by_ref() {
                let l = l.trim();
                if l.starts_with("-----END ") {
                    break;
                }
                body.push_str(l);
            }
            if let Some(der) = base64_std_decode(body.as_bytes()) {
                out.push((label, der));
            }
        }
    }
    out
}

/// Decodes standard (RFC 4648) base64, ignoring whitespace and `=` padding.
fn base64_std_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for &c in chunk {
            n = (n << 6) | val(c)?;
        }
        n <<= 6 * (4 - chunk.len());
        match chunk.len() {
            4 => {
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            3 => {
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
            }
            2 => out.push((n >> 16) as u8),
            _ => return None,
        }
    }
    Some(out)
}

/// Wraps an already-accepted TCP stream in a server-side TLS session.
pub(crate) async fn accept_tls(
    tcp: TcpStream,
    acceptor: &tokio_rustls::TlsAcceptor,
) -> Result<Conn, HttpError> {
    let stream = acceptor
        .accept(tcp)
        .await
        .map_err(|e| HttpError::Tls(e.to_string()))?;
    Ok(Box::new(stream))
}

/// Opens a connection to the URI's host, wrapping in TLS for `https`.
pub(crate) async fn connect(uri: &Uri, tls: &Arc<rustls::ClientConfig>) -> Result<Conn, HttpError> {
    let tcp = TcpStream::connect((uri.host.as_str(), uri.port)).await?;
    let _ = tcp.set_nodelay(true);
    if uri.is_tls() {
        let connector = tokio_rustls::TlsConnector::from(tls.clone());
        let server_name = rustls_pki_types::ServerName::try_from(uri.host.clone())
            .map_err(|e| HttpError::Tls(e.to_string()))?;
        let stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| HttpError::Tls(e.to_string()))?;
        Ok(Box::new(stream))
    } else {
        Ok(Box::new(tcp))
    }
}
