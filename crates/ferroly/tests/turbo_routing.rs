#![cfg(feature = "turbo")]
//! Routing and request-handling features of the router: automatic `HEAD` /
//! `OPTIONS`, catch-alls, specificity, groups and mounts, conflict detection,
//! per-route options, typed errors, state and extensions, asynchronous
//! filters, forms, schema validation, static files and the `serve` variants.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use ferroly::codec::json;
use ferroly::http::{
    Client, ClientTlsConfig, HttpResponse, Method, Multipart, Request, ServerConfig, StatusCode,
    TlsConfig,
};
use ferroly::turbo::{
    Ctx, IntoResponse, MultipartLimits, Problem, Route, RouteError, Router, StaticFiles,
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

struct Raw {
    status: u16,
    head: String,
    body: String,
}

impl Raw {
    fn header(&self, name: &str) -> Option<String> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    }
}

async fn raw(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Raw {
    let mut request = format!("{method} {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response).into_owned();
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
    raw(addr, "GET", target, &[], b"").await
}

fn ok(text: impl Into<String>) -> HttpResponse {
    HttpResponse::text(StatusCode::OK, text)
}

/// A handler that answers with the matched route template.
async fn template(ctx: Ctx) -> HttpResponse {
    ok(ctx.route().unwrap_or("-").to_string())
}

// ---- automatic HEAD and OPTIONS -------------------------------------------------------

#[tokio::test]
async fn head_is_answered_by_the_get_route() {
    let router = Router::new()
        .get("/doc", |_| async {
            ok("twelve bytes").header("x-kind", "doc")
        })
        .post("/only-post", |_| async { ok("posted") });
    let addr = spawn(router).await;

    let head = raw(addr, "HEAD", "/doc", &[], b"").await;
    assert_eq!(head.status, 200);
    assert_eq!(head.body, "", "a HEAD response has no body");
    assert_eq!(head.header("content-length").as_deref(), Some("12"));
    assert_eq!(head.header("x-kind").as_deref(), Some("doc"));

    // No GET route: nothing to fall back to.
    let head = raw(addr, "HEAD", "/only-post", &[], b"").await;
    assert_eq!(head.status, 405);
    assert_eq!(head.header("allow").as_deref(), Some("POST, OPTIONS"));

    // An explicit HEAD route wins over the fallback.
    let explicit = Router::new()
        .get("/doc", |_| async { ok("body") })
        .head("/doc", |_| async {
            HttpResponse::new(StatusCode::NO_CONTENT).header("x-explicit", "1")
        });
    let addr = spawn(explicit).await;
    let head = raw(addr, "HEAD", "/doc", &[], b"").await;
    assert_eq!(head.status, 204);
    assert_eq!(head.header("x-explicit").as_deref(), Some("1"));

    // Opt-out.
    let off = Router::new()
        .auto_head(false)
        .get("/doc", |_| async { ok("body") });
    let addr = spawn(off).await;
    let head = raw(addr, "HEAD", "/doc", &[], b"").await;
    assert_eq!(head.status, 405);
    assert_eq!(head.header("allow").as_deref(), Some("GET, OPTIONS"));
}

#[tokio::test]
async fn options_is_answered_with_allow() {
    let router = Router::new()
        .get("/thing", |_| async { ok("g") })
        .put("/thing", |_| async { ok("p") })
        .options("/custom", |_| async { ok("mine") })
        .get("/custom", |_| async { ok("g") });
    let addr = spawn(router).await;

    let options = raw(addr, "OPTIONS", "/thing", &[], b"").await;
    assert_eq!(options.status, 204);
    assert_eq!(
        options.header("allow").as_deref(),
        Some("GET, PUT, HEAD, OPTIONS")
    );
    // An explicit OPTIONS route is used as registered.
    assert_eq!(raw(addr, "OPTIONS", "/custom", &[], b"").await.body, "mine");
    // An unknown path is still a 404.
    assert_eq!(raw(addr, "OPTIONS", "/nope", &[], b"").await.status, 404);
    // `OPTIONS *` asks about the server as a whole.
    let star = raw(addr, "OPTIONS", "*", &[], b"").await;
    assert_eq!(star.status, 204);
    assert!(star.header("allow").unwrap().contains("PUT"));
    // 405 lists the same methods.
    let wrong = raw(addr, "DELETE", "/thing", &[], b"").await;
    assert_eq!(wrong.status, 405);
    assert_eq!(
        wrong.header("allow").as_deref(),
        Some("GET, PUT, HEAD, OPTIONS")
    );

    let off = Router::new()
        .auto_options(false)
        .get("/thing", |_| async { ok("g") });
    let addr = spawn(off).await;
    let options = raw(addr, "OPTIONS", "/thing", &[], b"").await;
    assert_eq!(options.status, 405);
    assert_eq!(options.header("allow").as_deref(), Some("GET, HEAD"));
}

// ---- patterns ---------------------------------------------------------------------------

#[tokio::test]
async fn catch_all_and_specificity() {
    // Registered least specific first, on purpose.
    let router = Router::new()
        .get("/files/*path", |ctx| async move {
            ok(format!(
                "all:{}:{}",
                ctx.param("path").unwrap_or("?"),
                ctx.tail_segments().len()
            ))
        })
        .get("/files/:name", |ctx| async move {
            ok(format!("one:{}", ctx.param("name").unwrap_or("?")))
        })
        .get("/files/special", |_| async { ok("special") })
        .get("/items/:id", |_| async { ok("id") })
        .get("/items/new", |_| async { ok("new") });
    assert!(router.validate().is_ok());
    let addr = spawn(router).await;

    assert_eq!(get(addr, "/files/special").await.body, "special");
    assert_eq!(get(addr, "/files/other").await.body, "one:other");
    assert_eq!(get(addr, "/files/a/b/c").await.body, "all:a/b/c:3");
    assert_eq!(get(addr, "/files").await.body, "all::0");
    assert_eq!(get(addr, "/files/").await.body, "all::0");
    // Each catch-all segment is decoded on its own.
    assert_eq!(get(addr, "/files/a%2Fb/c%20d").await.body, "all:a/b/c d:2");
    assert_eq!(get(addr, "/items/new").await.body, "new");
    assert_eq!(get(addr, "/items/7").await.body, "id");
}

#[tokio::test]
async fn groups_nest_mount_and_match_router_methods() {
    let v2 = Router::new()
        .with_state(String::from("from-v2"))
        .layer(|ctx, next| async move { next.run(ctx).await.header("x-mounted", "v2") })
        .get("/users/:id", template)
        .get("/", template);
    let router = Router::new()
        .with_state(41u32)
        .group("/api", |api| {
            api.layer(|ctx, next| async move { next.run(ctx).await.header("x-api", "1") })
                .get("/a", template)
                .patch("/a", template)
                .head("/h", |_| async {
                    HttpResponse::new(StatusCode::NO_CONTENT)
                })
                .options("/o", template)
                .add("/multi", template, vec![Method::Put, Method::Delete])
                .route("/r", Route::post(template))
                .group("/v1", |v1| {
                    v1.layer(|ctx, next| async move { next.run(ctx).await.header("x-v1", "1") })
                        .get("/users/:id", template)
                        .group("/deep", |deep| deep.get("/leaf", template))
                })
        })
        .mount("/v2", v2)
        .get("/state", |ctx| async move {
            ok(format!(
                "{}/{}",
                ctx.state::<u32>().map(|n| *n).unwrap_or(0),
                ctx.state::<String>()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            ))
        });
    assert!(router.validate().is_ok());
    let addr = spawn(router).await;

    assert_eq!(get(addr, "/api/a").await.body, "/api/a");
    assert_eq!(raw(addr, "PATCH", "/api/a", &[], b"").await.body, "/api/a");
    assert_eq!(raw(addr, "HEAD", "/api/h", &[], b"").await.status, 204);
    assert_eq!(
        raw(addr, "OPTIONS", "/api/o", &[], b"").await.body,
        "/api/o"
    );
    assert_eq!(
        raw(addr, "DELETE", "/api/multi", &[], b"").await.body,
        "/api/multi"
    );
    assert_eq!(raw(addr, "POST", "/api/r", &[], b"").await.body, "/api/r");

    let nested = get(addr, "/api/v1/users/9").await;
    assert_eq!(nested.body, "/api/v1/users/:id");
    // Outer and inner group layers both wrap a nested route …
    assert_eq!(nested.header("x-api").as_deref(), Some("1"));
    assert_eq!(nested.header("x-v1").as_deref(), Some("1"));
    assert_eq!(
        get(addr, "/api/v1/deep/leaf").await.body,
        "/api/v1/deep/leaf"
    );
    // … and neither wraps anything outside its group.
    let outer = get(addr, "/api/a").await;
    assert_eq!(outer.header("x-api").as_deref(), Some("1"));
    assert_eq!(outer.header("x-v1"), None);
    assert_eq!(get(addr, "/state").await.header("x-api"), None);

    let mounted = get(addr, "/v2/users/3").await;
    assert_eq!(mounted.body, "/v2/users/:id");
    assert_eq!(mounted.header("x-mounted").as_deref(), Some("v2"));
    assert_eq!(get(addr, "/v2").await.body, "/v2");
    assert_eq!(get(addr, "/api/a").await.header("x-mounted"), None);
    // State of both routers is visible everywhere.
    assert_eq!(get(addr, "/state").await.body, "41/from-v2");
}

#[tokio::test]
async fn conflicts_and_bad_patterns_are_reported_not_panicked() {
    let conflict = || {
        Router::new()
            .get("/users/:id", |_| async { ok("first") })
            .post("/users/:id", |_| async { ok("post") })
            .get("/users/:name", |_| async { ok("second") })
    };
    let problems = conflict().problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(
        problems[0],
        RouteError::Conflict {
            method: "GET".into(),
            first: "/users/:id".into(),
            second: "/users/:name".into(),
        }
    );
    assert!(conflict().validate().is_err());
    assert!(conflict().try_into_handler().is_err());
    // The lenient path keeps the first registration.
    let addr = spawn(conflict()).await;
    assert_eq!(get(addr, "/users/1").await.body, "first");

    // The same path with different methods is not a conflict; nor is a
    // literal beside a parameter.
    let fine = Router::new()
        .get("/a/:x", |_| async { ok("") })
        .put("/a/:y", |_| async { ok("") })
        .get("/a/b", |_| async { ok("") });
    assert_eq!(fine.problems(), vec![]);
    // Under strict slash, `/x` and `/x/` are different routes.
    let slashes = || {
        Router::new()
            .get("/x", |_| async { ok("") })
            .get("/x/", |_| async { ok("") })
    };
    assert!(slashes().validate().is_err());
    assert!(slashes().strict_slash(true).validate().is_ok());

    for pattern in ["/a/:", "/a/:x/:x", "/a/*rest/b", "/a/../b"] {
        let router = Router::new().get(pattern, |_| async { ok("reached") });
        assert!(
            matches!(router.validate(), Err(RouteError::InvalidPattern { .. })),
            "{pattern}"
        );
    }

    // A group with a malformed prefix registers nothing: its handlers must
    // not end up reachable without the group's filter somewhere else.
    let router = Router::new().group("/bad/*all", |g| {
        g.filter(|_| Some(HttpResponse::text(StatusCode::FORBIDDEN, "guard")))
            .get("/secret", |_| async { ok("secret") })
            .group("/inner", |inner| {
                inner.get("/deep", |_| async { ok("deep") })
            })
    });
    assert!(matches!(
        router.validate(),
        Err(RouteError::InvalidPattern { .. })
    ));
    let addr = spawn(router).await;
    for target in [
        "/secret",
        "/bad/secret",
        "/bad/x/secret",
        "/inner/deep",
        "/deep",
    ] {
        let resp = get(addr, target).await;
        assert_eq!(resp.status, 404, "{target}");
    }
}

// ---- per-route options --------------------------------------------------------------------

#[tokio::test]
async fn per_route_middleware_filter_limit_and_timeout() {
    let router = Router::new()
        .body_limit(8)
        .route(
            "/upload",
            Route::post(|ctx: Ctx| async move { ok(ctx.body().len().to_string()) })
                .body_limit(4)
                .layer(|ctx, next| async move { next.run(ctx).await.header("x-route", "upload") })
                .filter(|ctx| {
                    ctx.header("x-allow")
                        .is_none()
                        .then(|| HttpResponse::text(StatusCode::FORBIDDEN, "route filter"))
                }),
        )
        .post("/default-limit", |ctx| async move {
            ok(ctx.body().len().to_string())
        })
        .route(
            "/slow",
            Route::get(|_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                ok("never")
            })
            .timeout(Duration::from_millis(50)),
        )
        .get("/fast", |_| async { ok("fast") });
    let addr = spawn(router).await;

    let allow = [("x-allow", "1")];
    let small = raw(addr, "POST", "/upload", &allow, b"1234").await;
    assert_eq!((small.status, small.body.as_str()), (200, "4"));
    assert_eq!(small.header("x-route").as_deref(), Some("upload"));
    assert_eq!(
        raw(addr, "POST", "/upload", &allow, b"12345").await.status,
        413
    );
    assert_eq!(raw(addr, "POST", "/upload", &[], b"1").await.status, 403);

    // The router-wide default applies where a route sets none, and the
    // route's layer does not leak onto other routes.
    let other = raw(addr, "POST", "/default-limit", &[], b"12345678").await;
    assert_eq!((other.status, other.body.as_str()), (200, "8"));
    assert_eq!(other.header("x-route"), None);
    assert_eq!(
        raw(addr, "POST", "/default-limit", &[], b"123456789")
            .await
            .status,
        413
    );

    let started = std::time::Instant::now();
    assert_eq!(get(addr, "/slow").await.status, 503);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(get(addr, "/fast").await.body, "fast");
}

#[tokio::test]
async fn router_wide_request_timeout() {
    let router = Router::new()
        .request_timeout(Duration::from_millis(50))
        .get("/slow", |_| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            ok("never")
        })
        .route(
            "/patient",
            Route::get(|_| async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                ok("done")
            })
            .timeout(Duration::from_secs(10)),
        );
    let addr = spawn(router).await;
    assert_eq!(get(addr, "/slow").await.status, 503);
    assert_eq!(get(addr, "/patient").await.body, "done");
}

