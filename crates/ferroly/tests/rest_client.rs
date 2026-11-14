#![cfg(feature = "rest")]
//! REST client regression tests: path-parameter encoding (P0-6), retry
//! semantics (B20), asynchronous auth, timeouts and limits, base URL and
//! defaults, circuit breaking, policies, status handling and pagination.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroly::clients::{AuthError, AuthProvider, CircuitBreakerConfig, Policy, Retry, RetryPolicy};
use ferroly::http::{
    BoxFuture, HeaderMap, HttpError, HttpResponse, Interceptor, Method, Request, ServerConfig,
    StatusCode,
};
use ferroly::rest::{Client, ClientError, ClientOptions};
use ferroly::turbo::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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

fn ok(text: impl Into<String>) -> HttpResponse {
    HttpResponse::text(StatusCode::OK, text)
}

/// A server that records the head of every request it receives, byte for
/// byte, and answers `200 ok`.
async fn capture() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    (addr, seen)
}

fn request_line(head: &str) -> &str {
    head.lines().next().unwrap_or("")
}

// ---- P0-6: path parameters are data ---------------------------------------------------

#[tokio::test]
async fn path_param_values_cannot_change_the_request() {
    let (addr, seen) = capture().await;
    let client = Client::new();
    let cases = [
        ("42", "/items/42/detail"),
        ("a/b", "/items/a%2Fb/detail"),
        ("../../admin", "/items/..%2F..%2Fadmin/detail"),
        ("x?admin=true", "/items/x%3Fadmin%3Dtrue/detail"),
        ("x#frag", "/items/x%23frag/detail"),
        ("a b", "/items/a%20b/detail"),
        ("100%", "/items/100%25/detail"),
        ("%2e%2e", "/items/%252e%252e/detail"),
        (
            "x HTTP/1.1\r\nHost: evil\r\n\r\nGET /admin",
            "/items/x%20HTTP%2F1.1%0D%0AHost%3A%20evil%0D%0A%0D%0AGET%20%2Fadmin/detail",
        ),
        ("caf\u{e9}", "/items/caf%C3%A9/detail"),
    ];
    for (value, expected_target) in cases {
        seen.lock().unwrap().clear();
        let resp = client
            .get(format!("http://{addr}/items/${{id}}/detail"))
            .path_param("id", value)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{value:?}: {e}"));
        assert_eq!(resp.status_code(), 200);
        let heads = seen.lock().unwrap().clone();
        // Exactly one request reached the server, with exactly this line.
        assert_eq!(heads.len(), 1, "{value:?} produced {heads:#?}");
        assert_eq!(
            request_line(&heads[0]),
            format!("GET {expected_target} HTTP/1.1"),
            "{value:?}"
        );
        // And the head holds one request line and one Host header only.
        assert_eq!(heads[0].matches("HTTP/1.1").count(), 1, "{:?}", heads[0]);
        assert_eq!(
            heads[0].to_ascii_lowercase().matches("\nhost:").count(),
            1,
            "{:?}",
            heads[0]
        );
    }

    // Values that cannot be a path segment are refused before anything is sent.
    seen.lock().unwrap().clear();
    for value in ["", ".", ".."] {
        let err = client
            .get(format!("http://{addr}/items/${{id}}/detail"))
            .path_param("id", value)
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(err, ClientError::InvalidRequest(_)),
            "{value:?}: {err}"
        );
    }
    // A placeholder in the host position takes host characters only.
    for value in [
        "evil.test/x",
        "h h",
        "user@evil.test",
        "h\r\nX-Injected: 1",
        "h?x=1",
    ] {
        let err = client
            .get("http://${host}/items")
            .path_param("host", value)
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(err, ClientError::InvalidRequest(_)),
            "{value:?}: {err}"
        );
    }
    assert!(seen.lock().unwrap().is_empty());

    // The supported host form still works.
    let resp = client
        .get("http://${host}/ok")
        .path_param("host", addr.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text(), "ok");

    // Query placeholders and query() values are encoded as components.
    seen.lock().unwrap().clear();
    client
        .get(format!("http://{addr}/s?q=${{q}}"))
        .path_param("q", "a&b=c d")
        .query("k&ey", "v=1&x")
        .send()
        .await
        .unwrap();
    assert_eq!(
        request_line(&seen.lock().unwrap()[0]),
        "GET /s?q=a%26b%3Dc%20d&k%26ey=v%3D1%26x HTTP/1.1"
    );
}

