//! Forward-proxy configuration for the HTTP client.

use super::form::percent_decode;
use super::transport::base64_std_encode;
use super::{HttpError, Uri};

/// One proxy server and the credentials to present to it.
#[derive(Clone)]
pub(crate) struct ProxyServer {
    /// Where the proxy listens (`http://` or `https://`).
    pub(crate) uri: Uri,
    /// A ready-made `Proxy-Authorization` header value.
    pub(crate) authorization: Option<String>,
}

impl ProxyServer {
    fn parse(url: &str) -> Result<Self, HttpError> {
        let url = url.trim();
        let (scheme, rest) = match url.split_once("://") {
            Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
            None => ("http".to_string(), url),
        };
        if scheme != "http" && scheme != "https" {
            return Err(HttpError::Proxy(format!(
                "unsupported proxy scheme: {scheme}"
            )));
        }
        // `Uri::parse` refuses userinfo, so lift credentials out first.
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        let (userinfo, host) = match authority.rsplit_once('@') {
            Some((userinfo, host)) => (Some(userinfo), host),
            None => (None, authority),
        };
        let uri = Uri::parse(&format!("{scheme}://{host}{tail}"))
            .map_err(|_| HttpError::Proxy("invalid proxy URL".into()))?;
        let authorization = match userinfo {
            Some(userinfo) => {
                let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
                let bad = |_| HttpError::Proxy("invalid proxy credentials".into());
                let user = percent_decode(user).map_err(bad)?;
                let pass = percent_decode(pass).map_err(bad)?;
                Some(basic(&user, &pass))
            }
            None => None,
        };
        Ok(Self { uri, authorization })
    }

    /// A stable identity for pooling connections made through this proxy.
    pub(crate) fn pool_id(&self) -> String {
        format!("{}://{}", self.uri.scheme, self.uri.host_port())
    }
}

fn basic(user: &[u8], pass: &[u8]) -> String {
    let mut raw = Vec::with_capacity(user.len() + pass.len() + 1);
    raw.extend_from_slice(user);
    raw.push(b':');
    raw.extend_from_slice(pass);
    format!("Basic {}", base64_std_encode(&raw))
}

/// One `NO_PROXY` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Bypass {
    /// `*`: bypass the proxy for every host.
    All,
    /// A host name (matching itself and its subdomains) or an IP literal
    /// (matching exactly), optionally restricted to one port.
    Host(String, Option<u16>),
}

fn parse_bypass(list: &str) -> Vec<Bypass> {
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            if entry == "*" {
                return Bypass::All;
            }
            let (host, port) = if let Some(rest) = entry.strip_prefix('[') {
                // `[v6]` or `[v6]:port`
                match rest.split_once(']') {
                    Some((addr, after)) => (
                        addr,
                        after.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()),
                    ),
                    None => (rest, None),
                }
            } else {
                match entry.rsplit_once(':') {
                    Some((host, port)) if !host.contains(':') => match port.parse::<u16>() {
                        Ok(port) => (host, Some(port)),
                        Err(_) => (entry, None),
                    },
                    _ => (entry, None),
                }
            };
            let host = host.trim_start_matches("*.").trim_start_matches('.');
            Bypass::Host(host.to_ascii_lowercase(), port)
        })
        .collect()
}

