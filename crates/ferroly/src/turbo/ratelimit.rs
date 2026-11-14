//! Client-address resolution and token-bucket rate limiting.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroly::http::{HttpResponse, StatusCode};

use super::ctx::Ctx;
use super::error::ParamError;
use super::router::{Group, Router};

/// Which forwarding header a trusted reverse proxy maintains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardedHeader {
    /// `X-Forwarded-For: client, proxy1, proxy2` (the default).
    XForwardedFor,
    /// RFC 7239 `Forwarded: for=client, for=proxy1`.
    Forwarded,
}

/// The reverse proxies whose forwarding headers may be believed.
///
/// By default **nothing is trusted**: the client address is the address of
/// the TCP peer ([`Ctx::peer_addr`]), and `X-Forwarded-For` / `Forwarded` are
/// ignored — they are ordinary request headers that any client can set to
/// anything.
///
/// When the server runs behind load balancers or reverse proxies, list their
/// addresses here. The client address is then found by walking the
/// forwarding header **from the right** (the entries appended by the proxies
/// closest to this server) and taking the first address that is not itself a
/// trusted proxy. The header is consulted only when the direct peer is
/// trusted, and entries to the left of the first untrusted address — which
/// the client could have forged — are never used.
///
/// ```
/// use ferroly::turbo::{ForwardedHeader, TrustedProxies};
///
/// // The load balancers live in 10.0.0.0/8 and append to X-Forwarded-For.
/// let proxies = TrustedProxies::from_cidrs(["10.0.0.0/8", "127.0.0.1"]).unwrap();
/// # let _ = proxies.clone();
/// // A proxy that writes the RFC 7239 header instead:
/// let proxies = proxies.header(ForwardedHeader::Forwarded);
/// # let _ = proxies;
/// ```
///
/// Name the header your proxy actually sets, and make sure the proxy
/// overwrites or appends to it on every request: a header the proxy passes
/// through untouched is still under the client's control.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct TrustedProxies {
    nets: Vec<(IpAddr, u8)>,
    header: ForwardedHeader,
}

impl Default for TrustedProxies {
    fn default() -> Self {
        TrustedProxies::none()
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
            let bits = u32::from(prefix.min(32));
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            let bits = u32::from(prefix.min(128));
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
    }
}

impl TrustedProxies {
    /// Trusts no proxy: forwarding headers are ignored.
    pub fn none() -> Self {
        TrustedProxies {
            nets: Vec::new(),
            header: ForwardedHeader::XForwardedFor,
        }
    }

    /// Trusts the given addresses and CIDR ranges (`"10.0.0.0/8"`,
    /// `"192.0.2.7"`, `"fd00::/8"`).
    pub fn from_cidrs<I, S>(entries: I) -> Result<Self, ParamError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut proxies = TrustedProxies::none();
        for entry in entries {
            let entry = entry.as_ref().trim();
            let invalid = |reason: &str| ParamError::Invalid {
                name: entry.to_string(),
                reason: reason.to_string(),
            };
            let (addr, prefix) = match entry.split_once('/') {
                Some((addr, prefix)) => (addr, Some(prefix)),
                None => (entry, None),
            };
            let ip: IpAddr = addr
                .parse()
                .map_err(|_| invalid("not an IP address or CIDR range"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let prefix = match prefix {
                Some(p) => p
                    .parse::<u8>()
                    .ok()
                    .filter(|p| *p <= max)
                    .ok_or_else(|| invalid("invalid prefix length"))?,
                None => max,
            };
            proxies.nets.push((ip, prefix));
        }
        Ok(proxies)
    }

    /// Also trusts one address.
    pub fn trust_ip(mut self, ip: IpAddr) -> Self {
        let prefix = if ip.is_ipv4() { 32 } else { 128 };
        self.nets.push((ip, prefix));
        self
    }

    /// Selects the forwarding header the proxies maintain (default
    /// `X-Forwarded-For`).
    pub fn header(mut self, header: ForwardedHeader) -> Self {
        self.header = header;
        self
    }

    /// Whether `ip` is one of the trusted proxies.
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.nets
            .iter()
            .any(|(net, prefix)| in_net(ip, *net, *prefix))
    }