// ---- B20: retry behaviour matches the documentation ------------------------------------

/// A server whose `/flaky` answers `failures` times with `status`, then 200,
/// counting every hit and recording the `Idempotency-Key` it saw.
struct Flaky {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    keys: Arc<Mutex<Vec<String>>>,
}

async fn flaky(failures: usize, status: u16, retry_after: Option<&'static str>) -> Flaky {
    let hits = Arc::new(AtomicUsize::new(0));
    let keys = Arc::new(Mutex::new(Vec::new()));
    let (counter, seen) = (hits.clone(), keys.clone());
    let handler = move |ctx: ferroly::turbo::Ctx| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if let Some(key) = ctx.header("idempotency-key") {
            seen.lock().unwrap().push(key.to_string());
        }
        async move {
            if n < failures {
                let resp = HttpResponse::text(StatusCode(status), "try later");
                match retry_after {
                    Some(value) => resp.header("retry-after", value),
                    None => resp,
                }
            } else {
                ok(format!("served after {n} failures"))
            }
        }
    };
    let router = Router::new().add(
        "/flaky",
        handler,
        vec![Method::Get, Method::Post, Method::Put, Method::Delete],
    );
    Flaky {
        addr: spawn(router).await,
        hits,
        keys,
    }
}

fn retrying(retries: u32) -> Client {
    Client::with_options(
        ClientOptions::builder()
            .retry_policy(RetryPolicy::fixed(retries, Duration::from_millis(1)))
            .build(),
    )
}

