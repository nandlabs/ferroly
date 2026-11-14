//! Connection transport: plain TCP or TLS (via `tokio-rustls`/`ring`), behind a
//! boxed I/O object so the rest of the stack is transport-agnostic.
//!
//! All TLS configuration is expressed through this module's own types
//! ([`TlsConfig`], [`TlsConfigBuilder`], [`ClientTlsConfig`], [`TlsVersion`]);
//! no `rustls` type appears in the public API. Certificate verification can be
//! pointed at different roots but can never be switched off.

use std::sync::{Arc, OnceLock, RwLock};

use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use super::{HttpError, Uri};

/// A duplex byte stream (TCP or TLS) usable by the HTTP codec.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// A boxed connection — the single place TLS-vs-plaintext is erased.
pub type Conn = Box<dyn Io>;

/// A TLS protocol version, used to set the oldest version a client or server
/// will negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TlsVersion {
    /// TLS 1.2.
    Tls12,
    /// TLS 1.3.
    Tls13,
}

static TLS12_AND_UP: [&rustls::SupportedProtocolVersion; 2] =
    [&rustls::version::TLS13, &rustls::version::TLS12];
static TLS13_ONLY: [&rustls::SupportedProtocolVersion; 1] = [&rustls::version::TLS13];

fn protocol_versions(min: TlsVersion) -> &'static [&'static rustls::SupportedProtocolVersion] {
    match min {
        TlsVersion::Tls12 => &TLS12_AND_UP,
        TlsVersion::Tls13 => &TLS13_ONLY,
    }
}

/// The crypto provider is named **explicitly** (`ring`) rather than inferred
/// from the process default. This makes TLS setup deterministic even when
/// another dependency's feature unification pulls a second provider
/// (`aws-lc-rs`) into the build — the ambiguous default would otherwise panic
/// `ClientConfig::builder()` at runtime.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_err(e: impl std::fmt::Display) -> HttpError {
    HttpError::Tls(e.to_string())
}

/// Extracts every `CERTIFICATE` block from PEM input as DER; an error if
/// there is none.
fn pem_certs(pem: &[u8]) -> Result<Vec<Vec<u8>>, HttpError> {
    let certs: Vec<Vec<u8>> = pem_blocks(pem)
        .into_iter()
        .filter(|(label, _)| label == "CERTIFICATE")
        .map(|(_, der)| der)
        .collect();
    if certs.is_empty() {
        return Err(HttpError::Tls("no CERTIFICATE PEM block found".into()));
    }
    Ok(certs)
}

/// Extracts the first private-key block from PEM input as DER.
fn pem_key(pem: &[u8]) -> Result<Vec<u8>, HttpError> {
    pem_blocks(pem)
        .into_iter()
        .find(|(label, _)| {
            matches!(
                label.as_str(),
                "PRIVATE KEY" | "RSA PRIVATE KEY" | "EC PRIVATE KEY"
            )
        })
        .map(|(_, der)| der)
        .ok_or_else(|| HttpError::Tls("no private-key PEM block found".into()))
}

fn der_chain(chain: &[Vec<u8>]) -> Result<Vec<CertificateDer<'static>>, HttpError> {
    if chain.is_empty() {
        return Err(HttpError::Tls("empty certificate chain".into()));
    }
    Ok(chain
        .iter()
        .map(|der| CertificateDer::from(der.clone()))
        .collect())
}

fn der_key(key: &[u8]) -> Result<PrivateKeyDer<'static>, HttpError> {
    PrivateKeyDer::try_from(key.to_vec()).map_err(tls_err)
}

fn root_store(roots: &[Vec<u8>]) -> Result<rustls::RootCertStore, HttpError> {
    let mut store = rustls::RootCertStore::empty();
    for der in roots {
        store
            .add(CertificateDer::from(der.clone()))
            .map_err(tls_err)?;
    }
    Ok(store)
}

