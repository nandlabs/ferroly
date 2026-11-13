#![cfg(feature = "turbo")]
//! Security regression tests for the router: request-path normalisation,
//! filter scoping (P0-4), parameter decoding, client-address handling, rate
//! limiting, CORS, security headers, request ids, CSRF and cookies.
//!
//! Requests whose *shape* matters are written to the socket by hand, so that
//! no client library can tidy them up first.

use std::net::SocketAddr;

use ferroly::http::{HttpResponse, Method, ServerConfig, StatusCode};
use ferroly::turbo::{
    Cors, FrameOptions, RateLimit, RouteError, Router, SameSite, SecurityHeaders, SetCookie,
    TrustedProxies,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn spawn(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = router
            .serve_listener(listener, ServerConfig::default(), std::future::pending())
            .await;
    });
    addr
}

/// A parsed raw response.
struct Raw {
    status: u16,
    head: String,
    body: String,
}

impl Raw {
    /// The first value of a header (case-insensitive name).
    fn header(&self, name: &str) -> Option<String> {
        self.headers(name).into_iter().next()
    }

    fn headers(&self, name: &str) -> Vec<String> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .collect()
    }
}

/// Sends `method target` exactly as written, with extra header lines, and
/// reads the whole response.
async fn raw(addr: SocketAddr, method: &str, target: &str, headers: &[(&str, &str)]) -> Raw {
    raw_body(addr, method, target, headers, "").await
}

async fn raw_body(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Raw {
    let mut request = format!("{method} {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    request.push_str(body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    Raw {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

async fn get(addr: SocketAddr, target: &str) -> Raw {
    raw(addr, "GET", target, &[]).await
}

fn ok(text: &'static str) -> HttpResponse {
    HttpResponse::text(StatusCode::OK, text)
}

/// A router whose `/api` group is protected by a key filter.
fn guarded() -> Router {
    Router::new()
        .get("/public", |_| async { ok("public") })
        .group("/api", |g| {
            g.filter(|ctx| {
                if ctx.header("x-key") == Some("k") {
                    None
                } else {
                    Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "no key"))
                }
            })
            .get("/secret", |_| async { ok("secret") })
        })
}

// ---- P0-4: group filters cannot be bypassed ---------------------------------------

#[tokio::test]
async fn group_filter_is_not_bypassed_by_path_spelling() {
    let addr = spawn(guarded()).await;

    // The plain request is refused without the key and served with it.
    assert_eq!(get(addr, "/api/secret").await.status, 401);
    let with_key = raw(addr, "GET", "/api/secret", &[("x-key", "k")]).await;
    assert_eq!((with_key.status, with_key.body.as_str()), (200, "secret"));

    // Targets that are not a canonical absolute path are refused outright.
    for target in [
        "//api/secret",   // empty leading segment
        "/api//secret",   // empty inner segment
        "api/secret",     // not absolute
        "/api/secret%00", // encoded control character
        "/api/sec%ZZret", // malformed escape
        "/api/%",         // truncated escape
    ] {
        let resp = get(addr, target).await;
        assert_eq!(resp.status, 400, "{target} -> {}", resp.status);
        assert_ne!(resp.body, "secret", "{target}");
    }

    // Targets that normalise to the protected route reach the filter, which
    // sees the same path the router does.
    for target in [
        "/api/../api/secret", // dot segments
        "/public/../api/secret",
        "/api/./secret",
        "/%61pi/secret",          // encoded unreserved character
        "/api/%2e%2e/api/secret", // encoded dot segments
        "/x/%2E%2E/api/secret",
        "/api/secret/", // trailing slash (lenient policy)
        "/api/secret?x=1",
    ] {
        let resp = get(addr, target).await;
        assert_eq!(
            resp.status, 401,
            "{target} -> {} {}",
            resp.status, resp.body
        );
        let resp = raw(addr, "GET", target, &[("x-key", "k")]).await;
        assert_eq!(resp.status, 200, "{target} with key");
        assert_eq!(resp.body, "secret", "{target} with key");
    }

    // Spellings that are *different* paths match neither the route nor the
    // group: never the protected body.
    for target in [
        "/API/secret", // paths are case-sensitive
        "/Api/Secret",
        "/api%2Fsecret", // an encoded slash is not a separator
        "/apix/secret",  // a prefix is whole segments
    ] {
        let resp = get(addr, target).await;
        assert_eq!(resp.status, 404, "{target} -> {}", resp.status);
        assert_ne!(resp.body, "secret", "{target}");
    }

    // A path under the prefix that matches no route still meets the filter
    // first: the group does not reveal which paths exist.
    assert_eq!(get(addr, "/api/nothing-here").await.status, 401);
    assert_eq!(
        raw(addr, "GET", "/api/nothing-here", &[("x-key", "k")])
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn group_with_a_parameter_prefix_runs_its_filter() {
    let router = Router::new().group("/t/:tenant", |g| {
        g.filter(|ctx| {
            // The filter sees the decoded parameter of the matched route.
            if ctx.param("tenant") == Some("blocked") {
                return Some(HttpResponse::text(StatusCode::FORBIDDEN, "blocked tenant"));
            }
            if ctx.header("x-key").is_none() {
                return Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "no key"));
            }
            None
        })
        .get("/data", |ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                format!("data of {}", ctx.param("tenant").unwrap_or("?")),
            )
        })
    });
    let addr = spawn(router).await;

    assert_eq!(get(addr, "/t/7/data").await.status, 401);
    assert_eq!(get(addr, "/t/acme/data").await.status, 401);
    let resp = raw(addr, "GET", "/t/7/data", &[("x-key", "k")]).await;
    assert_eq!((resp.status, resp.body.as_str()), (200, "data of 7"));
    assert_eq!(
        raw(addr, "GET", "/t/blocked/data", &[("x-key", "k")])
            .await
            .status,
        403
    );
    // Too short to be under the prefix: not the group's business.
    assert_eq!(get(addr, "/t").await.status, 404);
}