#[tokio::test]
async fn idempotent_requests_are_retried_on_502_503_504_and_429() {
    for status in [502, 503, 504, 429] {
        let server = flaky(2, status, None).await;
        let resp = retrying(3)
            .get(format!("http://{}/flaky", server.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status_code(), 200, "after {status}");
        assert_eq!(resp.text(), "served after 2 failures");
        assert_eq!(server.hits.load(Ordering::SeqCst), 3, "after {status}");
    }

    // PUT and DELETE are idempotent too.
    let server = flaky(1, 503, None).await;
    let client = retrying(3);
    let url = format!("http://{}/flaky", server.addr);
    assert_eq!(client.put(&url).send().await.unwrap().status_code(), 200);
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);

    // Other statuses are not retried: a 500 may mean the work was done.
    for status in [500, 501, 400, 404] {
        let server = flaky(5, status, None).await;
        let resp = retrying(3)
            .get(format!("http://{}/flaky", server.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status_code(), status);
        assert_eq!(server.hits.load(Ordering::SeqCst), 1, "{status}");
    }

    // When the retries run out, the last response is what the caller gets.
    let server = flaky(100, 503, None).await;
    let resp = retrying(2)
        .get(format!("http://{}/flaky", server.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status_code(), 503);
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);

    // Without a retry policy nothing is retried.
    let server = flaky(1, 503, None).await;
    let resp = Client::new()
        .get(format!("http://{}/flaky", server.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status_code(), 503);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn non_idempotent_requests_are_retried_only_on_opt_in() {
    let server = flaky(2, 503, None).await;
    let client = retrying(3);
    let url = format!("http://{}/flaky", server.addr);

    // A POST is sent exactly once.
    let resp = client
        .post(&url)
        .body_bytes(b"x".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status_code(), 503);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);

    // With an idempotency key it is retried, and every attempt carries the key.
    let server = flaky(2, 503, None).await;
    let url = format!("http://{}/flaky", server.addr);
    let resp = client
        .post(&url)
        .body_bytes(b"x".to_vec())
        .idempotency_key("order-7f3a")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status_code(), 200);
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
    assert_eq!(*server.keys.lock().unwrap(), vec!["order-7f3a"; 3]);

    // `idempotent(true)` opts in without a key; `idempotent(false)` opts a
    // GET out.
    let server = flaky(1, 503, None).await;
    let url = format!("http://{}/flaky", server.addr);
    assert_eq!(
        client
            .post(&url)
            .idempotent(true)
            .send()
            .await
            .unwrap()
            .status_code(),
        200
    );
    let server = flaky(1, 503, None).await;
    let url = format!("http://{}/flaky", server.addr);
    assert_eq!(
        client
            .get(&url)
            .idempotent(false)
            .send()
            .await
            .unwrap()
            .status_code(),
        503
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_after_is_honoured_and_capped() {
    // The server asks for one second; the policy's own delay is 1 ms.
    let server = flaky(1, 429, Some("1")).await;
    let started = Instant::now();
    let resp = retrying(2)
        .get(format!("http://{}/flaky", server.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status_code(), 200);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "waited only {:?}",
        started.elapsed()
    );

    // A delay beyond the cap is not waited out (and not cut short either):
    // the response is returned.
    let server = flaky(5, 503, Some("3600")).await;
    let capped = Client::with_options(
        ClientOptions::builder()
            .retry_policy(
                Retry::new(RetryPolicy::fixed(3, Duration::from_millis(1)))
                    .max_retry_after(Some(Duration::from_millis(100))),
            )
            .build(),
    );
    let started = Instant::now();
    let resp = capped
        .get(format!("http://{}/flaky", server.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status_code(), 503);
    assert_eq!(resp.header("retry-after"), Some("3600"));
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ---- asynchronous auth -----------------------------------------------------------------

/// A provider whose token only exists after an asynchronous `prepare`.
struct Refreshing {
    token: Mutex<Option<String>>,
    refreshes: AtomicUsize,
    fail: bool,
}

impl AuthProvider for Refreshing {
    fn apply(&self, req: &mut Request) {
        if let Some(token) = self.token.lock().unwrap().as_deref() {
            req.headers.set("authorization", format!("Bearer {token}"));
        }
    }

    fn prepare(&self) -> BoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(5)).await; // the token endpoint
            if self.fail {
                return Err(AuthError::Transport("token endpoint unreachable".into()));
            }
            let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            *self.token.lock().unwrap() = Some(format!("token-{n}"));
            Ok(())
        })
    }
}

#[tokio::test]
async fn refreshing_auth_providers_are_prepared_before_each_attempt() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let router = Router::new().get("/whoami", move |ctx| {
        counter.fetch_add(1, Ordering::SeqCst);
        async move { ok(ctx.header("authorization").unwrap_or("none").to_string()) }
    });
    let addr = spawn(router).await;

    let provider = Arc::new(Refreshing {
        token: Mutex::new(None),
        refreshes: AtomicUsize::new(0),
        fail: false,
    });
    let client = Client::with_options(ClientOptions::builder().auth(provider.clone()).build());
    let url = format!("http://{addr}/whoami");
    assert_eq!(
        client.get(&url).send().await.unwrap().text(),
        "Bearer token-1"
    );
    assert_eq!(
        client.get(&url).send().await.unwrap().text(),
        "Bearer token-2"
    );

    // A provider that cannot get credentials fails the request without
    // sending it unauthenticated.
    let failing = Client::with_options(
        ClientOptions::builder()
            .auth(Arc::new(Refreshing {
                token: Mutex::new(None),
                refreshes: AtomicUsize::new(0),
                fail: true,
            }))
            .build(),
    );
    let before = hits.load(Ordering::SeqCst);
    let err = failing.get(&url).send().await.unwrap_err();
    assert!(
        matches!(err, ClientError::Auth(AuthError::Transport(_))),
        "{err}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), before);
}

// ---- timeouts and limits ---------------------------------------------------------------

/// A listener that accepts connections and never answers.
async fn black_hole() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    addr
}