/// How the client reaches a given target.
pub(crate) enum Route<'a> {
    /// Connect straight to the target.
    Direct,
    /// Send the request to the proxy with an absolute-form target (`http://`
    /// targets).
    Forward(&'a ProxyServer),
    /// Open a `CONNECT` tunnel through the proxy and speak TLS to the target
    /// inside it (`https://` targets).
    Tunnel(&'a ProxyServer),
}

/// Forward-proxy settings for a [`Client`](super::Client).
///
/// - `http://` targets are sent **to** the proxy with an absolute-form request
///   target (`GET http://host/path HTTP/1.1`);
/// - `https://` targets go **through** the proxy in a `CONNECT host:port`
///   tunnel, with TLS negotiated end to end inside it — the proxy never sees
///   the request.
///
/// The proxy itself may be reached over plain HTTP (`http://proxy:3128`) or
/// TLS (`https://proxy:3129`). Credentials are sent as
/// `Proxy-Authorization: Basic …` to the proxy only, never to the target.
///
/// A proxy is used only when configured: explicitly, or from the environment
/// with the opt-in [`Proxy::from_env`]. [`Client::new`](super::Client::new)
/// never reads the environment.
///
/// ```
/// use ferroly::http::{Client, Proxy};
///
/// # fn ex() -> Result<(), ferroly::http::HttpError> {
/// let proxy = Proxy::all("http://proxy.internal:3128")?
///     .basic_auth("svc", "s3cret")
///     .no_proxy("localhost,127.0.0.1,.internal");
/// let client = Client::builder().proxy(proxy).build()?;
/// # let _ = client;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Proxy {
    http: Option<ProxyServer>,
    https: Option<ProxyServer>,
    bypass: Vec<Bypass>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never prints credentials.
        let show = |p: &Option<ProxyServer>| p.as_ref().map(ProxyServer::pool_id);
        f.debug_struct("Proxy")
            .field("http", &show(&self.http))
            .field("https", &show(&self.https))
            .field("no_proxy", &self.bypass)
            .finish()
    }
}

impl Proxy {
    /// Uses the proxy at `url` for both `http://` and `https://` targets.
    ///
    /// `url` is `http://host:port` or `https://host:port` (a bare `host:port`
    /// means `http://`). Credentials may be embedded as `user:password@`
    /// (percent-encoded) or supplied with [`basic_auth`](Self::basic_auth).
    pub fn all(url: &str) -> Result<Self, HttpError> {
        let server = ProxyServer::parse(url)?;
        Ok(Self {
            http: Some(server.clone()),
            https: Some(server),
            bypass: Vec::new(),
        })
    }

    /// Uses the proxy at `url` for `http://` targets only.
    pub fn http(url: &str) -> Result<Self, HttpError> {
        Ok(Self {
            http: Some(ProxyServer::parse(url)?),
            https: None,
            bypass: Vec::new(),
        })
    }

    /// Uses the proxy at `url` for `https://` targets only (via `CONNECT`).
    pub fn https(url: &str) -> Result<Self, HttpError> {
        Ok(Self {
            http: None,
            https: Some(ProxyServer::parse(url)?),
            bypass: Vec::new(),
        })
    }

    /// Also (or instead) uses the proxy at `url` for `http://` targets.
    pub fn with_http(mut self, url: &str) -> Result<Self, HttpError> {
        self.http = Some(ProxyServer::parse(url)?);
        Ok(self)
    }

    /// Also (or instead) uses the proxy at `url` for `https://` targets.
    pub fn with_https(mut self, url: &str) -> Result<Self, HttpError> {
        self.https = Some(ProxyServer::parse(url)?);
        Ok(self)
    }

    /// Sends `Proxy-Authorization: Basic …` with these credentials to every
    /// proxy configured so far (replacing credentials taken from a URL).
    pub fn basic_auth(mut self, user: &str, password: &str) -> Self {
        let value = basic(user.as_bytes(), password.as_bytes());
        for server in [&mut self.http, &mut self.https].into_iter().flatten() {
            server.authorization = Some(value.clone());
        }
        self
    }

    /// Sets the hosts that bypass the proxy, in `NO_PROXY` syntax: a
    /// comma-separated list where `*` matches everything, `example.com` (or
    /// `.example.com`) matches that host and its subdomains, an IP literal
    /// matches exactly, and an entry may end in `:port` to match one port
    /// only. CIDR ranges are not supported.
    pub fn no_proxy(mut self, list: &str) -> Self {
        self.bypass = parse_bypass(list);
        self
    }