#[tokio::test]
async fn nested_and_mounted_filters_all_apply() {
    fn need(header: &'static str) -> impl Fn(&ferroly::turbo::Ctx) -> Option<HttpResponse> {
        move |ctx| {
            ctx.header(header)
                .is_none()
                .then(|| HttpResponse::text(StatusCode::UNAUTHORIZED, format!("missing {header}")))
        }
    }
    let admin = Router::new()
        .auth(need("x-admin"))
        .get("/stats", |_| async { ok("stats") });
    let router = Router::new()
        .group("/api", |api| {
            api.filter(need("x-outer")).group("/v1", |v1| {
                v1.filter(need("x-inner"))
                    .get("/item", |_| async { ok("item") })
            })
        })
        .mount("/admin", admin);
    let addr = spawn(router).await;

    let resp = get(addr, "/api/v1/item").await;
    assert_eq!((resp.status, resp.body.as_str()), (401, "missing x-outer"));
    let resp = raw(addr, "GET", "/api/v1/item", &[("x-outer", "1")]).await;
    assert_eq!((resp.status, resp.body.as_str()), (401, "missing x-inner"));
    let resp = raw(
        addr,
        "GET",
        "/api/v1/item",
        &[("x-outer", "1"), ("x-inner", "1")],
    )
    .await;
    assert_eq!((resp.status, resp.body.as_str()), (200, "item"));

    // The mounted router's authenticator guards its routes — and only them.
    assert_eq!(get(addr, "/admin/stats").await.status, 401);
    assert_eq!(get(addr, "//admin/stats").await.status, 400);
    assert_eq!(get(addr, "/x/../admin/stats").await.status, 401);
    let resp = raw(addr, "GET", "/admin/stats", &[("x-admin", "1")]).await;
    assert_eq!((resp.status, resp.body.as_str()), (200, "stats"));
}