// ---- typed errors ---------------------------------------------------------------------------

enum OrderError {
    Missing(u64),
    Locked,
}

impl IntoResponse for OrderError {
    fn into_response(self) -> HttpResponse {
        match self {
            OrderError::Missing(id) => Problem::new(StatusCode::NOT_FOUND)
                .detail(format!("order {id} does not exist"))
                .with("order_id", id)
                .into_response(),
            OrderError::Locked => {
                HttpResponse::text(StatusCode(423), "locked").header("retry-after", "5")
            }
        }
    }
}

async fn show_order(ctx: Ctx) -> Result<HttpResponse, OrderError> {
    match ctx.param_as::<u64>("id") {
        Ok(1) => Ok(ok("order 1")),
        Ok(2) => Err(OrderError::Locked),
        Ok(id) => Err(OrderError::Missing(id)),
        Err(_) => Err(OrderError::Missing(0)),
    }
}

#[tokio::test]
async fn handlers_return_typed_errors() {
    let router = Router::new()
        .get("/orders/:id", show_order)
        .get("/typed/:n", |ctx| async move {
            // `?` on a ParamError: 400 problem+json.
            let n: u32 = ctx.param_as("n")?;
            Ok::<_, ferroly::turbo::ParamError>(ok(n.to_string()))
        })
        .post("/body", |ctx| async move {
            #[derive(ferroly::codec::Decode)]
            struct Body {
                n: i64,
            }
            // `?` converts the CodecError into a 400 / 415 problem.
            let body: Body = ctx.read()?;
            Ok::<_, Problem>(ok(body.n.to_string()))
        })
        .get("/plain", |_| async { (StatusCode::CREATED, "made") })
        .on_not_found(|_| async { Problem::new(StatusCode::NOT_FOUND).detail("no such page") });
    let addr = spawn(router).await;

    assert_eq!(get(addr, "/orders/1").await.body, "order 1");
    let missing = get(addr, "/orders/9").await;
    assert_eq!(missing.status, 404);
    assert_eq!(
        missing.header("content-type").as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(
        missing.body,
        r#"{"type":"about:blank","title":"Not Found","status":404,"detail":"order 9 does not exist","order_id":9}"#
    );
    let locked = get(addr, "/orders/2").await;
    assert_eq!(locked.status, 423);
    assert_eq!(locked.header("retry-after").as_deref(), Some("5"));

    assert_eq!(get(addr, "/typed/7").await.body, "7");
    let bad = get(addr, "/typed/seven").await;
    assert_eq!(bad.status, 400);
    assert!(bad.body.contains(r#""status":400"#), "{}", bad.body);
    assert!(bad.body.contains("invalid parameter 'n'"), "{}", bad.body);

    let json = [("content-type", "application/json")];
    assert_eq!(
        raw(addr, "POST", "/body", &json, br#"{"n":5}"#).await.body,
        "5"
    );
    assert_eq!(
        raw(addr, "POST", "/body", &json, b"{oops").await.status,
        400
    );
    let unsupported = raw(
        addr,
        "POST",
        "/body",
        &[("content-type", "text/csv")],
        b"n\n5",
    )
    .await;
    assert_eq!(unsupported.status, 415);

    let plain = get(addr, "/plain").await;
    assert_eq!((plain.status, plain.body.as_str()), (201, "made"));
    let fallback = get(addr, "/nowhere").await;
    assert_eq!(fallback.status, 404);
    assert!(fallback.body.contains("no such page"));
}

#[cfg(feature = "errutils")]
#[tokio::test]
async fn classified_errors_render_as_problems_without_leaking_internals() {
    use ferroly::errutils::{Classify, ErrorKind};
    use ferroly::turbo::Classified;

    #[derive(Debug)]
    enum StoreError {
        Missing,
        Disk(&'static str),
    }
    impl std::fmt::Display for StoreError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                StoreError::Missing => f.write_str("no such record"),
                StoreError::Disk(path) => write!(f, "write to {path} failed"),
            }
        }
    }
    impl Classify for StoreError {
        fn kind(&self) -> ErrorKind {
            match self {
                StoreError::Missing => ErrorKind::NotFound,
                StoreError::Disk(_) => ErrorKind::Internal,
            }
        }
    }
    fn load(which: &str) -> Result<String, StoreError> {
        match which {
            "ok" => Ok("record".into()),
            "missing" => Err(StoreError::Missing),
            _ => Err(StoreError::Disk("/var/lib/secret/db")),
        }
    }

    let router = Router::new().get("/records/:which", |ctx| async move {
        let record = load(ctx.param("which").unwrap_or(""))?;
        Ok::<_, Classified<StoreError>>(ok(record))
    });
    let addr = spawn(router).await;

    assert_eq!(get(addr, "/records/ok").await.body, "record");
    let missing = get(addr, "/records/missing").await;
    assert_eq!(missing.status, 404);
    assert!(
        missing.body.contains(r#""kind":"not_found""#),
        "{}",
        missing.body
    );
    assert!(missing.body.contains("no such record"), "{}", missing.body);
    // A server-side failure keeps its message to itself.
    let internal = get(addr, "/records/disk").await;
    assert_eq!(internal.status, 500);
    assert!(internal.body.contains(r#""kind":"internal""#));
    assert!(!internal.body.contains("/var/lib"), "{}", internal.body);
    assert!(!internal.body.contains("detail"), "{}", internal.body);
}

// ---- state, extensions, asynchronous filters ---------------------------------------------

struct Tenant(String);
struct Counter(std::sync::atomic::AtomicU32);

#[tokio::test]
async fn async_filters_pass_identity_through_extensions() {
    async fn lookup(key: String) -> Option<String> {
        tokio::time::sleep(Duration::from_millis(5)).await; // a "database"
        match key.as_str() {
            "k-acme" => Some("acme".to_string()),
            _ => None,
        }
    }

    let router = Router::new()
        .with_state(Counter(std::sync::atomic::AtomicU32::new(0)))
        .auth_async(|ctx| {
            let key = ctx.header("x-api-key").unwrap_or_default().to_string();
            let extensions = ctx.extensions();
            async move {
                match lookup(key).await {
                    Some(tenant) => {
                        extensions.insert(Tenant(tenant));
                        None
                    }
                    None => Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "bad key")),
                }
            }
        })
        .filter(|ctx| {
            // A later, synchronous filter sees what the authenticator stored.
            match ctx.get::<Tenant>() {
                Some(_) => None,
                None => Some(HttpResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "identity lost",
                )),
            }
        })
        .filter_async(|ctx| {
            let counter = ctx.state::<Counter>();
            async move {
                tokio::task::yield_now().await;
                if let Some(counter) = counter {
                    counter.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                None
            }
        })
        .group("/g", |g| {
            g.filter_async(|ctx| {
                let blocked = ctx.query_param("block").is_some();
                async move {
                    blocked.then(|| HttpResponse::text(StatusCode::FORBIDDEN, "async group filter"))
                }
            })
            .get("/x", |_| async { ok("in group") })
        })
        .route(
            "/me",
            Route::get(|ctx: Ctx| async move {
                let tenant = ctx.get::<Tenant>().map(|t| t.0.clone()).unwrap_or_default();
                let seen = ctx
                    .state::<Counter>()
                    .map(|c| c.0.load(std::sync::atomic::Ordering::SeqCst))
                    .unwrap_or(0);
                ok(format!("{tenant}:{seen}"))
            })
            .filter_async(|ctx| {
                let deny = ctx.header("x-deny").is_some();
                async move { deny.then(|| HttpResponse::text(StatusCode::FORBIDDEN, "route")) }
            }),
        );
    let addr = spawn(router).await;

    assert_eq!(get(addr, "/me").await.status, 401);
    let key = [("x-api-key", "k-acme")];
    assert_eq!(raw(addr, "GET", "/me", &key, b"").await.body, "acme:1");
    assert_eq!(raw(addr, "GET", "/me", &key, b"").await.body, "acme:2");
    assert_eq!(
        raw(
            addr,
            "GET",
            "/me",
            &[("x-api-key", "k-acme"), ("x-deny", "1")],
            b""
        )
        .await
        .status,
        403
    );
    assert_eq!(raw(addr, "GET", "/g/x", &key, b"").await.body, "in group");
    assert_eq!(
        raw(addr, "GET", "/g/x?block=1", &key, b"").await.status,
        403
    );
}

#[tokio::test]
async fn a_rewriting_middleware_is_routed_and_filtered_afresh() {
    let router = Router::new()
        .layer(|ctx, next| async move {
            if ctx.path() != "/old" {
                return next.run(ctx).await;
            }
            let mut request = ctx.into_request();
            request.uri.path = "/api/../api/new".to_string();
            match Ctx::from_request(request) {
                Ok(rewritten) => next.run(rewritten).await,
                Err(_) => HttpResponse::new(StatusCode::BAD_REQUEST),
            }
        })
        .group("/api", |g| {
            g.filter(|ctx| {
                ctx.header("x-key")
                    .is_none()
                    .then(|| HttpResponse::text(StatusCode::UNAUTHORIZED, "no key"))
            })
            .get("/new", |ctx| async move { ok(ctx.path().to_string()) })
        })
        .get("/old", |_| async { ok("old handler") });
    let addr = spawn(router).await;

    // The rewritten request lands in the group, so the group's filter runs.
    assert_eq!(get(addr, "/old").await.status, 401);
    let resp = raw(addr, "GET", "/old", &[("x-key", "k")], b"").await;
    assert_eq!((resp.status, resp.body.as_str()), (200, "/api/new"));
}

// ---- forms and validation -----------------------------------------------------------------

#[tokio::test]
async fn urlencoded_and_multipart_forms() {
    let router = Router::new()
        .post("/login", |ctx| async move {
            let form = ctx.form()?;
            let tags: Vec<&str> = form.get_all("tag").collect();
            Ok::<_, ferroly::turbo::FormError>(ok(format!(
                "{}|{}|{}",
                form.get("user").unwrap_or(""),
                form.get("note").unwrap_or(""),
                tags.join(",")
            )))
        })
        .post("/upload", |ctx| async move {
            let limits = MultipartLimits {
                max_parts: 2,
                max_part_bytes: 16,
                ..MultipartLimits::default()
            };
            let parts = ctx.multipart(&limits)?;
            let summary: Vec<String> = parts
                .iter()
                .map(|p| {
                    format!(
                        "{}={}({})",
                        p.name(),
                        p.text().unwrap_or("<binary>"),
                        p.safe_filename().unwrap_or_default()
                    )
                })
                .collect();
            Ok::<_, ferroly::turbo::FormError>(ok(summary.join(";")))
        });
    let addr = spawn(router).await;
    let client = Client::new();
    let url = |path: &str| format!("http://{addr}{path}");

    let resp = client
        .send(
            Request::builder(Method::Post, &url("/login"))
                .unwrap()
                .form([
                    ("user", "ada"),
                    ("note", "p&ss w=rd \u{2713}"),
                    ("tag", "a"),
                    ("tag", "b"),
                ])
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "ada|p&ss w=rd \u{2713}|a,b");

    // The wrong content type is 415; a malformed body 400.
    let wrong = raw(
        addr,
        "POST",
        "/login",
        &[("content-type", "application/json")],
        b"{}",
    )
    .await;
    assert_eq!(wrong.status, 415);
    let malformed = raw(
        addr,
        "POST",
        "/login",
        &[("content-type", "application/x-www-form-urlencoded")],
        b"user=%ZZ",
    )
    .await;
    assert_eq!(malformed.status, 400);

    let form = Multipart::new().text("title", "Q3").file(
        "report",
        "../../q3.csv",
        "text/csv",
        b"a,b\n1,2\n".to_vec(),
    );
    let resp = client
        .send(
            Request::builder(Method::Post, &url("/upload"))
                .unwrap()
                .multipart(&form)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.text().await.unwrap(),
        "title=Q3();report=a,b\n1,2\n(q3.csv)"
    );

    // Limits: too many parts, and a part that is too large.
    let too_many = Multipart::new()
        .text("a", "1")
        .text("b", "2")
        .text("c", "3");
    let too_big = Multipart::new().text("a", "x".repeat(17));
    for form in [too_many, too_big] {
        let resp = client
            .send(
                Request::builder(Method::Post, &url("/upload"))
                    .unwrap()
                    .multipart(&form)
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 413);
    }
}

#[tokio::test]
async fn schema_validation_answers_422_with_details() {
    let router = Router::new().post("/items", |ctx| async move {
        let schema = json::from_str(
            r#"{"type":"object","required":["name","qty"],
                "properties":{"name":{"type":"string","minLength":1},
                              "qty":{"type":"integer","minimum":1}}}"#,
        )
        .expect("valid schema");
        #[derive(ferroly::codec::Decode)]
        struct Item {
            name: String,
            qty: i64,
        }
        let item: Item = ctx.read_valid(&schema)?;
        Ok::<_, Problem>(HttpResponse::text(
            StatusCode::CREATED,
            format!("{}x{}", item.qty, item.name),
        ))
    });
    let addr = spawn(router).await;
    let json_ct = [("content-type", "application/json")];

    let good = raw(
        addr,
        "POST",
        "/items",
        &json_ct,
        br#"{"name":"bolt","qty":3}"#,
    )
    .await;
    assert_eq!((good.status, good.body.as_str()), (201, "3xbolt"));

    let invalid = raw(addr, "POST", "/items", &json_ct, br#"{"name":"","qty":0}"#).await;
    assert_eq!(invalid.status, 422);
    assert_eq!(
        invalid.header("content-type").as_deref(),
        Some("application/problem+json")
    );
    let report = json::from_str(&invalid.body).unwrap();
    let errors = report.get("errors").and_then(|e| e.as_array()).unwrap();
    assert_eq!(errors.len(), 2, "{}", invalid.body);
    assert!(errors
        .iter()
        .all(|e| e.get("path").is_some() && e.get("message").is_some()));

    let missing = raw(addr, "POST", "/items", &json_ct, br#"{"name":"bolt"}"#).await;
    assert_eq!(missing.status, 422);
    assert_eq!(
        raw(addr, "POST", "/items", &json_ct, b"{").await.status,
        400
    );
}

// ---- static files ---------------------------------------------------------------------------

/// A fresh directory tree for one test: `<tmp>/<name>/public/...` plus a
/// file outside the served root.
fn fixture(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("turbo-static-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let public = base.join("public");
    std::fs::create_dir_all(public.join("css")).unwrap();
    std::fs::create_dir_all(public.join("docs")).unwrap();
    std::fs::write(public.join("index.html"), "<h1>home</h1>").unwrap();
    std::fs::write(public.join("css/site.css"), "body{}").unwrap();
    std::fs::write(public.join("docs/index.html"), "docs index").unwrap();
    std::fs::write(public.join("data.bin"), b"0123456789").unwrap();
    std::fs::write(public.join(".env"), "SECRET=1").unwrap();
    std::fs::write(public.join("big.bin"), vec![b'x'; 300 * 1024]).unwrap();
    std::fs::write(base.join("outside.txt"), "top secret").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(base.join("outside.txt"), public.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(public.join("css/site.css"), public.join("inner-link.css"))
            .unwrap();
    }
    base
}

async fn fetch(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> ferroly::http::Response {
    let mut builder = Request::builder(Method::Get, &format!("http://{addr}{path}")).unwrap();
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    Client::new().send(builder.build()).await.unwrap()
}

#[tokio::test]
async fn static_files_validators_conditionals_and_ranges() {
    let base = fixture("serve");
    let router = Router::new().static_files(
        "/assets",
        StaticFiles::new(base.join("public"))
            .index_file("index.html")
            .cache_control("public, max-age=60"),
    );
    let addr = spawn(router).await;

    let css = fetch(addr, "/assets/css/site.css", &[]).await;
    assert_eq!(css.status().as_u16(), 200);
    let content_type = css.headers().get("content-type").unwrap().to_string();
    assert!(content_type.starts_with("text/css"), "{content_type}");
    assert_eq!(css.headers().get("x-content-type-options"), Some("nosniff"));
    assert_eq!(css.headers().get("accept-ranges"), Some("bytes"));
    assert_eq!(
        css.headers().get("cache-control"),
        Some("public, max-age=60")
    );
    let etag = css.headers().get("etag").unwrap().to_string();
    let modified = css.headers().get("last-modified").unwrap().to_string();
    assert!(etag.starts_with("W/\""), "{etag}");
    assert!(modified.ends_with(" GMT"), "{modified}");
    assert_eq!(css.text().await.unwrap(), "body{}");

    // Conditional requests.
    let cached = fetch(addr, "/assets/css/site.css", &[("if-none-match", &etag)]).await;
    assert_eq!(cached.status().as_u16(), 304);
    assert_eq!(cached.headers().get("etag"), Some(etag.as_str()));
    assert_eq!(cached.text().await.unwrap(), "");
    let stale = fetch(
        addr,
        "/assets/css/site.css",
        &[("if-none-match", "\"other\"")],
    )
    .await;
    assert_eq!(stale.status().as_u16(), 200);
    let since = fetch(
        addr,
        "/assets/css/site.css",
        &[("if-modified-since", &modified)],
    )
    .await;
    assert_eq!(since.status().as_u16(), 304);
    let old = fetch(
        addr,
        "/assets/css/site.css",
        &[("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT")],
    )
    .await;
    assert_eq!(old.status().as_u16(), 200);

    // Ranges.
    let part = fetch(addr, "/assets/data.bin", &[("range", "bytes=2-5")]).await;
    assert_eq!(part.status().as_u16(), 206);
    assert_eq!(part.headers().get("content-range"), Some("bytes 2-5/10"));
    assert_eq!(part.text().await.unwrap(), "2345");
    let suffix = fetch(addr, "/assets/data.bin", &[("range", "bytes=-3")]).await;
    assert_eq!(suffix.headers().get("content-range"), Some("bytes 7-9/10"));
    assert_eq!(suffix.text().await.unwrap(), "789");
    let open = fetch(addr, "/assets/data.bin", &[("range", "bytes=8-")]).await;
    assert_eq!(open.text().await.unwrap(), "89");
    let beyond = fetch(addr, "/assets/data.bin", &[("range", "bytes=10-")]).await;
    assert_eq!(beyond.status().as_u16(), 416);
    assert_eq!(beyond.headers().get("content-range"), Some("bytes */10"));
    // Several ranges, or an If-Range that does not match: the whole file.
    let multi = fetch(addr, "/assets/data.bin", &[("range", "bytes=0-1,4-5")]).await;
    assert_eq!(multi.status().as_u16(), 200);
    let mismatch = fetch(
        addr,
        "/assets/data.bin",
        &[("range", "bytes=0-1"), ("if-range", "\"nope\"")],
    )
    .await;
    assert_eq!(mismatch.status().as_u16(), 200);
    assert_eq!(mismatch.text().await.unwrap(), "0123456789");

    // Index files, HEAD, and a file large enough to be streamed.
    assert_eq!(
        fetch(addr, "/assets", &[]).await.text().await.unwrap(),
        "<h1>home</h1>"
    );
    assert_eq!(
        fetch(addr, "/assets/docs/", &[])
            .await
            .text()
            .await
            .unwrap(),
        "docs index"
    );
    let head = raw(addr, "HEAD", "/assets/data.bin", &[], b"").await;
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length").as_deref(), Some("10"));
    assert_eq!(head.body, "");
    let big = fetch(addr, "/assets/big.bin", &[]).await;
    assert_eq!(big.status().as_u16(), 200);
    let bytes = big.bytes().await.unwrap();
    assert_eq!(bytes.len(), 300 * 1024);
    assert!(bytes.iter().all(|b| *b == b'x'));
    let tail = fetch(addr, "/assets/big.bin", &[("range", "bytes=-5")]).await;
    assert_eq!(tail.status().as_u16(), 206);
    assert_eq!(tail.text().await.unwrap(), "xxxxx");

    assert_eq!(
        raw(addr, "POST", "/assets/data.bin", &[], b"").await.status,
        405
    );
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test]
async fn static_files_are_confined_to_the_root() {
    let base = fixture("confine");
    let router = Router::new()
        .static_files("/assets", StaticFiles::new(base.join("public")))
        .get("/outside.txt", |_| async { ok("a different route") });
    let addr = spawn(router).await;

    // Sanity: the root is being served.
    assert_eq!(get(addr, "/assets/data.bin").await.body, "0123456789");

    for target in [
        // Encoded separators and dots never rebuild a traversal.
        "/assets/..%2Foutside.txt",
        "/assets/..%2F..%2Foutside.txt",
        "/assets/%2E%2E%2Foutside.txt",
        "/assets/css%2F..%2F..%2Foutside.txt",
        "/assets/..%5Coutside.txt",
        "/assets/css/..%2F..%2F..%2Foutside.txt",
        // An encoded slash is not a separator, even towards a real file.
        "/assets/css%2Fsite.css",
        // Hidden files.
        "/assets/.env",
        // Directories without an index, and missing files.
        "/assets/css",
        "/assets/",
        "/assets/nope.txt",
        // A symbolic link that leaves the root.
        "/assets/link.txt",
    ] {
        let resp = get(addr, target).await;
        assert_eq!(
            resp.status, 404,
            "{target} -> {} {:?}",
            resp.status, resp.body
        );
        assert!(!resp.body.contains("top secret"), "{target}");
        assert!(!resp.body.contains("SECRET"), "{target}");
    }
    // Literal dot segments are resolved by the router before anything else,
    // so they leave the `/assets` mount rather than the directory.
    assert_eq!(
        get(addr, "/assets/../outside.txt").await.body,
        "a different route"
    );
    assert_eq!(
        get(addr, "/assets/%2e%2e/outside.txt").await.body,
        "a different route"
    );
    assert_eq!(
        get(addr, "/assets/css/../../../outside.txt").await.body,
        "a different route"
    );
    // A link that stays inside the root is fine.
    #[cfg(unix)]
    assert_eq!(get(addr, "/assets/inner-link.css").await.body, "body{}");

    let hidden_ok = Router::new().static_files(
        "/assets",
        StaticFiles::new(base.join("public")).allow_hidden(true),
    );
    let addr = spawn(hidden_ok).await;
    assert_eq!(get(addr, "/assets/.env").await.body, "SECRET=1");
    let _ = std::fs::remove_dir_all(base);
}

// ---- serve variants ---------------------------------------------------------------------------

#[tokio::test]
async fn serve_with_shutdown_drains_and_returns() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let router = Router::new().get("/", |_| async { ok("up") });
    let server = tokio::spawn(
        router.serve_listener(listener, ServerConfig::default(), async {
            let _ = stopped.await;
        }),
    );

    assert_eq!(get(addr, "/").await.body, "up");
    stop.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve returns after shutdown")
        .unwrap();
    assert!(result.is_ok());
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "listener is closed"
    );

    // The address-taking variants bind for themselves.
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let router = Router::new().get("/", |_| async { ok("up") });
    let server = tokio::spawn(router.serve_with_shutdown("127.0.0.1:0", async {
        let _ = stopped.await;
    }));
    stop.send(()).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .is_ok());
}

#[tokio::test]
async fn serve_with_config_applies_server_limits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .post(
            "/echo",
            |ctx| async move { ok(ctx.body().len().to_string()) },
        )
        .get("/slow", |_| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            ok("never")
        });
    let config = ServerConfig {
        max_body_bytes: 16,
        handler_timeout: Some(Duration::from_millis(50)),
        ..ServerConfig::default()
    };
    tokio::spawn(router.serve_listener(listener, config, std::future::pending()));

    assert_eq!(
        raw(addr, "POST", "/echo", &[], &[b'a'; 16]).await.body,
        "16"
    );
    assert_eq!(
        raw(addr, "POST", "/echo", &[], &[b'a'; 17]).await.status,
        413
    );
    assert_eq!(get(addr, "/slow").await.status, 503);
}

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");
const SERVER2_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server2_cert.pem");
const SERVER2_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server2_key.pem");

fn tls_client() -> Client {
    Client::builder()
        .timeout(Some(Duration::from_secs(5)))
        .connect_timeout(Some(Duration::from_secs(5)))
        .pool_max_idle_per_host(0)
        .tls(
            ClientTlsConfig::new()
                .without_web_pki_roots()
                .add_root_pem(CA)
                .unwrap(),
        )
        .build()
        .unwrap()
}

#[tokio::test]
async fn serve_tls_and_reloadable_tls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tls = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .reloadable();
    let router = Router::new().get("/", |ctx| async move {
        ok(format!("tls={}", ctx.is_tls())).header("connection", "close")
    });
    tokio::spawn(router.serve_tls_listener(
        listener,
        tls.clone(),
        ServerConfig::default(),
        std::future::pending(),
    ));

    let url = format!("https://localhost:{port}/");
    let fetch = || async {
        let req = Request::builder(Method::Get, &url).unwrap().build();
        tls_client().send(req).await.unwrap().text().await.unwrap()
    };
    assert_eq!(fetch().await, "tls=true");

    // Rotating the certificate does not need a restart.
    tls.reload(TlsConfig::from_pem(SERVER2_CERT, SERVER2_KEY).unwrap());
    assert_eq!(fetch().await, "tls=true");

    // A plain-HTTP client cannot talk to it.
    let plain = Request::builder(Method::Get, &format!("http://localhost:{port}/"))
        .unwrap()
        .build();
    let client = Client::builder()
        .timeout(Some(Duration::from_secs(2)))
        .build()
        .unwrap();
    assert!(client.send(plain).await.is_err());
}