    /// The address of the client that originated the request: the TCP peer,
    /// or — when the peer is a trusted proxy — the nearest address in the
    /// forwarding header that is not a trusted proxy. `None` only for a
    /// hand-built request without a peer address.
    pub fn client_ip(&self, ctx: &Ctx) -> Option<IpAddr> {
        let peer = canonical(ctx.peer_addr()?.ip());
        if !self.is_trusted(peer) {
            return Some(peer);
        }
        let chain: Vec<Option<IpAddr>> = match self.header {
            ForwardedHeader::XForwardedFor => ctx
                .header_all("x-forwarded-for")
                .flat_map(|value| value.split(','))
                .map(parse_node)
                .collect(),
            ForwardedHeader::Forwarded => ctx
                .header_all("forwarded")
                .flat_map(|value| value.split(','))
                .map(forwarded_for)
                .collect(),
        };
        let mut client = peer;
        for hop in chain.into_iter().rev() {
            match hop {
                Some(ip) => {
                    client = ip;
                    if !self.is_trusted(ip) {
                        break;
                    }
                }
                // An entry that is not an address (`unknown`, an obfuscated
                // identifier): nothing further left can be relied on.
                None => break,
            }
        }
        Some(client)
    }
}

/// Parses one forwarding-chain node: an IP address, optionally quoted, with
/// IPv6 in brackets and an optional port.
fn parse_node(node: &str) -> Option<IpAddr> {
    let node = node.trim().trim_matches('"');
    if let Ok(ip) = node.parse::<IpAddr>() {
        return Some(canonical(ip));
    }
    if let Ok(addr) = node.parse::<SocketAddr>() {
        return Some(canonical(addr.ip()));
    }
    let inner = node.strip_prefix('[')?.strip_suffix(']')?;
    inner
        .parse::<Ipv6Addr>()
        .ok()
        .map(|ip| canonical(ip.into()))
}

/// The `for=` node of one `Forwarded` element.
fn forwarded_for(element: &str) -> Option<IpAddr> {
    element.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("for")
            .then(|| parse_node(value))
            .flatten()
    })
}

impl Ctx {
    /// The originating client address according to `proxies`; see
    /// [`TrustedProxies::client_ip`].
    pub fn client_ip(&self, proxies: &TrustedProxies) -> Option<IpAddr> {
        proxies.client_ip(self)
    }
}

type KeyFn = Arc<dyn Fn(&Ctx) -> String + Send + Sync>;

/// A token-bucket rate limit; install with [`Router::rate_limit_with`],
/// [`Group::rate_limit_with`], or use [`into_filter`](Self::into_filter) on a
/// single [`Route`](super::Route).
///
/// Each key has a bucket of `burst` tokens that refills at `per_second`; a
/// request takes one token, and a request that finds none is answered `429`.
///
/// **Key.** By default requests are keyed by client address: the TCP peer
/// address, or the address derived from forwarding headers when
/// [`trusted_proxies`](Self::trusted_proxies) is configured (never
/// otherwise). An IPv6 client is keyed by its `/64` network, since one host
/// typically controls a whole `/64`. Supply [`key_fn`](Self::key_fn) to key
/// by something else, such as an API key or the authenticated user.
///
/// **Memory.** At most [`max_keys`](Self::max_keys) buckets are kept (10 000
/// by default). When the table is full, idle buckets (fully refilled) are
/// dropped first; if none are idle, the tenth least recently used are
/// evicted. Evicting an active bucket hands that client a fresh burst, which
/// is the price of a hard memory bound — size `max_keys` for your traffic.
///
/// **Headers.** Responses carry `RateLimit-Limit` (the burst),
/// `RateLimit-Remaining` (tokens left) and `RateLimit-Reset` (seconds until
/// the bucket is full again); a `429` also carries `Retry-After` (seconds
/// until one token is available). Turn them off with
/// [`headers(false)`](Self::headers).
///
/// The limiter lives in this process: several instances of a service each
/// keep their own counts.
///
/// ```
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{RateLimit, Router, TrustedProxies};
///
/// // 5 requests per second with bursts of 20, per client address, behind
/// // load balancers in 10.0.0.0/8.
/// let limit = RateLimit::new(5.0, 20.0)
///     .trusted_proxies(TrustedProxies::from_cidrs(["10.0.0.0/8"]).unwrap());
///
/// let _router = Router::new()
///     .rate_limit_with(limit)
///     .get("/", |_| async { HttpResponse::text(StatusCode::OK, "ok") });
/// ```
#[derive(Clone)]
#[must_use]
pub struct RateLimit {
    per_second: f64,
    burst: f64,
    key: Option<KeyFn>,
    proxies: TrustedProxies,
    max_keys: usize,
    headers: bool,
}