#[tokio::test]
async fn timeouts_and_body_cap_are_configurable() {
    let silent = black_hole().await;
    let client = Client::with_options(
        ClientOptions::builder()
            .request_timeout(Duration::from_millis(100))
            .build(),
    );
    let started = Instant::now();
    let err = client
        .get(format!("http://{silent}/"))
        .send()
        .await
        .unwrap_err();
    assert!(err.is_timeout(), "{err}");
    assert!(matches!(err, ClientError::Transport(HttpError::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(5));

    // A body that keeps trickling is bounded by the total timeout.
    let router = Router::new()
        .get("/drip", |_| async {
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
            tokio::spawn(async move {
                loop {
                    if tx.send(b"drip".to_vec()).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
            HttpResponse::stream(StatusCode::OK, rx)
        })
        .get("/big", |_| async { ok("x".repeat(4096)) });
    let addr = spawn(router).await;
    let client = Client::with_options(
        ClientOptions::builder()
            .total_timeout(Duration::from_millis(200))
            .connect_timeout(Duration::from_secs(2))
            .read_timeout(Duration::from_secs(2))
            .build(),
    );
    let started = Instant::now();
    let err = client
        .get(format!("http://{addr}/drip"))
        .send()
        .await
        .unwrap_err();
    assert!(err.is_timeout(), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));

    let capped = Client::with_options(
        ClientOptions::builder()
            .max_response_body_bytes(1024)
            .build(),
    );
    let err = capped
        .get(format!("http://{addr}/big"))
        .send()
        .await
        .unwrap_err();
    assert!(
        matches!(err, ClientError::Transport(HttpError::BodyTooLarge(1024))),
        "{err}"
    );
    // The default client has a (larger) cap and reads this body fine.
    assert_eq!(
        Client::new()
            .get(format!("http://{addr}/big"))
            .send()
            .await
            .unwrap()
            .raw()
            .len(),
        4096
    );
}

// ---- base URL, defaults, headers, status ------------------------------------------------

struct Stamp;

impl Interceptor for Stamp {
    fn on_request(&self, req: &mut Request) -> Result<(), HttpError> {
        req.headers.set("x-intercepted", "yes");
        Ok(())
    }
    fn on_response(&self, _req: &Request, _status: StatusCode, _headers: &HeaderMap) {}
}

fn echo_router() -> Router {
    Router::new()
        .add(
            "/v1/echo/:id",
            |ctx| async move {
                let header = |name: &str| ctx.header(name).unwrap_or("-").to_string();
                HttpResponse::text(
                    StatusCode::OK,
                    format!(
                        "{} {} q={} accept={} tenant={} trace={} stamp={} ct={}",
                        ctx.method().as_str(),
                        ctx.param("id").unwrap_or("-"),
                        ctx.raw_query().unwrap_or("-"),
                        header("accept"),
                        header("x-tenant"),
                        header("x-trace"),
                        header("x-intercepted"),
                        header("content-type"),
                    ),
                )
                .header("x-served-by", "echo")
            },
            vec![
                Method::Get,
                Method::Post,
                Method::Patch,
                Method::Delete,
                Method::Options,
            ],
        )
        .get("/v1/missing", |_| async {
            HttpResponse::new(StatusCode::NOT_FOUND)
                .header("content-type", "application/json")
                .body(br#"{"error":"no such thing"}"#.to_vec())
        })
        .get("/v1/moved", |_| async {
            HttpResponse::new(StatusCode::FOUND).header("location", "/v1/echo/after-redirect")
        })
}

#[tokio::test]
async fn base_url_defaults_accept_and_response_headers() {
    let addr = spawn(echo_router()).await;
    let client = Client::with_options(
        ClientOptions::builder()
            .base_url(format!("http://{addr}/v1/"))
            .default_header("x-tenant", "acme")
            .default_header("x-trace", "default")
            .default_query("api-version", "2024-01")
            .interceptor(Stamp)
            .user_agent("rest-tests/1.0")
            .build(),
    );

    let resp = client
        .get("/echo/${id}")
        .path_param("id", "a b")
        .header("x-trace", "per-request")
        .query("page", "2")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.text(),
        "GET a b q=api-version=2024-01&page=2 accept=application/json, */*;q=0.5 \
         tenant=acme trace=per-request stamp=yes ct=-"
    );
    // Response headers, status and final URL are all available.
    assert_eq!(resp.header("x-served-by"), Some("echo"));
    assert_eq!(resp.headers().get("X-Served-By"), Some("echo"));
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.url(),
        format!("http://{addr}/v1/echo/a%20b?api-version=2024-01&page=2")
    );

    // The Accept header follows the request's codec, or an explicit choice.
    let resp = client
        .get("echo/1")
        .content_type("application/xml")
        .send()
        .await
        .unwrap();
    assert!(
        resp.text().contains("accept=application/xml, */*;q=0.5"),
        "{}",
        resp.text()
    );
    let resp = client
        .get("echo/1")
        .accept("text/plain")
        .send()
        .await
        .unwrap();
    assert!(
        resp.text().contains("accept=text/plain "),
        "{}",
        resp.text()
    );
    let resp = client
        .get("echo/1")
        .header("Accept", "image/png")
        .send()
        .await
        .unwrap();
    assert!(resp.text().contains("accept=image/png "), "{}", resp.text());

    // An absolute URL bypasses the base; the shorthands exist for all verbs.
    let absolute = format!("http://{addr}/v1/echo/abs");
    assert!(client
        .get(&absolute)
        .send()
        .await
        .unwrap()
        .text()
        .starts_with("GET abs "));
    assert!(client
        .patch("echo/p")
        .body_bytes(b"{}".to_vec())
        .send()
        .await
        .unwrap()
        .text()
        .starts_with("PATCH p "));
    assert!(client
        .delete("echo/d")
        .send()
        .await
        .unwrap()
        .text()
        .starts_with("DELETE d "));
    assert!(client
        .options("echo/o")
        .send()
        .await
        .unwrap()
        .text()
        .starts_with("OPTIONS o "));
    let head = client.head("echo/h").send().await.unwrap();
    assert_eq!(head.status_code(), 200);
    assert!(head.raw().is_empty());
    assert_eq!(head.header("x-served-by"), Some("echo"));

    // Redirects are opt-in.
    assert_eq!(client.get("moved").send().await.unwrap().status_code(), 302);
    let following = Client::with_options(
        ClientOptions::builder()
            .base_url(format!("http://{addr}/v1"))
            .max_redirects(2)
            .build(),
    );
    let resp = following.get("moved").send().await.unwrap();
    assert!(
        resp.text().starts_with("GET after-redirect "),
        "{}",
        resp.text()
    );
    assert_eq!(resp.url(), format!("http://{addr}/v1/echo/after-redirect"));
}

#[tokio::test]
async fn error_for_status_makes_failures_explicit() {
    let addr = spawn(echo_router()).await;
    let url = format!("http://{addr}/v1/missing");

    // Default: a 404 is a response.
    let resp = Client::new().get(&url).send().await.unwrap();
    assert_eq!(resp.status_code(), 404);
    assert!(!resp.is_success());

    // Explicit, per response.
    let err = resp.error_for_status().unwrap_err();
    assert_eq!(err.status(), Some(404));
    assert_eq!(err.body(), Some(&br#"{"error":"no such thing"}"#[..]));
    assert_eq!(err.to_string(), "unexpected response status 404");

    // Or for every request of a client.
    let strict = Client::with_options(ClientOptions::builder().error_for_status(true).build());
    let err = strict.get(&url).send().await.unwrap_err();
    assert!(
        matches!(err, ClientError::Status { status: 404, .. }),
        "{err}"
    );
    let fine = strict
        .get(format!("http://{addr}/v1/echo/1"))
        .send()
        .await
        .unwrap();
    assert!(fine.is_success());
    assert!(fine.error_for_status().is_ok());
}

#[tokio::test]
async fn try_with_options_reports_unusable_transport_options() {
    use ferroly::http::ClientTlsConfig;

    // No trust root at all: there is no way to verify a server.
    let unusable = || {
        ClientOptions::builder()
            .tls(ClientTlsConfig::new().without_web_pki_roots())
            .build()
    };
    assert!(matches!(
        Client::try_with_options(unusable()),
        Err(ClientError::InvalidRequest(_))
    ));
    // The infallible constructor defers the same error to the request.
    let client = Client::with_options(unusable());
    let err = client.get("http://127.0.0.1:1/").send().await.unwrap_err();
    assert!(matches!(err, ClientError::InvalidRequest(_)), "{err}");
    assert!(Client::try_with_options(ClientOptions::default()).is_ok());
}

// ---- circuit breaker and policy ---------------------------------------------------------

#[tokio::test]
async fn circuit_breaker_is_per_host_and_ignores_caller_errors() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let failing = Router::new()
        .get("/boom", move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { HttpResponse::text(StatusCode::INTERNAL_SERVER_ERROR, "boom") }
        })
        .get("/missing", |_| async {
            HttpResponse::new(StatusCode::NOT_FOUND)
        });
    let bad = spawn(failing).await;
    let good = spawn(Router::new().get("/ok", |_| async { ok("fine") })).await;

    let client = Client::with_options(
        ClientOptions::builder()
            .circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 2,
                timeout: Duration::from_secs(60),
                ..CircuitBreakerConfig::default()
            })
            .build(),
    );

    // Caller errors — 404s — never open the circuit.
    for _ in 0..10 {
        let resp = client
            .get(format!("http://{bad}/missing"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status_code(), 404);
    }

    // Two server failures do.
    for _ in 0..2 {
        let resp = client
            .get(format!("http://{bad}/boom"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status_code(), 500);
    }
    let err = client
        .get(format!("http://{bad}/boom"))
        .send()
        .await
        .unwrap_err();
    match &err {
        ClientError::CircuitOpen(host) => assert_eq!(host, &bad.to_string()),
        other => panic!("expected an open circuit, got {other}"),
    }
    // The request was not sent, and the open circuit covers the whole host.
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(matches!(
        client.get(format!("http://{bad}/missing")).send().await,
        Err(ClientError::CircuitOpen(_))
    ));
    // Another host is unaffected.
    assert_eq!(
        client
            .get(format!("http://{good}/ok"))
            .send()
            .await
            .unwrap()
            .text(),
        "fine"
    );
}

#[tokio::test]
async fn a_policy_drives_retries_and_deadlines() {
    let server = flaky(2, 503, None).await;
    let url = format!("http://{}/flaky", server.addr);
    let client = Client::with_options(
        ClientOptions::builder()
            .policy(Policy::new().retry(RetryPolicy::fixed(3, Duration::from_millis(1))))
            .build(),
    );
    assert_eq!(client.get(&url).send().await.unwrap().status_code(), 200);
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);

    // The policy's retry follows the same idempotency rule.
    let server = flaky(2, 503, None).await;
    let url = format!("http://{}/flaky", server.addr);
    assert_eq!(client.post(&url).send().await.unwrap().status_code(), 503);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);

    // An overall deadline on the policy.
    let silent = black_hole().await;
    let deadline = Client::with_options(
        ClientOptions::builder()
            .policy(Policy::new().timeout(Duration::from_millis(100)))
            .build(),
    );
    let started = Instant::now();
    let err = deadline
        .get(format!("http://{silent}/"))
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::Timeout), "{err}");
    assert!(err.is_timeout());
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ---- pagination ---------------------------------------------------------------------------

