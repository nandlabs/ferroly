//! The PROXY protocol (versions 1 and 2) on the server listener, so that
//! behind a layer-4 load balancer (HAProxy, AWS NLB, GCP, Envoy, …) the peer
//! address a handler sees is the real client's, not the balancer's.
//!
//! The header is read **only from trusted sources**: a connection whose
//! socket peer is in the allow-list must start with a PROXY header (v1 text or
//! v2 binary) before anything else, TLS included; a connection from anywhere
//! else is served as an ordinary direct client and its first bytes are never
//! parsed as a PROXY header, so an untrusted client cannot spoof its address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use super::HttpError;

/// The v2 binary signature.
const V2_SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// The longest v1 header, CRLF included (the spec's limit).
const V1_MAX: usize = 107;

/// PROXY-protocol settings for a server listener; set
/// [`ServerConfig::proxy_protocol`](super::ServerConfig::proxy_protocol).
///
/// Lists the networks the load balancers connect from. Connections from them
/// must begin with a PROXY v1 or v2 header, whose source address becomes
/// [`Request::peer_addr`](super::Request::peer_addr) (and the destination
/// [`Request::local_addr`](super::Request::local_addr)); a header that is
/// missing, malformed or late (past `head_timeout`) closes the connection and
/// is reported to the error hook. A v2 `LOCAL` header (a balancer health
/// check) or v1 `UNKNOWN` keeps the socket addresses. Connections from any
/// other address are served directly, without looking for a header.
///
/// ```
/// use ferroly::http::{ProxyProtocol, ServerConfig};
///
/// let config = ServerConfig {
///     proxy_protocol: Some(ProxyProtocol::from_cidrs(["10.0.0.0/8", "fd00::/8"]).unwrap()),
///     ..ServerConfig::default()
/// };
/// # let _ = config;
/// ```
#[derive(Debug, Clone, Default)]
pub struct ProxyProtocol {
    nets: Vec<(IpAddr, u8)>,
}

impl ProxyProtocol {
    /// Settings that trust no source yet; add networks with
    /// [`trust`](Self::trust) or [`trust_ip`](Self::trust_ip).
    pub fn new() -> Self {
        Self::default()
    }

    /// Trusts the given networks: CIDR blocks (`10.0.0.0/8`, `fd00::/8`) or
    /// single addresses (`127.0.0.1`, `::1`). `0.0.0.0/0` and `::/0` trust
    /// every source — only safe when nothing but the balancer can reach the
    /// port.
    pub fn from_cidrs<I, S>(entries: I) -> Result<Self, HttpError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        entries
            .into_iter()
            .try_fold(Self::new(), |pp, entry| pp.trust(entry.as_ref()))
    }

    /// Adds one network (see [`from_cidrs`](Self::from_cidrs)).
    pub fn trust(mut self, cidr: &str) -> Result<Self, HttpError> {
        let invalid = || HttpError::Protocol(format!("invalid trusted network {cidr:?}"));
        let (addr, prefix) = match cidr.trim().split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (cidr.trim(), None),
        };
        let ip: IpAddr = addr.parse().map_err(|_| invalid())?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(invalid)?,
            Some(_) => return Err(invalid()),
        };
        self.nets.push((canonical(ip), prefix));
        Ok(self)
    }

    /// Adds one trusted address.
    pub fn trust_ip(mut self, ip: IpAddr) -> Self {
        let ip = canonical(ip);
        let prefix = if ip.is_ipv4() { 32 } else { 128 };
        self.nets.push((ip, prefix));
        self
    }

    /// Whether connections from `ip` must carry a PROXY header.
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.nets
            .iter()
            .any(|(net, prefix)| in_net(ip, *net, *prefix))
    }
}

/// Maps an IPv4-mapped IPv6 address to its IPv4 form.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

fn in_net(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
    }
}

fn bad(reason: &str) -> HttpError {
    HttpError::Protocol(format!("PROXY protocol: {reason}"))
}