/// TLS settings for the HTTP client: which roots to trust, an optional client
/// certificate for mutual TLS, and the minimum protocol version.
///
/// The default trusts the bundled web PKI roots, presents no client
/// certificate and accepts TLS 1.2 and 1.3. Server certificates are always
/// verified (chain, validity and host name); there is no way to disable that.
///
/// ```no_run
/// use ferroly::http::{Client, ClientTlsConfig, TlsVersion};
///
/// # fn ex(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> Result<(), ferroly::http::HttpError> {
/// let tls = ClientTlsConfig::new()
///     .add_root_pem(ca_pem)?            // trust a private CA as well
///     .identity_pem(cert_pem, key_pem)? // present a client certificate
///     .min_version(TlsVersion::Tls13);
/// let client = Client::builder().tls(tls).build()?;
/// # let _ = client;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ClientTlsConfig {
    web_pki_roots: bool,
    extra_roots: Vec<Vec<u8>>,
    identity: Option<(Vec<Vec<u8>>, Vec<u8>)>,
    min_version: TlsVersion,
}

impl std::fmt::Debug for ClientTlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never prints key material.
        f.debug_struct("ClientTlsConfig")
            .field("web_pki_roots", &self.web_pki_roots)
            .field("extra_roots", &self.extra_roots.len())
            .field("identity", &self.identity.is_some())
            .field("min_version", &self.min_version)
            .finish()
    }
}

impl Default for ClientTlsConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientTlsConfig {
    /// The default settings: bundled web PKI roots, no client certificate,
    /// TLS 1.2 or newer.
    pub fn new() -> Self {
        Self {
            web_pki_roots: true,
            extra_roots: Vec::new(),
            identity: None,
            min_version: TlsVersion::Tls12,
        }
    }

    /// Trusts the CA certificate(s) in `pem` (one or more `CERTIFICATE`
    /// blocks) in addition to the roots already configured.
    pub fn add_root_pem(mut self, pem: &[u8]) -> Result<Self, HttpError> {
        self.extra_roots.extend(pem_certs(pem)?);
        Ok(self)
    }

    /// Trusts one DER-encoded CA certificate in addition to the roots already
    /// configured. The certificate is checked by [`Client::builder`]'s
    /// `build`.
    ///
    /// [`Client::builder`]: super::Client::builder
    pub fn add_root_der(mut self, der: Vec<u8>) -> Self {
        self.extra_roots.push(der);
        self
    }

    /// Stops trusting the bundled web PKI roots, so only the roots added with
    /// [`add_root_pem`](Self::add_root_pem) / [`add_root_der`](Self::add_root_der)
    /// are trusted — use this to pin a client to a private CA. Building a
    /// client with no trusted root at all is an error.
    pub fn without_web_pki_roots(mut self) -> Self {
        self.web_pki_roots = false;
        self
    }

    /// Presents a client certificate (mutual TLS). `cert_pem` holds the chain
    /// (leaf first); `key_pem` holds a `PRIVATE KEY`, `RSA PRIVATE KEY` or
    /// `EC PRIVATE KEY` block.
    pub fn identity_pem(mut self, cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, HttpError> {
        self.identity = Some((pem_certs(cert_pem)?, pem_key(key_pem)?));
        Ok(self)
    }

    /// Presents a client certificate from DER: the chain (leaf first) and a
    /// PKCS#8, PKCS#1 or SEC1 private key.
    pub fn identity_der(mut self, cert_chain: Vec<Vec<u8>>, key: Vec<u8>) -> Self {
        self.identity = Some((cert_chain, key));
        self
    }

    /// Sets the oldest TLS version the client will negotiate (default
    /// [`TlsVersion::Tls12`]).
    pub fn min_version(mut self, version: TlsVersion) -> Self {
        self.min_version = version;
        self
    }

    /// Builds the engine configuration.
    pub(crate) fn build(&self) -> Result<Arc<rustls::ClientConfig>, HttpError> {
        let mut roots = root_store(&self.extra_roots)?;
        if self.web_pki_roots {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        if roots.is_empty() {
            return Err(HttpError::Tls(
                "no trusted root certificates configured".into(),
            ));
        }
        let builder = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(protocol_versions(self.min_version))
            .map_err(tls_err)?
            .with_root_certificates(roots);
        let config = match &self.identity {
            Some((chain, key)) => builder
                .with_client_auth_cert(der_chain(chain)?, der_key(key)?)
                .map_err(tls_err)?,
            None => builder.with_no_client_auth(),
        };
        Ok(Arc::new(config))
    }
}

/// The shared default client TLS configuration (web PKI roots, TLS 1.2+),
/// built once per process.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    static DEFAULT: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            ClientTlsConfig::new()
                .build()
                .expect("the default TLS client configuration is valid")
        })
        .clone()
}