#[tokio::test]
async fn authenticator_filters_and_router_see_the_same_path() {
    let router = Router::new()
        .auth(|ctx| {
            (ctx.path().starts_with("/secure") && ctx.header("authorization").is_none())
                .then(|| HttpResponse::text(StatusCode::UNAUTHORIZED, "no auth"))
        })
        .get("/secure/data", |ctx| async move {
            HttpResponse::text(StatusCode::OK, ctx.path().to_string())
        })
        .layer(|ctx, next| async move {
            let seen = ctx.path().to_string();
            next.run(ctx).await.header("x-layer-path", seen)
        });
    let addr = spawn(router).await;

    for target in ["/x/../secure/data", "/%73ecure/data", "/secure/./data"] {
        let resp = get(addr, target).await;
        assert_eq!(resp.status, 401, "{target}");
        let resp = raw(addr, "GET", target, &[("authorization", "x")]).await;
        assert_eq!(resp.status, 200, "{target}");
        assert_eq!(resp.body, "/secure/data", "{target}");
        assert_eq!(resp.header("x-layer-path").as_deref(), Some("/secure/data"));
    }
}

// ---- parameter and query decoding ---------------------------------------------------

#[tokio::test]
async fn path_parameters_are_decoded_after_routing() {
    let router = Router::new()
        .get("/files/:name", |ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                format!("one:{}", ctx.param("name").unwrap_or("")),
            )
        })
        .get("/files/:dir/:name", |ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                "two".to_string() + ctx.param("name").unwrap(),
            )
        });
    let addr = spawn(router).await;

    // An encoded slash stays inside the one segment it was sent in.
    assert_eq!(get(addr, "/files/a%2Fb").await.body, "one:a/b");
    assert_eq!(get(addr, "/files/a%2fb").await.body, "one:a/b");
    assert_eq!(get(addr, "/files/a%20b").await.body, "one:a b");
    assert_eq!(get(addr, "/files/%E2%9C%93").await.body, "one:\u{2713}");
    assert_eq!(get(addr, "/files/100%25").await.body, "one:100%");
    assert_eq!(get(addr, "/files/a+b").await.body, "one:a+b");
    // Encoded dots are dot segments (and are resolved); they never reach a
    // handler as a parameter.
    assert_eq!(get(addr, "/files/x/%2e%2e/y").await.body, "one:y");
    // A real slash is a separator.
    assert_eq!(get(addr, "/files/a/b").await.body, "twob");
    // Bytes that are not UTF-8 cannot become a `&str` parameter.
    let resp = get(addr, "/files/%FF%FE").await;
    assert_eq!(resp.status, 400, "{}", resp.body);
}

#[tokio::test]
async fn query_keys_and_values_are_decoded_and_validated() {
    let router = Router::new().get("/q", |ctx| async move {
        let pairs: Vec<String> = ctx
            .query_pairs()
            .iter()
            .map(|(k, v)| format!("[{k}]=[{v}]"))
            .collect();
        HttpResponse::text(StatusCode::OK, pairs.join(","))
    });
    let addr = spawn(router).await;

    assert_eq!(
        get(addr, "/q?na%20me=va+lue%26x&%E2%9C%93=1&flag")
            .await
            .body,
        "[na me]=[va lue&x],[\u{2713}]=[1],[flag]=[]"
    );
    assert_eq!(get(addr, "/q?a=1%2B1").await.body, "[a]=[1+1]");
    for target in ["/q?a=%FF", "/q?%FF=1", "/q?a=%zz", "/q?a=100%"] {
        assert_eq!(get(addr, target).await.status, 400, "{target}");
    }
}

// ---- connection metadata and rate limiting --------------------------------------------

#[tokio::test]
async fn ctx_exposes_connection_metadata() {
    let router = Router::new().get("/whoami", |ctx| async move {
        HttpResponse::text(
            StatusCode::OK,
            format!(
                "{} tls={} certs={}",
                ctx.peer_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_default(),
                ctx.is_tls(),
                ctx.peer_certificates().len()
            ),
        )
    });
    let addr = spawn(router).await;
    // A forwarding header does not change what the peer address is.
    let resp = raw(addr, "GET", "/whoami", &[("x-forwarded-for", "1.2.3.4")]).await;
    assert_eq!(resp.body, "127.0.0.1 tls=false certs=0");
}