/// Reads the PROXY header at the start of `sock`, consuming exactly its
/// bytes. Returns the (source, destination) addresses it carries, or `None`
/// for a header that carries none (v2 `LOCAL`, v1 `UNKNOWN`, non-IP families).
pub(crate) async fn read_header(
    sock: &mut TcpStream,
) -> Result<Option<(SocketAddr, SocketAddr)>, HttpError> {
    // Both forms are at least 12 bytes long (v1's shortest is 15), so this
    // never reads into the protocol that follows.
    let mut start = [0u8; 12];
    sock.read_exact(&mut start).await?;
    if start == V2_SIGNATURE {
        let mut fixed = [0u8; 4];
        sock.read_exact(&mut fixed).await?;
        let len = u16::from_be_bytes([fixed[2], fixed[3]]) as usize;
        let mut rest = vec![0u8; len];
        sock.read_exact(&mut rest).await?;
        return parse_v2(fixed[0], fixed[1], &rest);
    }
    if !start.starts_with(b"PROXY ") {
        return Err(bad("the connection did not start with a PROXY header"));
    }
    let mut line = start.to_vec();
    // Peek to find the line end without reading past it; fall back to
    // consuming what has arrived when the line is split across segments.
    loop {
        let mut window = [0u8; V1_MAX];
        let room = V1_MAX - line.len();
        if room == 0 {
            return Err(bad("v1 header too long"));
        }
        let n = sock.peek(&mut window[..room]).await?;
        if n == 0 {
            return Err(bad("the connection closed inside the header"));
        }
        let peeked = &window[..n];
        // A CR at the end of what was already read may pair with an LF now.
        let joined_end = line.last() == Some(&b'\r') && peeked[0] == b'\n';
        let take = if joined_end {
            Some(1)
        } else {
            peeked.windows(2).position(|w| w == b"\r\n").map(|i| i + 2)
        };
        let take_now = take.unwrap_or(n);
        let mut chunk = vec![0u8; take_now];
        sock.read_exact(&mut chunk).await?;
        line.extend_from_slice(&chunk);
        if take.is_some() {
            break;
        }
    }
    parse_v1(&line)
}

/// Parses a complete v1 line (CRLF included).
pub(crate) fn parse_v1(line: &[u8]) -> Result<Option<(SocketAddr, SocketAddr)>, HttpError> {
    let text = line
        .strip_suffix(b"\r\n")
        .and_then(|l| std::str::from_utf8(l).ok())
        .ok_or_else(|| bad("malformed v1 header"))?;
    if text.len() + 2 > V1_MAX {
        return Err(bad("v1 header too long"));
    }
    let parts: Vec<&str> = text.split(' ').collect();
    match parts.as_slice() {
        ["PROXY", "UNKNOWN", ..] => Ok(None),
        ["PROXY", family @ ("TCP4" | "TCP6"), src, dst, sport, dport] => {
            let ip = |s: &str| -> Result<IpAddr, HttpError> {
                let ip = if *family == "TCP4" {
                    s.parse::<Ipv4Addr>().map(IpAddr::V4)
                } else {
                    s.parse::<Ipv6Addr>().map(IpAddr::V6)
                };
                ip.map_err(|_| bad("invalid v1 address"))
            };
            let port = |s: &str| -> Result<u16, HttpError> {
                let digits = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
                let leading_zero = s.len() > 1 && s.starts_with('0');
                match s.parse::<u16>() {
                    Ok(p) if digits && !leading_zero => Ok(p),
                    _ => Err(bad("invalid v1 port")),
                }
            };
            let source = SocketAddr::new(ip(src)?, port(sport)?);
            let destination = SocketAddr::new(ip(dst)?, port(dport)?);
            Ok(Some((source, destination)))
        }
        _ => Err(bad("malformed v1 header")),
    }
}