/// How a TLS server treats client certificates.
#[derive(Clone)]
enum ClientAuth {
    /// Clients are not asked for a certificate.
    None,
    /// A certificate is requested and, when presented, must chain to one of
    /// these CAs; clients without one are still accepted.
    Optional(Vec<Vec<u8>>),
    /// A certificate chaining to one of these CAs is mandatory.
    Required(Vec<Vec<u8>>),
}

/// A server-side TLS configuration (certificate chain + private key, plus
/// optional client-certificate verification and a minimum protocol version),
/// kept opaque so `rustls` does not leak into the public API. Build one with
/// [`from_pem`](TlsConfig::from_pem), [`from_der`](TlsConfig::from_der) or
/// [`builder`](TlsConfig::builder) and pass it to
/// [`serve_tls`](super::serve_tls).
#[derive(Clone)]
pub struct TlsConfig(pub(crate) Arc<rustls::ServerConfig>);

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsConfig { .. }")
    }
}

impl TlsConfig {
    /// Builds a config from a DER-encoded certificate chain (leaf first) and a
    /// DER-encoded private key (PKCS#8, PKCS#1, or SEC1).
    pub fn from_der(cert_chain: Vec<Vec<u8>>, key: Vec<u8>) -> Result<Self, HttpError> {
        Self::builder().cert_der(cert_chain, key).build()
    }

    /// Builds a config from PEM-encoded certificate and key data. `cert_pem` may
    /// contain one or more `CERTIFICATE` blocks (leaf first); `key_pem` must
    /// contain a `PRIVATE KEY`, `RSA PRIVATE KEY`, or `EC PRIVATE KEY` block.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, HttpError> {
        Self::builder().cert_pem(cert_pem, key_pem)?.build()
    }

    /// Starts a builder for a config with client-certificate verification
    /// (mutual TLS) and/or a minimum protocol version.
    ///
    /// ```no_run
    /// use ferroly::http::{TlsConfig, TlsVersion};
    ///
    /// # fn ex(cert: &[u8], key: &[u8], client_ca: &[u8]) -> Result<(), ferroly::http::HttpError> {
    /// let tls = TlsConfig::builder()
    ///     .cert_pem(cert, key)?
    ///     .client_auth_required_pem(client_ca)? // mutual TLS
    ///     .min_version(TlsVersion::Tls13)
    ///     .build()?;
    /// # let _ = tls;
    /// # Ok(())
    /// # }
    /// ```
    pub fn builder() -> TlsConfigBuilder {
        TlsConfigBuilder {
            cert_chain: Vec::new(),
            key: Vec::new(),
            client_auth: ClientAuth::None,
            min_version: TlsVersion::Tls12,
        }
    }

    /// Wraps this config in a handle whose certificate, key and settings can
    /// be replaced while the server runs; see [`ReloadableTlsConfig`].
    pub fn reloadable(self) -> ReloadableTlsConfig {
        ReloadableTlsConfig::new(self)
    }
}

/// Builder for a server [`TlsConfig`]; see [`TlsConfig::builder`].
#[derive(Clone)]
#[must_use]
pub struct TlsConfigBuilder {
    cert_chain: Vec<Vec<u8>>,
    key: Vec<u8>,
    client_auth: ClientAuth,
    min_version: TlsVersion,
}

impl std::fmt::Debug for TlsConfigBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never prints key material.
        let client_auth = match self.client_auth {
            ClientAuth::None => "none",
            ClientAuth::Optional(_) => "optional",
            ClientAuth::Required(_) => "required",
        };
        f.debug_struct("TlsConfigBuilder")
            .field("certificates", &self.cert_chain.len())
            .field("client_auth", &client_auth)
            .field("min_version", &self.min_version)
            .finish()
    }
}