#[tokio::test]
async fn rate_limit_keys_on_the_peer_and_ignores_spoofed_headers() {
    let router = Router::new()
        .rate_limit_with(RateLimit::new(0.5, 2.0))
        .get("/", |_| async { ok("ok") });
    let addr = spawn(router).await;

    // Each request claims to come from somewhere else; none of it is believed.
    let first = raw(addr, "GET", "/", &[("x-forwarded-for", "10.1.1.1")]).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.header("ratelimit-limit").as_deref(), Some("2"));
    assert_eq!(first.header("ratelimit-remaining").as_deref(), Some("1"));
    assert!(first.header("ratelimit-reset").is_some());

    let second = raw(addr, "GET", "/", &[("x-forwarded-for", "10.2.2.2")]).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.header("ratelimit-remaining").as_deref(), Some("0"));

    let third = raw(
        addr,
        "GET",
        "/",
        &[
            ("x-forwarded-for", "10.3.3.3"),
            ("forwarded", "for=10.4.4.4"),
        ],
    )
    .await;
    assert_eq!(third.status, 429);
    assert_eq!(third.header("ratelimit-remaining").as_deref(), Some("0"));
    assert_eq!(third.header("retry-after").as_deref(), Some("2"));
}

#[tokio::test]
async fn rate_limit_trusted_proxy_mode_is_explicit() {
    let proxies = TrustedProxies::from_cidrs(["127.0.0.0/8"]).unwrap();
    let router = Router::new()
        .rate_limit_with(RateLimit::new(0.0, 1.0).trusted_proxies(proxies))
        .get("/", |_| async { ok("ok") });
    let addr = spawn(router).await;

    // The peer (127.0.0.1) is a declared proxy, so the client address comes
    // from the header: two clients, two buckets.
    let from = |client: &'static str| async move {
        raw(addr, "GET", "/", &[("x-forwarded-for", client)])
            .await
            .status
    };
    assert_eq!(from("198.51.100.1").await, 200);
    assert_eq!(from("198.51.100.2").await, 200);
    assert_eq!(from("198.51.100.1").await, 429);
    // The left-most entry is client-controlled and is not the key: the
    // right-most untrusted address is.
    assert_eq!(from("6.6.6.6, 198.51.100.2").await, 429);
    assert_eq!(from("198.51.100.2, 198.51.100.3").await, 200);
}

#[tokio::test]
async fn legacy_rate_limit_signature_still_works() {
    let router = Router::new()
        .rate_limit(0.0, 1.0, |_ctx| "shared".to_string())
        .get("/", |_| async { ok("ok") });
    let addr = spawn(router).await;
    assert_eq!(get(addr, "/").await.status, 200);
    let limited = get(addr, "/").await;
    assert_eq!(limited.status, 429);
    // No refill: there is no honest Retry-After to give.
    assert_eq!(limited.header("retry-after"), None);
}

// ---- CORS -----------------------------------------------------------------------------

fn cors_router() -> Router {
    let cors = Cors::new()
        .allow_origin("https://app.test")
        .allow_origin_fn(|origin| {
            origin.ends_with(".tenant.test") && origin.starts_with("https://")
        })
        .allow_methods([Method::Get, Method::Post, Method::Delete])
        .allow_headers(["content-type", "x-token"])
        .expose_headers(["x-request-id"])
        .allow_credentials(true)
        .max_age(std::time::Duration::from_secs(600));
    Router::new()
        .cors(cors)
        .auth(|ctx| {
            ctx.header("authorization")
                .is_none()
                .then(|| HttpResponse::text(StatusCode::UNAUTHORIZED, "no auth"))
        })
        .get("/items", |_| async { ok("items") })
}