fn paged_router(other_origin: Option<String>) -> Router {
    Router::new()
        .get("/items", move |ctx| {
            let page = ctx.query_int("page").unwrap_or(1);
            let tenant = ctx.header("x-tenant").unwrap_or("-").to_string();
            let other_origin = other_origin.clone();
            async move {
                let resp = HttpResponse::new(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(format!(r#"{{"page":{page},"tenant":"{tenant}"}}"#).into_bytes());
                match (page, other_origin) {
                    (1, _) => resp.header(
                        "link",
                        r#"</items?page=2>; rel="next", </items?page=3>; rel="last""#,
                    ),
                    (2, None) => resp.header("link", r#"<?page=3>; rel="next""#),
                    (2, Some(origin)) => {
                        resp.header("link", format!(r#"<{origin}/items?page=3>; rel="next""#))
                    }
                    _ => resp,
                }
            }
        })
        .get("/cursor", |ctx| async move {
            let cursor = ctx.query_param("cursor").unwrap_or_else(|| "a".to_string());
            let next = match cursor.as_str() {
                "a" => r#""b""#,
                "b" => r#""c""#,
                _ => "null",
            };
            HttpResponse::new(StatusCode::OK)
                .header("content-type", "application/json")
                .body(format!(r#"{{"cursor":"{cursor}","next":{next}}}"#).into_bytes())
        })
        .get("/broken", |ctx| async move {
            if ctx.query_param("page").is_some() {
                HttpResponse::text(StatusCode::INTERNAL_SERVER_ERROR, "boom")
            } else {
                ok("first").header("link", r#"</broken?page=2>; rel="next""#)
            }
        })
}

#[tokio::test]
async fn pagination_follows_links_and_cursors() {
    use ferroly::codec::Value;

    let addr = spawn(paged_router(None)).await;
    let client = Client::new();
    let base = format!("http://{addr}");

    // Link headers: absolute-path and query-only references both resolve,
    // and the first request's headers are repeated.
    let pages = client
        .paginate_links(
            client
                .get(format!("{base}/items"))
                .header("x-tenant", "acme"),
            10,
        )
        .await
        .unwrap();
    let bodies: Vec<String> = pages.iter().map(|p| p.text()).collect();
    assert_eq!(
        bodies,
        [
            r#"{"page":1,"tenant":"acme"}"#,
            r#"{"page":2,"tenant":"acme"}"#,
            r#"{"page":3,"tenant":"acme"}"#,
        ]
    );
    assert_eq!(pages[0].next_link(), Some(format!("{base}/items?page=2")));
    assert_eq!(pages[0].link("last"), Some(format!("{base}/items?page=3")));
    assert_eq!(pages[2].next_link(), None);

    // The page limit stops a never-ending collection.
    let limited = client
        .paginate_links(client.get(format!("{base}/items")), 2)
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);

    // The cursor form.
    let pages = client
        .paginate(client.get(format!("{base}/cursor")), 10, |page| {
            let body: Value = page.decode().ok()?;
            let next = body.get("next")?.as_str()?.to_string();
            Some(client.get(format!("{base}/cursor")).query("cursor", next))
        })
        .await
        .unwrap();
    let cursors: Vec<String> = pages
        .iter()
        .map(|p| {
            let body: Value = p.decode().unwrap();
            body.get("cursor")
                .and_then(Value::as_str)
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(cursors, ["a", "b", "c"]);

    // A failing page is an error, not the end of the list.
    let err = client
        .paginate_links(client.get(format!("{base}/broken")), 10)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ClientError::Status { status: 500, .. }),
        "{err}"
    );

    // A next link to another origin is not followed.
    let elsewhere = spawn(Router::new().get("/items", |_| async { ok("stolen") })).await;
    let addr = spawn(paged_router(Some(format!("http://{elsewhere}")))).await;
    let err = client
        .paginate_links(client.get(format!("http://{addr}/items")), 10)
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::InvalidRequest(_)), "{err}");
}

// ---- trace propagation --------------------------------------------------------------------

#[cfg(feature = "obs")]
#[tokio::test]
async fn trace_context_is_injected_into_outgoing_requests() {
    use ferroly::obs::{self, Instrument, Span};

    let router = Router::new().get("/tp", |ctx| async move {
        ok(ctx.header("traceparent").unwrap_or("none").to_string())
    });
    let addr = spawn(router).await;
    let client = Client::new();
    let url = format!("http://{addr}/tp");

    // Inside a span, the request joins that trace as a child span.
    let (seen, trace_id) = async {
        let trace_id = obs::current().map(|c| c.trace_id().to_string());
        let seen = client.get(&url).send().await.unwrap().text();
        (seen, trace_id.unwrap())
    }
    .instrument(Span::new("caller"))
    .await;
    let parts: Vec<&str> = seen.split('-').collect();
    assert_eq!(parts.len(), 4, "{seen}");
    assert_eq!(parts[0], "00");
    assert_eq!(parts[1], trace_id);

    // A traceparent the caller set explicitly is left alone.
    let explicit = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let seen = client
        .get(&url)
        .header("traceparent", explicit)
        .send()
        .await
        .unwrap()
        .text();
    assert_eq!(seen, explicit);
}