impl TlsConfigBuilder {
    /// Sets the server certificate chain (one or more `CERTIFICATE` blocks,
    /// leaf first) and private key (`PRIVATE KEY`, `RSA PRIVATE KEY` or
    /// `EC PRIVATE KEY`) from PEM.
    pub fn cert_pem(mut self, cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, HttpError> {
        self.cert_chain = pem_certs(cert_pem)?;
        self.key = pem_key(key_pem)?;
        Ok(self)
    }

    /// Sets the server certificate chain (leaf first) and private key (PKCS#8,
    /// PKCS#1 or SEC1) from DER.
    pub fn cert_der(mut self, cert_chain: Vec<Vec<u8>>, key: Vec<u8>) -> Self {
        self.cert_chain = cert_chain;
        self.key = key;
        self
    }

    /// Requires every client to present a certificate that chains to one of
    /// the CA certificates in `ca_pem` (mutual TLS). A client without a valid
    /// certificate fails the handshake.
    pub fn client_auth_required_pem(mut self, ca_pem: &[u8]) -> Result<Self, HttpError> {
        self.client_auth = ClientAuth::Required(pem_certs(ca_pem)?);
        Ok(self)
    }

    /// Asks clients for a certificate without requiring one. A certificate
    /// that is presented must chain to one of the CA certificates in `ca_pem`;
    /// a client that presents none is still served, and
    /// [`Request::peer_certificates`](super::Request::peer_certificates) is
    /// empty for it.
    pub fn client_auth_optional_pem(mut self, ca_pem: &[u8]) -> Result<Self, HttpError> {
        self.client_auth = ClientAuth::Optional(pem_certs(ca_pem)?);
        Ok(self)
    }

    /// DER form of [`client_auth_required_pem`](Self::client_auth_required_pem).
    pub fn client_auth_required_der(mut self, ca_certs: Vec<Vec<u8>>) -> Self {
        self.client_auth = ClientAuth::Required(ca_certs);
        self
    }

    /// DER form of [`client_auth_optional_pem`](Self::client_auth_optional_pem).
    pub fn client_auth_optional_der(mut self, ca_certs: Vec<Vec<u8>>) -> Self {
        self.client_auth = ClientAuth::Optional(ca_certs);
        self
    }

    /// Sets the oldest TLS version the server will negotiate (default
    /// [`TlsVersion::Tls12`]).
    pub fn min_version(mut self, version: TlsVersion) -> Self {
        self.min_version = version;
        self
    }

    /// Validates the settings and builds the [`TlsConfig`].
    pub fn build(self) -> Result<TlsConfig, HttpError> {
        let certs = der_chain(&self.cert_chain)?;
        let key = der_key(&self.key)?;
        let builder = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(protocol_versions(self.min_version))
            .map_err(tls_err)?;
        let builder = match &self.client_auth {
            ClientAuth::None => builder.with_no_client_auth(),
            ClientAuth::Optional(cas) | ClientAuth::Required(cas) => {
                let roots = root_store(cas)?;
                if roots.is_empty() {
                    return Err(HttpError::Tls(
                        "client authentication needs at least one CA certificate".into(),
                    ));
                }
                let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::new(roots),
                    provider(),
                );
                let verifier = match self.client_auth {
                    ClientAuth::Optional(_) => verifier.allow_unauthenticated(),
                    _ => verifier,
                };
                builder.with_client_cert_verifier(verifier.build().map_err(tls_err)?)
            }
        };
        let config = builder.with_single_cert(certs, key).map_err(tls_err)?;
        Ok(TlsConfig(Arc::new(config)))
    }
}