impl std::fmt::Debug for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimit")
            .field("per_second", &self.per_second)
            .field("burst", &self.burst)
            .field("custom_key", &self.key.is_some())
            .field("proxies", &self.proxies)
            .field("max_keys", &self.max_keys)
            .field("headers", &self.headers)
            .finish()
    }
}

/// What the limiter decided for one request.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Decision {
    allowed: bool,
    remaining: u64,
    /// Seconds until the bucket is full; `None` when it never refills.
    reset: Option<u64>,
    /// Seconds until one token is available; `None` when it never refills.
    retry_after: Option<u64>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

struct Limiter {
    per_second: f64,
    burst: f64,
    max_keys: usize,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    buckets: HashMap<String, Bucket>,
    last_sweep: Instant,
}

/// How often idle buckets are swept even when the table is not full.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

impl Limiter {
    fn refilled(&self, bucket: &Bucket, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        (bucket.tokens + elapsed * self.per_second).min(self.burst)
    }

    fn check(&self, key: String, now: Instant) -> Decision {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Dropping a fully refilled bucket is lossless: a returning client is
        // re-inserted with an identical full bucket.
        let due = now.saturating_duration_since(state.last_sweep) >= SWEEP_INTERVAL;
        let full = state.buckets.len() >= self.max_keys && !state.buckets.contains_key(&key);
        if (due || full) && self.per_second > 0.0 {
            state.last_sweep = now;
            let burst = self.burst;
            let rate = self.per_second;
            state.buckets.retain(|_, b| {
                let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
                b.tokens + elapsed * rate < burst
            });
        }
        if state.buckets.len() >= self.max_keys && !state.buckets.contains_key(&key) {
            // Still full: evict the least recently used tenth.
            let mut ages: Vec<(Instant, String)> = state
                .buckets
                .iter()
                .map(|(k, b)| (b.last, k.clone()))
                .collect();
            ages.sort_by_key(|(last, _)| *last);
            let evict = (self.max_keys / 10).max(1);
            for (_, stale) in ages.into_iter().take(evict) {
                state.buckets.remove(&stale);
            }
        }

        let burst = self.burst;
        let bucket = state.buckets.entry(key).or_insert(Bucket {
            tokens: burst,
            last: now,
        });
        bucket.tokens = self.refilled(bucket, now);
        bucket.last = now;
        let allowed = bucket.tokens >= 1.0;
        if allowed {
            bucket.tokens -= 1.0;
        }
        let tokens = bucket.tokens;
        let seconds = |missing: f64| -> Option<u64> {
            if missing <= 0.0 {
                Some(0)
            } else if self.per_second > 0.0 {
                Some((missing / self.per_second).ceil() as u64)
            } else {
                None
            }
        };
        Decision {
            allowed,
            remaining: tokens.max(0.0).floor() as u64,
            reset: seconds(self.burst - tokens),
            retry_after: seconds(1.0 - tokens),
        }
    }
}

impl RateLimit {
    /// A limit of `per_second` requests per second with bursts of up to
    /// `burst`, per key. A `burst` below 1 is raised to 1.
    pub fn new(per_second: f64, burst: f64) -> Self {
        RateLimit {
            per_second: if per_second.is_finite() {
                per_second.max(0.0)
            } else {
                0.0
            },
            burst: if burst.is_finite() {
                burst.max(1.0)
            } else {
                1.0
            },
            key: None,
            proxies: TrustedProxies::none(),
            max_keys: 10_000,
            headers: true,
        }
    }

    /// Keys requests by the string `key_of` returns instead of by client
    /// address.
    pub fn key_fn<K>(mut self, key_of: K) -> Self
    where
        K: Fn(&Ctx) -> String + Send + Sync + 'static,
    {
        self.key = Some(Arc::new(key_of));
        self
    }

    /// Believes the forwarding headers written by these proxies when
    /// working out the client address; see [`TrustedProxies`]. Without this
    /// call no forwarding header is ever read.
    pub fn trusted_proxies(mut self, proxies: TrustedProxies) -> Self {
        self.proxies = proxies;
        self
    }

    /// Sets the maximum number of buckets kept (default 10 000, minimum 1).
    pub fn max_keys(mut self, max: usize) -> Self {
        self.max_keys = max.max(1);
        self
    }