    /// Reads the conventional proxy environment variables: `http_proxy` /
    /// `HTTP_PROXY` for `http://` targets, `https_proxy` / `HTTPS_PROXY` for
    /// `https://` targets, `all_proxy` / `ALL_PROXY` as the fallback for both,
    /// and `no_proxy` / `NO_PROXY` for the bypass list. The lower-case name
    /// wins when both are set.
    ///
    /// Returns `None` when no proxy variable is set (or none parses).
    ///
    /// The upper-case `HTTP_PROXY` is ignored when `REQUEST_METHOD` is set:
    /// in a CGI environment that variable is controlled by the remote client
    /// through the `Proxy:` request header ("httpoxy").
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Like [`from_env`](Self::from_env), but reads the variables through
    /// `lookup` instead of the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let cgi = get("REQUEST_METHOD").is_some();
        let all = get("all_proxy").or_else(|| get("ALL_PROXY"));
        let http = get("http_proxy")
            .or_else(|| if cgi { None } else { get("HTTP_PROXY") })
            .or_else(|| all.clone());
        let https = get("https_proxy").or_else(|| get("HTTPS_PROXY")).or(all);
        let http = http.and_then(|url| ProxyServer::parse(&url).ok());
        let https = https.and_then(|url| ProxyServer::parse(&url).ok());
        if http.is_none() && https.is_none() {
            return None;
        }
        let bypass = get("no_proxy")
            .or_else(|| get("NO_PROXY"))
            .map(|list| parse_bypass(&list))
            .unwrap_or_default();
        Some(Self {
            http,
            https,
            bypass,
        })
    }

    fn bypassed(&self, target: &Uri) -> bool {
        let host = target.host.to_ascii_lowercase();
        self.bypass.iter().any(|rule| match rule {
            Bypass::All => true,
            Bypass::Host(pattern, port) => {
                let port_ok = port.map(|p| p == target.port).unwrap_or(true);
                let host_ok = host == *pattern
                    || (host.len() > pattern.len()
                        && host.ends_with(pattern.as_str())
                        && host.as_bytes()[host.len() - pattern.len() - 1] == b'.'
                        && host.parse::<std::net::IpAddr>().is_err());
                port_ok && host_ok
            }
        })
    }

    /// Decides how to reach `target`.
    pub(crate) fn route(&self, target: &Uri) -> Route<'_> {
        if self.bypassed(target) {
            return Route::Direct;
        }
        let server = if target.is_tls() {
            &self.https
        } else {
            &self.http
        };
        match server {
            Some(server) if target.is_tls() => Route::Tunnel(server),
            Some(server) => Route::Forward(server),
            None => Route::Direct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(route: Route<'_>) -> &'static str {
        match route {
            Route::Direct => "direct",
            Route::Forward(_) => "forward",
            Route::Tunnel(_) => "tunnel",
        }
    }

    fn uri(s: &str) -> Uri {
        Uri::parse(s).unwrap()
    }

    #[test]
    fn parses_proxy_urls_and_credentials() {
        let p = ProxyServer::parse("http://proxy.local:3128").unwrap();
        assert_eq!(p.uri.host, "proxy.local");
        assert_eq!(p.uri.port, 3128);
        assert!(p.authorization.is_none());

        let p = ProxyServer::parse("proxy.local:8080").unwrap();
        assert_eq!(p.uri.scheme, "http");
        assert_eq!(p.uri.port, 8080);

        let p = ProxyServer::parse("https://us%40er:p%3Ass@proxy.local/").unwrap();
        assert!(p.uri.is_tls());
        assert_eq!(p.uri.port, 443);
        // base64("us@er:p:ss")
        assert_eq!(p.authorization.as_deref(), Some("Basic dXNAZXI6cDpzcw=="));

        assert!(ProxyServer::parse("socks5://proxy.local:1080").is_err());
        assert!(ProxyServer::parse("http://bad host/").is_err());
        assert!(ProxyServer::parse("http://u:%zz@h/").is_err());
    }

    #[test]
    fn routes_by_scheme_and_bypass_list() {
        let p = Proxy::all("http://proxy:3128")
            .unwrap()
            .no_proxy("localhost, .internal.example, 10.0.0.5, svc.example:8443, [::1]");
        assert_eq!(kind(p.route(&uri("http://example.com/"))), "forward");
        assert_eq!(kind(p.route(&uri("https://example.com/"))), "tunnel");
        assert_eq!(kind(p.route(&uri("http://localhost:8080/"))), "direct");
        assert_eq!(kind(p.route(&uri("http://LOCALHOST/"))), "direct");
        assert_eq!(kind(p.route(&uri("http://a.internal.example/"))), "direct");
        assert_eq!(kind(p.route(&uri("http://internal.example/"))), "direct");
        assert_eq!(
            kind(p.route(&uri("http://notinternal.example/"))),
            "forward"
        );
        assert_eq!(kind(p.route(&uri("http://10.0.0.5/"))), "direct");
        assert_eq!(kind(p.route(&uri("http://110.0.0.5/"))), "forward");
        assert_eq!(kind(p.route(&uri("https://svc.example:8443/"))), "direct");
        assert_eq!(kind(p.route(&uri("https://svc.example/"))), "tunnel");
        assert_eq!(kind(p.route(&uri("http://[::1]:9/"))), "direct");

        let only_http = Proxy::http("http://proxy:3128").unwrap();
        assert_eq!(kind(only_http.route(&uri("http://x/"))), "forward");
        assert_eq!(kind(only_http.route(&uri("https://x/"))), "direct");
        let only_https = Proxy::https("http://proxy:3128").unwrap();
        assert_eq!(kind(only_https.route(&uri("http://x/"))), "direct");
        assert_eq!(kind(only_https.route(&uri("https://x/"))), "tunnel");

        let none = Proxy::all("http://proxy:3128").unwrap().no_proxy("*");
        assert_eq!(kind(none.route(&uri("https://x/"))), "direct");
    }

    #[test]
    fn basic_auth_applies_to_every_proxy_and_stays_out_of_debug() {
        let p = Proxy::all("http://proxy:3128")
            .unwrap()
            .basic_auth("user", "pass");
        let expect = Some("Basic dXNlcjpwYXNz");
        assert_eq!(p.http.as_ref().unwrap().authorization.as_deref(), expect);
        assert_eq!(p.https.as_ref().unwrap().authorization.as_deref(), expect);
        let debug = format!("{p:?}");
        assert!(!debug.contains("dXNlcjpwYXNz") && !debug.contains("pass"));
    }

    #[test]
    fn reads_environment_style_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(Proxy::from_lookup(env(&[])).is_none());
        assert!(Proxy::from_lookup(env(&[("NO_PROXY", "*")])).is_none());

        let p = Proxy::from_lookup(env(&[
            ("HTTP_PROXY", "http://upper:1"),
            ("http_proxy", "http://lower:2"),
            ("HTTPS_PROXY", "http://secure:3"),
            ("NO_PROXY", "skip.example"),
        ]))
        .unwrap();
        assert_eq!(p.http.as_ref().unwrap().uri.host, "lower");
        assert_eq!(p.https.as_ref().unwrap().uri.host, "secure");
        assert_eq!(kind(p.route(&uri("http://skip.example/"))), "direct");

        let p = Proxy::from_lookup(env(&[("ALL_PROXY", "http://all:9")])).unwrap();
        assert_eq!(p.http.as_ref().unwrap().uri.host, "all");
        assert_eq!(p.https.as_ref().unwrap().uri.host, "all");

        // httpoxy: HTTP_PROXY is attacker-controlled under CGI.
        let p = Proxy::from_lookup(env(&[
            ("REQUEST_METHOD", "GET"),
            ("HTTP_PROXY", "http://evil:1"),
        ]));
        assert!(p.is_none());
        let p = Proxy::from_lookup(env(&[
            ("REQUEST_METHOD", "GET"),
            ("HTTP_PROXY", "http://evil:1"),
            ("https_proxy", "http://ok:2"),
        ]))
        .unwrap();
        assert!(p.http.is_none());
        assert_eq!(p.https.as_ref().unwrap().uri.host, "ok");
    }
}