/// A server TLS configuration that can be replaced at runtime — for example
/// to rotate a certificate without restarting.
///
/// Pass it to [`serve_tls_reloadable`](super::serve_tls_reloadable), keep a
/// clone, and call [`reload`](Self::reload) with a new [`TlsConfig`]. Every
/// connection accepted after the call uses the new configuration; connections
/// that are already established keep the one they started with.
///
/// ```no_run
/// use ferroly::http::TlsConfig;
///
/// # fn ex(cert: &[u8], key: &[u8], new_cert: &[u8], new_key: &[u8]) -> Result<(), ferroly::http::HttpError> {
/// let tls = TlsConfig::from_pem(cert, key)?.reloadable();
/// // ... serve_tls_reloadable(listener, handler, shutdown, config, tls.clone()) ...
/// tls.reload(TlsConfig::from_pem(new_cert, new_key)?);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct ReloadableTlsConfig {
    current: Arc<RwLock<TlsConfig>>,
}

impl ReloadableTlsConfig {
    /// A handle that starts out serving `initial`.
    pub fn new(initial: TlsConfig) -> Self {
        Self {
            current: Arc::new(RwLock::new(initial)),
        }
    }

    /// Replaces the configuration used for connections accepted from now on.
    pub fn reload(&self, next: TlsConfig) {
        let mut guard = self.current.write().unwrap_or_else(|e| e.into_inner());
        *guard = next;
    }

    /// The configuration new connections currently get.
    pub(crate) fn current(&self) -> TlsConfig {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

/// Encodes standard (RFC 4648) base64 with `=` padding.
pub(crate) fn base64_std_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Wraps an already-accepted TCP stream in a server-side TLS session. Returns
/// the connection and the client's verified certificate chain (DER, leaf
/// first; empty when the client presented none).
pub(crate) async fn accept_tls(
    tcp: TcpStream,
    config: &TlsConfig,
) -> Result<(Conn, Vec<Vec<u8>>), HttpError> {
    let acceptor = tokio_rustls::TlsAcceptor::from(config.0.clone());
    let stream = acceptor.accept(tcp).await.map_err(tls_err)?;
    let certs = stream
        .get_ref()
        .1
        .peer_certificates()
        .map(|chain| chain.iter().map(|c| c.as_ref().to_vec()).collect())
        .unwrap_or_default();
    Ok((Box::new(stream), certs))
}

/// Opens a TCP connection to `host:port` (`host` is a name or a bracket-free
/// IP literal).
pub(crate) async fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, HttpError> {
    let tcp = TcpStream::connect((host, port)).await?;
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

/// Runs a client TLS handshake over `stream`, verifying the peer as `host`.
pub(crate) async fn tls_handshake<S>(
    stream: S,
    host: &str,
    tls: &Arc<rustls::ClientConfig>,
) -> Result<Conn, HttpError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let connector = tokio_rustls::TlsConnector::from(tls.clone());
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string()).map_err(tls_err)?;
    let stream = connector
        .connect(server_name, stream)
        .await
        .map_err(tls_err)?;
    Ok(Box::new(stream))
}

/// Opens a connection to the URI's host, wrapping in TLS for `https`.
pub(crate) async fn connect(uri: &Uri, tls: &Arc<rustls::ClientConfig>) -> Result<Conn, HttpError> {
    let tcp = connect_tcp(&uri.host, uri.port).await?;
    if uri.is_tls() {
        tls_handshake(tcp, &uri.host, tls).await
    } else {
        Ok(Box::new(tcp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for (plain, encoded) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"user:pass", "dXNlcjpwYXNz"),
            (&[0xfb, 0xff, 0xfe], "+//+"),
        ] {
            assert_eq!(base64_std_encode(plain), encoded);
            assert_eq!(base64_std_decode(encoded.as_bytes()).unwrap(), plain);
        }
    }

    #[test]
    fn client_tls_config_validation() {
        assert!(ClientTlsConfig::new().build().is_ok());
        // Removing the bundled roots without adding any is refused.
        assert!(ClientTlsConfig::new()
            .without_web_pki_roots()
            .build()
            .is_err());
        assert!(ClientTlsConfig::new().add_root_pem(b"not pem").is_err());
        assert!(ClientTlsConfig::new()
            .add_root_der(b"not der".to_vec())
            .build()
            .is_err());
        let debug = format!("{:?}", ClientTlsConfig::new());
        assert!(debug.contains("web_pki_roots: true"));
    }
}