/// Parses the v2 header after its signature: the version/command byte, the
/// family byte and the address block (TLVs after the addresses are ignored).
pub(crate) fn parse_v2(
    version_command: u8,
    family: u8,
    rest: &[u8],
) -> Result<Option<(SocketAddr, SocketAddr)>, HttpError> {
    if version_command >> 4 != 2 {
        return Err(bad("unsupported v2 version"));
    }
    match version_command & 0x0F {
        // LOCAL: the balancer's own connection (health checks).
        0x0 => return Ok(None),
        0x1 => {}
        _ => return Err(bad("unsupported v2 command")),
    }
    match family >> 4 {
        // AF_INET
        0x1 => {
            if rest.len() < 12 {
                return Err(bad("v2 address block too short"));
            }
            let src = Ipv4Addr::new(rest[0], rest[1], rest[2], rest[3]);
            let dst = Ipv4Addr::new(rest[4], rest[5], rest[6], rest[7]);
            let sport = u16::from_be_bytes([rest[8], rest[9]]);
            let dport = u16::from_be_bytes([rest[10], rest[11]]);
            Ok(Some((
                SocketAddr::new(IpAddr::V4(src), sport),
                SocketAddr::new(IpAddr::V4(dst), dport),
            )))
        }
        // AF_INET6
        0x2 => {
            if rest.len() < 36 {
                return Err(bad("v2 address block too short"));
            }
            let mut src = [0u8; 16];
            let mut dst = [0u8; 16];
            src.copy_from_slice(&rest[..16]);
            dst.copy_from_slice(&rest[16..32]);
            let sport = u16::from_be_bytes([rest[32], rest[33]]);
            let dport = u16::from_be_bytes([rest[34], rest[35]]);
            Ok(Some((
                SocketAddr::new(IpAddr::V6(Ipv6Addr::from(src)), sport),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::from(dst)), dport),
            )))
        }
        // AF_UNSPEC, AF_UNIX: no IP addresses to report.
        0x0 | 0x3 => Ok(None),
        _ => Err(bad("unsupported v2 address family")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn v1_lines() {
        assert_eq!(
            parse_v1(b"PROXY TCP4 192.0.2.7 10.0.0.1 56324 443\r\n").unwrap(),
            Some((addr("192.0.2.7:56324"), addr("10.0.0.1:443")))
        );
        assert_eq!(
            parse_v1(b"PROXY TCP6 2001:db8::7 2001:db8::1 1 65535\r\n").unwrap(),
            Some((addr("[2001:db8::7]:1"), addr("[2001:db8::1]:65535")))
        );
        assert_eq!(parse_v1(b"PROXY UNKNOWN\r\n").unwrap(), None);
        assert_eq!(parse_v1(b"PROXY UNKNOWN ff 00 1 2\r\n").unwrap(), None);
        for bad_line in [
            &b"PROXY TCP4 192.0.2.7 10.0.0.1 56324 443\n"[..],
            b"PROXY TCP4 192.0.2.7 10.0.0.1 56324\r\n",
            b"PROXY TCP4  192.0.2.7 10.0.0.1 56324 443\r\n",
            b"PROXY TCP4 2001:db8::7 10.0.0.1 1 2\r\n",
            b"PROXY TCP6 192.0.2.7 2001:db8::1 1 2\r\n",
            b"PROXY TCP4 192.0.2.7 10.0.0.1 +1 443\r\n",
            b"PROXY TCP4 192.0.2.7 10.0.0.1 01 443\r\n",
            b"PROXY TCP4 192.0.2.7 10.0.0.1 1 65536\r\n",
            b"PROXY UDP4 192.0.2.7 10.0.0.1 1 2\r\n",
        ] {
            assert!(
                parse_v1(bad_line).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad_line)
            );
        }
    }

    #[test]
    fn v2_blocks() {
        let mut v4 = vec![192, 0, 2, 7, 10, 0, 0, 1];
        v4.extend_from_slice(&56324u16.to_be_bytes());
        v4.extend_from_slice(&443u16.to_be_bytes());
        // A TLV after the addresses is ignored.
        v4.extend_from_slice(&[0x04, 0x00, 0x01, 0xAA]);
        assert_eq!(
            parse_v2(0x21, 0x11, &v4).unwrap(),
            Some((addr("192.0.2.7:56324"), addr("10.0.0.1:443")))
        );
        let mut v6 = Ipv6Addr::LOCALHOST.octets().to_vec();
        v6.extend_from_slice(&Ipv6Addr::UNSPECIFIED.octets());
        v6.extend_from_slice(&[0, 80, 0, 81]);
        assert_eq!(
            parse_v2(0x21, 0x21, &v6).unwrap(),
            Some((addr("[::1]:80"), addr("[::]:81")))
        );
        assert_eq!(parse_v2(0x20, 0x00, &[]).unwrap(), None, "LOCAL");
        assert_eq!(parse_v2(0x21, 0x31, &[0; 216]).unwrap(), None, "AF_UNIX");
        assert!(
            parse_v2(0x11, 0x11, &v4).is_err(),
            "version 1 in a v2 header"
        );
        assert!(parse_v2(0x22, 0x11, &v4).is_err(), "unknown command");
        assert!(parse_v2(0x21, 0x11, &v4[..11]).is_err(), "short block");
        assert!(parse_v2(0x21, 0x21, &v6[..35]).is_err(), "short block");
        assert!(parse_v2(0x21, 0x41, &v4).is_err(), "unknown family");
    }

    #[test]
    fn trusted_networks() {
        let pp = ProxyProtocol::from_cidrs(["10.0.0.0/8", "127.0.0.1", "fd00::/8"]).unwrap();
        assert!(pp.is_trusted("10.1.2.3".parse().unwrap()));
        assert!(pp.is_trusted("127.0.0.1".parse().unwrap()));
        assert!(pp.is_trusted("::ffff:10.0.0.9".parse().unwrap()));
        assert!(pp.is_trusted("fd12::1".parse().unwrap()));
        assert!(!pp.is_trusted("11.0.0.1".parse().unwrap()));
        assert!(!pp.is_trusted("127.0.0.2".parse().unwrap()));
        assert!(!pp.is_trusted("fe80::1".parse().unwrap()));
        let all = ProxyProtocol::from_cidrs(["0.0.0.0/0", "::/0"]).unwrap();
        assert!(all.is_trusted("203.0.113.9".parse().unwrap()));
        assert!(all.is_trusted("2001:db8::1".parse().unwrap()));
        assert!(!ProxyProtocol::new().is_trusted("127.0.0.1".parse().unwrap()));
        assert!(ProxyProtocol::new()
            .trust_ip("::1".parse().unwrap())
            .is_trusted("::1".parse().unwrap()));
        for bad in [
            "10.0.0.0/33",
            "::/129",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "nope",
            "1.2.3.4/x",
        ] {
            assert!(ProxyProtocol::from_cidrs([bad]).is_err(), "{bad}");
        }
    }
}