    /// Whether to send the `RateLimit-*` and `Retry-After` headers (default
    /// `true`).
    pub fn headers(mut self, enabled: bool) -> Self {
        self.headers = enabled;
        self
    }

    /// The default key: the client address, with IPv6 reduced to its `/64`.
    fn address_key(proxies: &TrustedProxies, ctx: &Ctx) -> String {
        match proxies.client_ip(ctx) {
            Some(IpAddr::V4(v4)) => v4.to_string(),
            Some(IpAddr::V6(v6)) => {
                let network = u128::from(v6) & (u128::MAX << 64);
                format!("{}/64", Ipv6Addr::from(network))
            }
            None => "unknown".to_string(),
        }
    }

    /// Turns the limit into a filter, for [`Router::filter`],
    /// [`Group::filter`] or [`Route::filter`](super::Route::filter). Each
    /// call creates an independent set of buckets.
    pub fn into_filter(self) -> impl Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static {
        let limiter = Limiter {
            per_second: self.per_second,
            burst: self.burst,
            max_keys: self.max_keys,
            state: Mutex::new(LimiterState {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        };
        let limit = self.burst.floor() as u64;
        move |ctx: &Ctx| {
            let key = match &self.key {
                Some(key_of) => key_of(ctx),
                None => RateLimit::address_key(&self.proxies, ctx),
            };
            let decision = limiter.check(key, Instant::now());
            if self.headers {
                ctx.set_response_header("RateLimit-Limit", limit.to_string());
                ctx.set_response_header("RateLimit-Remaining", decision.remaining.to_string());
                if let Some(reset) = decision.reset {
                    ctx.set_response_header("RateLimit-Reset", reset.to_string());
                }
            }
            if decision.allowed {
                return None;
            }
            let mut resp =
                HttpResponse::text(StatusCode::TOO_MANY_REQUESTS, "429 Too Many Requests");
            if self.headers {
                if let Some(retry_after) = decision.retry_after {
                    resp.headers
                        .set("Retry-After", retry_after.max(1).to_string());
                }
            }
            Some(resp)
        }
    }
}

impl Router {
    /// Adds a token-bucket rate-limit filter keyed by `key_of`: each key gets
    /// `burst` tokens that refill at `per_second` per second, and over-limit
    /// requests short-circuit with `429`.
    ///
    /// Shorthand for
    /// `rate_limit_with(RateLimit::new(per_second, burst).key_fn(key_of))`.
    /// Do **not** derive the key from `X-Forwarded-For` or `Forwarded`
    /// yourself — any client can set those. Use
    /// [`rate_limit_with`](Self::rate_limit_with), whose default key is the
    /// peer address and which reads forwarding headers only from proxies you
    /// declare trusted.
    pub fn rate_limit<K>(self, per_second: f64, burst: f64, key_of: K) -> Self
    where
        K: Fn(&Ctx) -> String + Send + Sync + 'static,
    {
        self.rate_limit_with(RateLimit::new(per_second, burst).key_fn(key_of))
    }

    /// Adds a [`RateLimit`] as a filter (so it runs after the authenticator,
    /// in registration order with the other filters).
    pub fn rate_limit_with(self, limit: RateLimit) -> Self {
        self.filter(limit.into_filter())
    }
}

impl Group {
    /// Adds a [`RateLimit`] for requests under this group's prefix.
    pub fn rate_limit_with(self, limit: RateLimit) -> Self {
        self.filter(limit.into_filter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::http::{Method, Request, Uri};

    fn ctx_from(peer: &str, headers: &[(&str, &str)]) -> Ctx {
        let mut req = Request::new(Method::Get, Uri::parse("http://localhost/").unwrap());
        req.set_peer_addr(peer.parse().unwrap());
        for (k, v) in headers {
            req.headers.append(*k, *v);
        }
        Ctx::from_request(req).unwrap()
    }

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn untrusted_peer_headers_are_ignored() {
        let ctx = ctx_from("203.0.113.9:4000", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(TrustedProxies::none().client_ip(&ctx), ip("203.0.113.9"));
        // Even with proxies configured: this peer is not one of them.
        let proxies = TrustedProxies::from_cidrs(["10.0.0.0/8"]).unwrap();
        assert_eq!(proxies.client_ip(&ctx), ip("203.0.113.9"));
    }

    #[test]
    fn trusted_peer_walks_chain_from_the_right() {
        let proxies = TrustedProxies::from_cidrs(["10.0.0.0/8"]).unwrap();
        // The client forged the left-most entry; 198.51.100.7 is what the
        // outer proxy actually saw.
        let ctx = ctx_from(
            "10.0.0.2:1",
            &[("x-forwarded-for", "6.6.6.6, 198.51.100.7, 10.0.0.1")],
        );
        assert_eq!(proxies.client_ip(&ctx), ip("198.51.100.7"));

        // No header: the proxy itself.
        let ctx = ctx_from("10.0.0.2:1", &[]);
        assert_eq!(proxies.client_ip(&ctx), ip("10.0.0.2"));

        // Garbage stops the walk.
        let ctx = ctx_from("10.0.0.2:1", &[("x-forwarded-for", "1.2.3.4, unknown")]);
        assert_eq!(proxies.client_ip(&ctx), ip("10.0.0.2"));
    }

    #[test]
    fn forwarded_header_is_parsed() {
        let proxies = TrustedProxies::from_cidrs(["10.0.0.0/8"])
            .unwrap()
            .header(ForwardedHeader::Forwarded);
        let ctx = ctx_from(
            "10.0.0.2:1",
            &[(
                "forwarded",
                "for=6.6.6.6, For=\"[2001:db8::7]:4711\";proto=https, for=10.0.0.1",
            )],
        );
        assert_eq!(proxies.client_ip(&ctx), ip("2001:db8::7"));
        // X-Forwarded-For is not the configured header and is ignored.
        let ctx = ctx_from("10.0.0.2:1", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(proxies.client_ip(&ctx), ip("10.0.0.2"));
    }

    #[test]
    fn cidr_parsing() {
        assert!(TrustedProxies::from_cidrs(["10.0.0.0/33"]).is_err());
        assert!(TrustedProxies::from_cidrs(["nope"]).is_err());
        let p = TrustedProxies::from_cidrs(["192.168.0.0/16", "fd00::/8"]).unwrap();
        assert!(p.is_trusted("192.168.4.4".parse().unwrap()));
        assert!(!p.is_trusted("192.169.0.1".parse().unwrap()));
        assert!(p.is_trusted("fd12::1".parse().unwrap()));
        assert!(p.is_trusted("::ffff:192.168.1.1".parse().unwrap()));
    }

    fn limiter(per_second: f64, burst: f64, max_keys: usize) -> Limiter {
        Limiter {
            per_second,
            burst,
            max_keys,
            state: Mutex::new(LimiterState {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }

    #[test]
    fn bucket_refills_and_reports() {
        let l = limiter(2.0, 2.0, 100);
        let t0 = Instant::now();
        let d = l.check("k".into(), t0);
        assert!(d.allowed);
        assert_eq!((d.remaining, d.reset), (1, Some(1)));
        assert!(l.check("k".into(), t0).allowed);
        let denied = l.check("k".into(), t0);
        assert!(!denied.allowed);
        assert_eq!(denied.retry_after, Some(1));
        assert!(l.check("k".into(), t0 + Duration::from_millis(600)).allowed);
    }

    #[test]
    fn bucket_table_is_bounded() {
        // No refill, so nothing is ever idle: eviction must still bound it.
        let l = limiter(0.0, 1.0, 10);
        let t0 = Instant::now();
        for i in 0..1000 {
            l.check(format!("k{i}"), t0 + Duration::from_millis(i));
        }
        assert!(l.state.lock().unwrap().buckets.len() <= 10);

        // With refill, idle buckets are swept before active ones are evicted.
        let l = limiter(1000.0, 1.0, 10);
        for i in 0..10 {
            l.check(format!("k{i}"), t0);
        }
        l.check("new".into(), t0 + Duration::from_secs(1));
        assert_eq!(l.state.lock().unwrap().buckets.len(), 1);
    }

    #[test]
    fn default_key_groups_ipv6_by_network() {
        let none = TrustedProxies::none();
        let a = ctx_from("[2001:db8:1:2::1]:1", &[]);
        let b = ctx_from("[2001:db8:1:2:ffff::9]:1", &[]);
        let c = ctx_from("[2001:db8:1:3::1]:1", &[]);
        assert_eq!(
            RateLimit::address_key(&none, &a),
            RateLimit::address_key(&none, &b)
        );
        assert_ne!(
            RateLimit::address_key(&none, &a),
            RateLimit::address_key(&none, &c)
        );
        assert_eq!(
            RateLimit::address_key(&none, &ctx_from("192.0.2.1:9", &[])),
            "192.0.2.1"
        );
    }
}