#[tokio::test]
async fn cors_preflight_is_answered_before_authentication() {
    let addr = spawn(cors_router()).await;
    let preflight = raw(
        addr,
        "OPTIONS",
        "/items",
        &[
            ("origin", "https://app.test"),
            ("access-control-request-method", "DELETE"),
            ("access-control-request-headers", "x-token"),
        ],
    )
    .await;
    assert_eq!(preflight.status, 204, "{}", preflight.head);
    assert_eq!(
        preflight.header("access-control-allow-origin").as_deref(),
        Some("https://app.test")
    );
    assert_eq!(
        preflight
            .header("access-control-allow-credentials")
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        preflight.header("access-control-allow-methods").as_deref(),
        Some("GET, POST, DELETE")
    );
    assert_eq!(
        preflight.header("access-control-allow-headers").as_deref(),
        Some("content-type, x-token")
    );
    assert_eq!(
        preflight.header("access-control-max-age").as_deref(),
        Some("600")
    );
    assert!(preflight.header("vary").unwrap().contains("Origin"));

    // A predicate-matched origin is allowed too.
    let preflight = raw(
        addr,
        "OPTIONS",
        "/items",
        &[
            ("origin", "https://a.tenant.test"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(preflight.status, 204);

    // Any other origin gets no CORS headers at all.
    for origin in ["https://evil.test", "https://app.test.evil.test", "null"] {
        let refused = raw(
            addr,
            "OPTIONS",
            "/items",
            &[("origin", origin), ("access-control-request-method", "GET")],
        )
        .await;
        assert_eq!(refused.status, 403, "{origin}");
        assert_eq!(
            refused.header("access-control-allow-origin"),
            None,
            "{origin}"
        );
        assert_eq!(
            refused.header("access-control-allow-credentials"),
            None,
            "{origin}"
        );
    }
}

#[tokio::test]
async fn cors_actual_requests_get_headers_only_for_allowed_origins() {
    let addr = spawn(cors_router()).await;

    let allowed = raw(
        addr,
        "GET",
        "/items",
        &[("origin", "https://app.test"), ("authorization", "x")],
    )
    .await;
    assert_eq!(allowed.status, 200);
    assert_eq!(
        allowed.header("access-control-allow-origin").as_deref(),
        Some("https://app.test")
    );
    assert_eq!(
        allowed
            .header("access-control-allow-credentials")
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        allowed.header("access-control-expose-headers").as_deref(),
        Some("x-request-id")
    );
    assert_eq!(allowed.header("vary").as_deref(), Some("Origin"));

    // CORS is not access control: the request still has to authenticate.
    let unauthenticated = raw(addr, "GET", "/items", &[("origin", "https://app.test")]).await;
    assert_eq!(unauthenticated.status, 401);

    let other = raw(
        addr,
        "GET",
        "/items",
        &[("origin", "https://evil.test"), ("authorization", "x")],
    )
    .await;
    assert_eq!(other.status, 200);
    assert_eq!(other.header("access-control-allow-origin"), None);
    assert_eq!(other.header("access-control-allow-credentials"), None);
    assert_eq!(other.header("vary").as_deref(), Some("Origin"));

    // Same-origin requests (no Origin) still vary by Origin for caches.
    let same = raw(addr, "GET", "/items", &[("authorization", "x")]).await;
    assert_eq!(same.header("access-control-allow-origin"), None);
    assert_eq!(same.header("vary").as_deref(), Some("Origin"));
}

#[tokio::test]
async fn cors_wildcard_is_never_combined_with_credentials() {
    let router = Router::new()
        .cors(Cors::new().allow_any_origin().allow_credentials(true))
        .get("/", |_| async { ok("ok") });
    // The contradiction is reported …
    assert!(matches!(router.validate(), Err(RouteError::Config(_))));

    // … and at run time the wildcard is sent without credentials.
    let addr = spawn(router).await;
    let resp = raw(addr, "GET", "/", &[("origin", "https://anyone.test")]).await;
    assert_eq!(
        resp.header("access-control-allow-origin").as_deref(),
        Some("*")
    );
    assert_eq!(resp.header("access-control-allow-credentials"), None);

    let preflight = raw(
        addr,
        "OPTIONS",
        "/",
        &[
            ("origin", "https://anyone.test"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(
        preflight.header("access-control-allow-origin").as_deref(),
        Some("*")
    );
    assert_eq!(preflight.header("access-control-allow-credentials"), None);
}

// ---- security headers and request ids -------------------------------------------------

#[tokio::test]
async fn security_headers_cover_every_response() {
    let router = Router::new()
        .security_headers(SecurityHeaders::new())
        .get("/", |_| async { ok("ok") })
        .get("/own", |_| async {
            ok("own").header("X-Frame-Options", "SAMEORIGIN")
        });
    let addr = spawn(router).await;

    for target in ["/", "/missing"] {
        let resp = get(addr, target).await;
        assert_eq!(
            resp.header("x-content-type-options").as_deref(),
            Some("nosniff"),
            "{target}"
        );
        assert_eq!(resp.header("x-frame-options").as_deref(), Some("DENY"));
        assert_eq!(
            resp.header("content-security-policy").as_deref(),
            Some("frame-ancestors 'none'")
        );
        assert_eq!(
            resp.header("referrer-policy").as_deref(),
            Some("no-referrer")
        );
        // Plain HTTP: no HSTS.
        assert_eq!(resp.header("strict-transport-security"), None);
    }
    // A header the handler set is kept.
    assert_eq!(
        get(addr, "/own").await.headers("x-frame-options"),
        vec!["SAMEORIGIN".to_string()]
    );

    let behind_proxy = Router::new()
        .security_headers(
            SecurityHeaders::new()
                .hsts(std::time::Duration::from_secs(60), false)
                .hsts_behind_proxy(true)
                .frame_options(FrameOptions::SameOrigin)
                .content_security_policy("default-src 'self'"),
        )
        .get("/", |_| async { ok("ok") });
    let addr = spawn(behind_proxy).await;
    let resp = get(addr, "/").await;
    assert_eq!(
        resp.header("strict-transport-security").as_deref(),
        Some("max-age=60")
    );
    assert_eq!(
        resp.header("x-frame-options").as_deref(),
        Some("SAMEORIGIN")
    );
    assert_eq!(
        resp.header("content-security-policy").as_deref(),
        Some("default-src 'self'; frame-ancestors 'self'")
    );
}

#[tokio::test]
async fn request_id_accepts_only_well_formed_inbound_ids() {
    let router = Router::new().request_id().get("/", |ctx| async move {
        HttpResponse::text(StatusCode::OK, ctx.request_id().unwrap_or_default())
    });
    let addr = spawn(router).await;

    let kept = raw(addr, "GET", "/", &[("x-request-id", "req-42.A_b")]).await;
    assert_eq!(kept.body, "req-42.A_b");
    assert_eq!(kept.header("x-request-id").as_deref(), Some("req-42.A_b"));

    let too_long = "a".repeat(129);
    for bad in [
        "has space",
        "quote\"d",
        "semi;colon",
        "new=line",
        "<script>",
        too_long.as_str(),
    ] {
        let resp = raw(addr, "GET", "/", &[("x-request-id", bad)]).await;
        assert_ne!(resp.body, bad, "{bad:?} must not be accepted");
        assert_eq!(resp.body.len(), 32, "{bad:?} -> {:?}", resp.body);
        assert!(resp.body.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(resp.header("x-request-id"), Some(resp.body.clone()));
    }

    // Without an inbound id, each request gets its own.
    let a = get(addr, "/").await.body;
    let b = get(addr, "/").await.body;
    assert_eq!(a.len(), 32);
    assert_ne!(a, b);
}

// ---- CSRF and cookies -----------------------------------------------------------------

#[cfg(feature = "hash")]
#[tokio::test]
async fn csrf_double_submit() {
    use ferroly::turbo::{Csrf, CsrfToken};

    let router = Router::new()
        .csrf(Csrf::new())
        .get("/form", |ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                ctx.get::<CsrfToken>()
                    .map(|t| t.0.clone())
                    .unwrap_or_default(),
            )
        })
        .post("/transfer", |_| async { ok("done") });
    assert!(router.validate().is_ok());
    let addr = spawn(router).await;

    // A safe request issues the token, in the page and in a cookie.
    let form = get(addr, "/form").await;
    let token = form.body.clone();
    assert!(token.len() >= 32, "{token:?}");
    let cookie = form.header("set-cookie").expect("csrf cookie");
    assert!(
        cookie.starts_with(&format!("__Host-csrf={token}; ")),
        "{cookie}"
    );
    assert!(cookie.contains("; Secure"), "{cookie}");
    assert!(cookie.contains("; SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("; Path=/"), "{cookie}");
    assert!(!cookie.contains("HttpOnly"), "{cookie}");
    let cookie_header = format!("__Host-csrf={token}");

    // A request that already has the cookie is not given another.
    let again = raw(addr, "GET", "/form", &[("cookie", cookie_header.as_str())]).await;
    assert_eq!(again.body, token);
    assert_eq!(again.header("set-cookie"), None);

    // Unsafe requests need the cookie and the matching header.
    let post = |headers: Vec<(&'static str, String)>| async move {
        let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        raw(addr, "POST", "/transfer", &borrowed).await.status
    };
    assert_eq!(post(vec![]).await, 403);
    assert_eq!(post(vec![("cookie", cookie_header.clone())]).await, 403);
    assert_eq!(post(vec![("x-csrf-token", token.clone())]).await, 403);
    assert_eq!(
        post(vec![
            ("cookie", cookie_header.clone()),
            ("x-csrf-token", format!("{token}x")),
        ])
        .await,
        403
    );
    // An attacker-chosen pair that was never issued in this shape is refused.
    assert_eq!(
        post(vec![
            ("cookie", "__Host-csrf=short".to_string()),
            ("x-csrf-token", "short".to_string()),
        ])
        .await,
        403
    );
    assert_eq!(
        post(vec![
            ("cookie", cookie_header.clone()),
            ("x-csrf-token", token.clone()),
        ])
        .await,
        200
    );

    // A `__Host-` cookie cannot be issued without `Secure`: reported up front.
    let misconfigured = Router::new().csrf(Csrf::new().secure(false));
    assert!(matches!(
        misconfigured.validate(),
        Err(RouteError::Config(_))
    ));
}

#[tokio::test]
async fn cookies_are_parsed_and_set() {
    let router = Router::new().get("/session", |ctx| async move {
        let seen = ctx.cookie("sid").unwrap_or_else(|| "none".to_string());
        let resp = HttpResponse::text(StatusCode::OK, format!("{seen}/{}", ctx.cookies().len()));
        let resp = SetCookie::new("__Host-sid", "fresh")
            .same_site(SameSite::Strict)
            .max_age(std::time::Duration::from_secs(60))
            .apply(resp)
            .unwrap();
        SetCookie::removal("old").apply(resp).unwrap()
    });
    let addr = spawn(router).await;

    let resp = raw(
        addr,
        "GET",
        "/session",
        &[("cookie", "theme=dark; sid=abc")],
    )
    .await;
    assert_eq!(resp.body, "abc/2");
    let set = resp.headers("set-cookie");
    assert_eq!(set.len(), 2, "{set:?}");
    assert_eq!(
        set[0],
        "__Host-sid=fresh; Path=/; Max-Age=60; Secure; HttpOnly; SameSite=Strict"
    );
    assert!(set[1].starts_with("old=; Path=/; Max-Age=0; Expires=Thu, 01 Jan 1970"));

    assert_eq!(get(addr, "/session").await.body, "none/0");
}
