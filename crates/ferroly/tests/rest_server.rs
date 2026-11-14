#![cfg(feature = "rest")]
//! REST server regression tests: restartability and cancel-safety as a
//! lifecycle component, surfaced start errors, asynchronous health probes,
//! TLS and `ServerConfig` passthrough, and the builder's routing wrappers.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferroly::http::{ClientTlsConfig, HttpResponse, Method, ServerConfig, StatusCode, TlsConfig};
use ferroly::lifecycle::{
    CheckOptions, Component, ComponentManager, ComponentState, HealthRegistry, HealthStatus,
    LifecycleError, ProbeKind,
};
use ferroly::rest::{Client, ClientOptions, Cors, SecurityHeaders, Server, ServerOptions};

fn local() -> ServerOptions {
    ServerOptions {
        listen_host: "127.0.0.1".into(),
        listen_port: 0,
        ..Default::default()
    }
}

fn ok(text: &'static str) -> HttpResponse {
    HttpResponse::text(StatusCode::OK, text)
}

async fn body(client: &Client, url: String) -> (u16, String) {
    let resp = client.get(url).send().await.unwrap();
    (resp.status_code(), resp.text())
}

// ---- lifecycle ------------------------------------------------------------------------------

#[tokio::test]
async fn server_can_be_stopped_and_started_again() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let server = Server::builder(local())
        .get("/ping", move |_| {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            async move { HttpResponse::text(StatusCode::OK, format!("pong {n}")) }
        })
        .build();
    let client = Client::new();
    assert_eq!(server.state(), ComponentState::Unknown);
    assert_eq!(server.local_addr(), None);

    for round in 1..=3 {
        server.start().await.unwrap();
        assert_eq!(server.state(), ComponentState::Running);
        assert_eq!(server.health(), None, "healthy while serving");
        let addr = server.local_addr().unwrap();
        assert_eq!(
            body(&client, format!("http://{addr}/ping")).await,
            (200, format!("pong {round}"))
        );

        // A second start while running is refused, and changes nothing.
        assert!(matches!(
            server.start().await,
            Err(LifecycleError::ComponentAlreadyStarted(_))
        ));
        assert_eq!(server.state(), ComponentState::Running);

        server.stop().await.unwrap();
        assert_eq!(server.state(), ComponentState::Stopped);
        assert!(
            client
                .get(format!("http://{addr}/ping"))
                .send()
                .await
                .is_err(),
            "the listener is closed after stop"
        );
        // Stopping a stopped server is a no-op.
        server.stop().await.unwrap();
    }
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_cancelled_start_leaves_the_server_startable() {
    // `localhost` needs a (blocking-pool) name lookup, so the first poll of
    // `start` is pending and the timeout below cancels it mid-way.
    let server = Server::builder(ServerOptions {
        listen_host: "localhost".into(),
        listen_port: 0,
        ..Default::default()
    })
    .get("/ping", |_| async { ok("pong") })
    .build();

    for _ in 0..5 {
        let cancelled = tokio::time::timeout(Duration::ZERO, server.start()).await;
        if cancelled.is_ok() {
            // The lookup won the race: a complete start. Reset and try again.
            server.stop().await.unwrap();
            continue;
        }
        assert_ne!(server.state(), ComponentState::Running);
    }

    // Whatever happened above, a real start works and serves.
    server.start().await.unwrap();
    assert_eq!(server.state(), ComponentState::Running);
    let port = server.local_addr().unwrap().port();
    assert_eq!(
        body(&Client::new(), format!("http://localhost:{port}/ping")).await,
        (200, "pong".to_string())
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn a_cancelled_stop_can_be_repeated() {
    let entered = Arc::new(AtomicBool::new(false));
    let flag = entered.clone();
    let server = Arc::new(
        Server::builder(local().server_config(ServerConfig {
            drain_timeout: Duration::from_secs(5),
            ..ServerConfig::default()
        }))
        .get("/slow", move |_| {
            flag.store(true, Ordering::SeqCst);
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                ok("slow done")
            }
        })
        .get("/ping", |_| async { ok("pong") })
        .build(),
    );
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();

    // An in-flight request makes `stop` wait for the drain.
    let in_flight =
        tokio::spawn(async move { body(&Client::new(), format!("http://{addr}/slow")).await });
    while !entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Give up on the first stop almost at once …
    assert!(
        tokio::time::timeout(Duration::from_millis(20), server.stop())
            .await
            .is_err()
    );
    // … the shutdown keeps going, the request still completes …
    assert_eq!(in_flight.await.unwrap(), (200, "slow done".to_string()));
    // … and a second stop finishes the job.
    server.stop().await.unwrap();
    assert_eq!(server.state(), ComponentState::Stopped);

    // Startable again afterwards.
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();
    assert_eq!(
        body(&Client::new(), format!("http://{addr}/ping")).await,
        (200, "pong".to_string())
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn start_errors_are_reported_and_recoverable() {
    // Occupy a port, then ask a server to bind it.
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = taken.local_addr().unwrap().port();
    let server = Server::builder(ServerOptions {
        id: "busy".into(),
        listen_host: "127.0.0.1".into(),
        listen_port: port,
        ..Default::default()
    })
    .get("/ping", |_| async { ok("pong") })
    .build();

    let err = server.start().await.unwrap_err();
    assert!(
        matches!(&err, LifecycleError::ComponentFailure { id, .. } if id == "busy"),
        "{err}"
    );
    assert_ne!(server.state(), ComponentState::Running);
    assert_eq!(server.local_addr(), None);

    // Once the port is free the very same server starts.
    drop(taken);
    server.start().await.unwrap();
    assert_eq!(
        body(&Client::new(), format!("http://127.0.0.1:{port}/ping")).await,
        (200, "pong".to_string())
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn server_runs_under_a_component_manager() {
    let server = Arc::new(
        Server::builder(ServerOptions {
            id: "api".into(),
            ..local()
        })
        .get("/ping", |_| async { ok("pong") })
        .build(),
    );
    let manager = ComponentManager::new();
    manager.register(server.clone()).unwrap();

    for _ in 0..2 {
        manager.start_all().await.unwrap();
        assert_eq!(manager.get_state("api"), Some(ComponentState::Running));
        assert!(manager.is_ready());
        let addr = server.local_addr().unwrap();
        assert_eq!(
            body(&Client::new(), format!("http://{addr}/ping")).await,
            (200, "pong".to_string())
        );
        manager.stop_all().await.unwrap();
        assert_eq!(manager.get_state("api"), Some(ComponentState::Stopped));
    }
}

// ---- health probes --------------------------------------------------------------------------

#[tokio::test]
async fn health_endpoints_await_async_checks_and_split_probes() {
    let db_up = Arc::new(AtomicBool::new(false));
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = HealthRegistry::new();
    // Liveness: the process is alive.
    registry.register_with(
        "process",
        CheckOptions::new().probes(&[ProbeKind::Liveness]),
        || HealthStatus::Up,
    );
    // Readiness: an asynchronous dependency check.
    let (flag, counter) = (db_up.clone(), runs.clone());
    registry.register_async(
        "db",
        CheckOptions::new()
            .probes(&[ProbeKind::Readiness])
            .timeout(Duration::from_secs(2)),
        move || {
            let (flag, counter) = (flag.clone(), counter.clone());
            async move {
                tokio::time::sleep(Duration::from_millis(5)).await; // a ping
                counter.fetch_add(1, Ordering::SeqCst);
                if flag.load(Ordering::SeqCst) {
                    HealthStatus::Up
                } else {
                    HealthStatus::Down
                }
            }
        },
    );

    let server = Server::builder(ServerOptions {
        path_prefix: "/api".into(),
        ..local()
    })
    .health_endpoints(registry)
    .build();
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();
    let client = Client::new();

    // The dependency is down: not ready, but alive (no restart).
    let (status, text) = body(&client, format!("http://{addr}/ready")).await;
    assert_eq!((status, text.as_str()), (503, "not ready"));
    let (status, text) = body(&client, format!("http://{addr}/health")).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains(r#""overall":"up""#), "{text}");
    assert!(text.contains(r#""process":"up""#), "{text}");
    assert!(
        !text.contains("db"),
        "liveness does not include readiness checks: {text}"
    );
    // The asynchronous check really ran for the readiness request.
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The dependency recovers.
    db_up.store(true, Ordering::SeqCst);
    assert_eq!(
        body(&client, format!("http://{addr}/ready")).await,
        (200, "ready".to_string())
    );
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    // Probes are unprefixed.
    assert_eq!(
        body(&client, format!("http://{addr}/api/ready")).await.0,
        404
    );
    server.stop().await.unwrap();

    // Two registries: a failed liveness check answers 503 with the report.
    let liveness = HealthRegistry::new();
    liveness.register("deadlock-detector", || HealthStatus::Down);
    let readiness = HealthRegistry::new();
    readiness.register("cache", || HealthStatus::Degraded);
    let server = Server::builder(local())
        .health_endpoints_split(liveness, readiness)
        .build();
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();
    let (status, text) = body(&client, format!("http://{addr}/health")).await;
    assert_eq!(status, 503);
    assert!(text.contains(r#""deadlock-detector":"down""#), "{text}");
    // Degraded is not ready.
    assert_eq!(body(&client, format!("http://{addr}/ready")).await.0, 503);
    server.stop().await.unwrap();
}

// ---- TLS and server configuration -----------------------------------------------------------

const CA: &[u8] = include_bytes!("../src/http/testdata/test_ca_cert.pem");
const SERVER_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server_cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server_key.pem");
const SERVER2_CERT: &[u8] = include_bytes!("../src/http/testdata/test_server2_cert.pem");
const SERVER2_KEY: &[u8] = include_bytes!("../src/http/testdata/test_server2_key.pem");

fn tls_client() -> Client {
    Client::try_with_options(
        ClientOptions::builder()
            .tls(
                ClientTlsConfig::new()
                    .without_web_pki_roots()
                    .add_root_pem(CA)
                    .unwrap(),
            )
            .request_timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .build(),
    )
    .unwrap()
}

#[tokio::test]
async fn server_terminates_tls_and_can_rotate_certificates() {
    let who = |ctx: ferroly::rest::Ctx| async move {
        HttpResponse::text(StatusCode::OK, format!("tls={}", ctx.is_tls()))
            .header("connection", "close")
    };

    // A fixed certificate.
    let server =
        Server::builder(local().tls(TlsConfig::from_pem(SERVER_CERT, SERVER_KEY).unwrap()))
            .get("/", who)
            .build();
    assert!(server.is_tls());
    server.start().await.unwrap();
    let port = server.local_addr().unwrap().port();
    assert_eq!(
        body(&tls_client(), format!("https://localhost:{port}/")).await,
        (200, "tls=true".to_string())
    );
    // Without the CA the certificate is not trusted; plain HTTP does not work.
    assert!(Client::new()
        .get(format!("https://localhost:{port}/"))
        .send()
        .await
        .is_err());
    let plain = Client::with_options(
        ClientOptions::builder()
            .request_timeout(Duration::from_secs(2))
            .build(),
    );
    assert!(plain
        .get(format!("http://localhost:{port}/"))
        .send()
        .await
        .is_err());
    server.stop().await.unwrap();
    // TLS survives a restart.
    server.start().await.unwrap();
    let port = server.local_addr().unwrap().port();
    assert_eq!(
        body(&tls_client(), format!("https://localhost:{port}/"))
            .await
            .0,
        200
    );
    server.stop().await.unwrap();

    // A reloadable configuration: rotate while serving.
    let reloadable = TlsConfig::from_pem(SERVER_CERT, SERVER_KEY)
        .unwrap()
        .reloadable();
    let server = Server::builder(local().tls(reloadable.clone()))
        .get("/", who)
        .build();
    server.start().await.unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("https://localhost:{port}/");
    assert_eq!(body(&tls_client(), url.clone()).await.0, 200);
    reloadable.reload(TlsConfig::from_pem(SERVER2_CERT, SERVER2_KEY).unwrap());
    assert_eq!(body(&tls_client(), url).await.0, 200);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn server_config_limits_are_passed_through() {
    let reported = Arc::new(AtomicUsize::new(0));
    let hook = reported.clone();
    let config = ServerConfig {
        max_body_bytes: 8,
        handler_timeout: Some(Duration::from_millis(50)),
        on_error: Some(ferroly::http::ErrorHook::new(move |_peer, _err| {
            hook.fetch_add(1, Ordering::SeqCst);
        })),
        ..ServerConfig::default()
    };
    let server = Server::builder(local().server_config(config))
        .post("/echo", |ctx| async move {
            HttpResponse::text(StatusCode::OK, ctx.body().len().to_string())
        })
        .get("/slow", |_| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            ok("never")
        })
        .build();
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();
    let client = Client::new();

    let small = client
        .post(format!("http://{addr}/echo"))
        .body_bytes(b"12345678".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!((small.status_code(), small.text().as_str()), (200, "8"));
    let large = client
        .post(format!("http://{addr}/echo"))
        .body_bytes(b"123456789".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(large.status_code(), 413);
    assert_eq!(body(&client, format!("http://{addr}/slow")).await.0, 503);
    // Both refusals went through the configured error hook.
    assert!(reported.load(Ordering::SeqCst) >= 2);
    server.stop().await.unwrap();
}

// ---- builder wrappers -----------------------------------------------------------------------

#[tokio::test]
async fn builder_wrappers_prefix_consistently_and_expose_middleware() {
    use ferroly::rest::Route;

    let server = Server::builder(ServerOptions {
        path_prefix: "/api/".into(),
        ..local()
    })
    .request_id()
    .cors(Cors::new().allow_origin("https://app.test"))
    .security_headers(SecurityHeaders::new())
    .layer(|ctx, next| async move { next.run(ctx).await.header("x-layer", "1") })
    .get("/thing", |ctx| async move {
        HttpResponse::text(StatusCode::OK, format!("GET {}", ctx.route().unwrap_or("")))
    })
    .patch("/thing", |ctx| async move {
        HttpResponse::text(StatusCode::OK, format!("PATCH {}", ctx.body().len()))
    })
    .head("/explicit", |_| async {
        HttpResponse::new(StatusCode::NO_CONTENT)
    })
    .options("/explicit", |_| async { ok("options") })
    .add_route(
        "/multi",
        |ctx| async move { HttpResponse::text(StatusCode::OK, ctx.method().as_str().to_string()) },
        vec![Method::Put, Method::Delete],
    )
    .route(
        "/limited",
        Route::post(|_| async { ok("limited") }).body_limit(2),
    )
    .group("/admin", |g| {
        g.filter(|ctx| {
            ctx.header("x-admin")
                .is_none()
                .then(|| HttpResponse::text(StatusCode::FORBIDDEN, "admins only"))
        })
        .get("/stats", |ctx| async move {
            HttpResponse::text(StatusCode::OK, ctx.route().unwrap_or("").to_string())
        })
    })
    .map_router(|router| router.get("/raw", |_| async { ok("unprefixed") }))
    .try_build()
    .expect("valid routes");
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();
    let client = Client::new();
    let url = |path: &str| format!("http://{addr}{path}");

    let resp = client.get(url("/api/thing")).send().await.unwrap();
    assert_eq!(resp.text(), "GET /api/thing");
    // Middleware installed through the builder is active.
    assert_eq!(resp.header("x-layer"), Some("1"));
    assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(resp.header("x-request-id").map(str::len), Some(32));
    assert_eq!(resp.header("vary"), Some("Origin"));

    let resp = client
        .patch(url("/api/thing"))
        .body_bytes(b"abc".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text(), "PATCH 3");
    assert_eq!(
        client
            .head(url("/api/explicit"))
            .send()
            .await
            .unwrap()
            .status_code(),
        204
    );
    assert_eq!(
        client
            .options(url("/api/explicit"))
            .send()
            .await
            .unwrap()
            .text(),
        "options"
    );
    assert_eq!(
        client
            .delete(url("/api/multi"))
            .send()
            .await
            .unwrap()
            .text(),
        "DELETE"
    );
    assert_eq!(
        client
            .post(url("/api/limited"))
            .body_bytes(b"abc".to_vec())
            .send()
            .await
            .unwrap()
            .status_code(),
        413
    );

    // The group sits under the server prefix, with its filter.
    assert_eq!(
        client
            .get(url("/api/admin/stats"))
            .send()
            .await
            .unwrap()
            .status_code(),
        403
    );
    let resp = client
        .get(url("/api/admin/stats"))
        .header("x-admin", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text(), "/api/admin/stats");
    assert_eq!(
        client
            .get(url("/admin/stats"))
            .send()
            .await
            .unwrap()
            .status_code(),
        404
    );
    assert_eq!(
        client
            .get(url("/thing"))
            .send()
            .await
            .unwrap()
            .status_code(),
        404
    );

    // `map_router` routes are not prefixed.
    assert_eq!(
        client.get(url("/raw")).send().await.unwrap().text(),
        "unprefixed"
    );

    // CORS preflight through the builder-installed policy.
    let preflight = client
        .options(url("/api/thing"))
        .header("origin", "https://app.test")
        .header("access-control-request-method", "PATCH")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status_code(), 204);
    assert_eq!(
        preflight.header("access-control-allow-origin"),
        Some("https://app.test")
    );
    server.stop().await.unwrap();

    // `try_build` reports routing mistakes; `build` does not fail.
    let conflicting = || {
        Server::builder(local())
            .get("/a/:x", |_| async { ok("1") })
            .get("/a/:y", |_| async { ok("2") })
    };
    assert!(conflicting().try_build().is_err());
    let lenient = conflicting().build();
    lenient.start().await.unwrap();
    let addr = lenient.local_addr().unwrap();
    assert_eq!(
        body(&client, format!("http://{addr}/a/z")).await,
        (200, "1".to_string())
    );
    lenient.stop().await.unwrap();
}
