#![cfg(feature = "turbo")]
//! Serves a real turbo router on an ephemeral port and drives it with the
//! in-house HTTP client.

use ferroly::http::{serve, Client, HttpResponse, Method, Request, StatusCode};
use ferroly::turbo::Router;
use tokio::net::TcpListener;

async fn spawn(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = router.into_handler();
    tokio::spawn(async move {
        let _ = serve(listener, handler, std::future::pending::<()>()).await;
    });
    format!("http://{addr}")
}

fn app() -> Router {
    Router::new()
        .get("/greet/:name", |ctx| async move {
            let name = ctx.param("name").unwrap_or("world").to_string();
            HttpResponse::text(StatusCode::OK, format!("hi {name}"))
        })
        .get("/items/:id", |ctx| async move {
            let id: i64 = ctx.param_as("id").unwrap();
            HttpResponse::text(StatusCode::OK, format!("id*2={}", id * 2))
        })
        .add(
            "/thing",
            |_ctx| async move { HttpResponse::text(StatusCode::CREATED, "made") },
            vec![Method::Post, Method::Put],
        )
        .auth(|ctx| {
            if ctx.path().starts_with("/secure") && ctx.header("authorization").is_none() {
                Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "no auth"))
            } else {
                None
            }
        })
        .get("/secure", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "secret")
        })
}

async fn get(base: &str, path: &str) -> (u16, String) {
    let client = Client::new();
    let req = Request::builder(Method::Get, &format!("{base}{path}"))
        .unwrap()
        .build();
    let resp = client.send(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

#[tokio::test]
async fn routes_params_and_methods() {
    let base = spawn(app()).await;

    assert_eq!(get(&base, "/greet/ada").await, (200, "hi ada".to_string()));
    assert_eq!(get(&base, "/items/21").await, (200, "id*2=42".to_string()));

    // POST /thing -> 201; GET /thing -> 405 (path exists, wrong method)
    let client = Client::new();
    let resp = client
        .send(
            Request::builder(Method::Post, &format!("{base}/thing"))
                .unwrap()
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    assert_eq!(get(&base, "/thing").await.0, 405);

    // unknown path -> 404
    assert_eq!(get(&base, "/nope").await.0, 404);
}

#[tokio::test]
async fn authenticator_runs_first() {
    let base = spawn(app()).await;
    assert_eq!(get(&base, "/secure").await, (401, "no auth".to_string()));

    let client = Client::new();
    let resp = client
        .send(
            Request::builder(Method::Get, &format!("{base}/secure"))
                .unwrap()
                .header("Authorization", "Bearer x")
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "secret");
}

#[tokio::test]
async fn middleware_wraps_and_short_circuits() {
    let router = Router::new()
        .layer(|ctx, next| async move {
            // short-circuit before the handler for one path
            if ctx.path() == "/blocked" {
                return HttpResponse::text(StatusCode::FORBIDDEN, "blocked by mw");
            }
            // otherwise run the inner chain and post-process the response
            next.run(ctx).await.header("X-Mw", "1")
        })
        .get("/ok", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "ok")
        })
        .get("/blocked", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "unreachable")
        });
    let base = spawn(router).await;

    let client = Client::new();

    // handler runs; middleware adds a response header (post-processing).
    let resp = client
        .send(
            Request::builder(Method::Get, &format!("{base}/ok"))
                .unwrap()
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mw_header = resp.headers().get("x-mw").map(str::to_string);
    let body = resp.text().await.unwrap();
    assert_eq!(mw_header.as_deref(), Some("1"));
    assert_eq!(body, "ok");

    // middleware short-circuits; the handler never runs.
    let resp = client
        .send(
            Request::builder(Method::Get, &format!("{base}/blocked"))
                .unwrap()
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(resp.text().await.unwrap(), "blocked by mw");
}

#[derive(ferroly::codec::Encode, ferroly::codec::Decode)]
struct Body {
    msg: String,
}

fn app2() -> Router {
    Router::new()
        .put("/put", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "put")
        })
        .delete("/del", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "del")
        })
        .patch("/patch", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "patch")
        })
        .post("/echo", |ctx| async move {
            let b: Body = ctx.read().unwrap();
            HttpResponse::text(StatusCode::OK, b.msg)
        })
        .get("/q", |ctx| async move {
            HttpResponse::text(StatusCode::OK, ctx.query_param("x").unwrap_or_default())
        })
        .get("/num/:id", |ctx| async move {
            match ctx.param_as::<i64>("id") {
                Ok(n) => HttpResponse::text(StatusCode::OK, (n * 2).to_string()),
                Err(_) => HttpResponse::text(StatusCode::BAD_REQUEST, "bad id"),
            }
        })
        .filter(|ctx| {
            if ctx.path() == "/deny" {
                Some(HttpResponse::text(StatusCode::FORBIDDEN, "denied"))
            } else {
                None
            }
        })
        .get("/deny", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "unreachable")
        })
        .on_not_found(|_ctx| async move { HttpResponse::text(StatusCode::NOT_FOUND, "custom 404") })
        .on_method_not_allowed(|_ctx| async move {
            HttpResponse::text(StatusCode::METHOD_NOT_ALLOWED, "custom 405")
        })
}

async fn send(base: &str, method: Method, path: &str) -> (u16, String) {
    let client = Client::new();
    let req = Request::builder(method, &format!("{base}{path}"))
        .unwrap()
        .build();
    let resp = client.send(req).await.unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

#[tokio::test]
async fn methods_body_query_and_custom_fallbacks() {
    let base = spawn(app2()).await;

    assert_eq!(send(&base, Method::Put, "/put").await, (200, "put".into()));
    assert_eq!(
        send(&base, Method::Delete, "/del").await,
        (200, "del".into())
    );
    assert_eq!(
        send(&base, Method::Patch, "/patch").await,
        (200, "patch".into())
    );

    // codec body-read
    let client = Client::new();
    let resp = client
        .send(
            Request::builder(Method::Post, &format!("{base}/echo"))
                .unwrap()
                .header("content-type", "application/json")
                .body(br#"{"msg":"hey"}"#.to_vec())
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "hey");

    // query param + typed path param (ok and error)
    assert_eq!(
        send(&base, Method::Get, "/q?x=a%20b").await,
        (200, "a b".into())
    );
    assert_eq!(
        send(&base, Method::Get, "/num/21").await,
        (200, "42".into())
    );
    assert_eq!(send(&base, Method::Get, "/num/abc").await.0, 400);

    // non-auth filter short-circuit
    assert_eq!(
        send(&base, Method::Get, "/deny").await,
        (403, "denied".into())
    );

    // custom 404 / 405 handlers
    assert_eq!(
        send(&base, Method::Get, "/missing").await,
        (404, "custom 404".into())
    );
    assert_eq!(
        send(&base, Method::Get, "/put").await,
        (405, "custom 405".into())
    );
}

#[tokio::test]
async fn head_options_allow_and_typed_query() {
    let router = Router::new()
        .get("/ping", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "pong")
        })
        .head(
            "/ping",
            |_ctx| async move { HttpResponse::new(StatusCode::OK) },
        )
        .get("/q", |ctx| async move {
            let n = ctx.query_int("n").unwrap_or(-1);
            let b = ctx.query_bool("flag").unwrap_or(false);
            HttpResponse::text(StatusCode::OK, format!("{n}:{b}"))
        });
    let base = spawn(router).await;

    assert_eq!(
        send(&base, Method::Get, "/q?n=42&flag=yes").await,
        (200, "42:true".into())
    );

    // 405 with Allow listing the registered methods.
    let client = Client::new();
    let resp = client
        .send(
            Request::builder(Method::Post, &format!("{base}/ping"))
                .unwrap()
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 405);
    let allow = resp.headers().get("allow").unwrap_or("").to_string();
    assert!(
        allow.contains("GET") && allow.contains("HEAD"),
        "allow was {allow:?}"
    );
}

#[tokio::test]
async fn route_groups_and_scoped_filter() {
    let router = Router::new().group("/api", |g| {
        g.get("/health", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "ok")
        })
        .filter(|ctx| {
            if ctx.header("x-key").is_none() {
                Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "no key"))
            } else {
                None
            }
        })
    });
    let base = spawn(router).await;
    let client = Client::new();

    // group filter rejects without the header
    assert_eq!(send(&base, Method::Get, "/api/health").await.0, 401);
    // and allows with it
    let resp = client
        .send(
            Request::builder(Method::Get, &format!("{base}/api/health"))
                .unwrap()
                .header("x-key", "k")
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[cfg(feature = "log")]
#[tokio::test]
async fn access_log_records_requests() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    ferroly::log::set_max_level(ferroly::log::Level::Info);
    let buf = Buf(Arc::new(Mutex::new(Vec::new())));
    let logger = ferroly::log::Logger::json().to_writer(buf.clone());
    let router = Router::new()
        .access_log(logger)
        .get("/x", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "ok")
        });
    let base = spawn(router).await;

    let _ = send(&base, Method::Get, "/x").await;
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains(r#""path":"/x""#), "log was {out:?}");
    assert!(out.contains(r#""status":200"#), "log was {out:?}"); // typed number, not "200"
    assert!(out.contains(r#""ts":"#), "log was {out:?}"); // timestamped
}

#[cfg(feature = "log")]
#[tokio::test]
async fn trace_context_propagates_trace_id() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    ferroly::log::set_max_level(ferroly::log::Level::Info);
    let buf = Buf(Arc::new(Mutex::new(Vec::new())));
    ferroly::log::set_global(ferroly::log::Logger::json().to_writer(buf.clone()));

    let router = Router::new().trace_context().get("/t", |_ctx| async move {
        // Logged deep in the handler with no logger threaded in — the trace_id
        // set by `trace_context` must still attach.
        ferroly::log::info("in-handler", &[]);
        HttpResponse::text(StatusCode::OK, "ok")
    });
    let base = spawn(router).await;

    let _ = Client::new()
        .send(
            Request::builder(Method::Get, &format!("{base}/t"))
                .unwrap()
                .header("x-request-id", "req-42")
                .build(),
        )
        .await
        .unwrap();

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains(r#""trace_id":"req-42""#), "log was {out:?}");
    assert!(out.contains(r#""msg":"in-handler""#), "log was {out:?}");
}

#[cfg(feature = "auth")]
#[tokio::test]
async fn jwt_auth_guards_routes() {
    use ferroly::auth::encode_hs256;
    use ferroly::codec::Value;

    let router = Router::new()
        .jwt_auth("secret")
        .get("/secure", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "ok")
        });
    let base = spawn(router).await;

    // no token -> 401
    assert_eq!(send(&base, Method::Get, "/secure").await.0, 401);

    // valid HS256 token -> 200
    let token = encode_hs256(&Value::Object(vec![("sub".into(), "u1".into())]), b"secret");
    let resp = Client::new()
        .send(
            Request::builder(Method::Get, &format!("{base}/secure"))
                .unwrap()
                .header("authorization", format!("Bearer {token}"))
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn sse_event_stream() {
    let router = Router::new().get("/events", |_ctx| async move {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(4);
        tokio::spawn(async move {
            for i in 0..3 {
                let _ = tx.send(format!("event-{i}")).await;
            }
        });
        HttpResponse::event_stream(rx)
    });
    let base = spawn(router).await;
    let resp = Client::new()
        .send(
            Request::builder(Method::Get, &format!("{base}/events"))
                .unwrap()
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .contains("event-stream"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("data: event-0\n\n"), "body was {body:?}");
    assert!(body.contains("data: event-2\n\n"), "body was {body:?}");
}

#[tokio::test]
async fn content_negotiated_respond() {
    #[derive(ferroly::codec::Encode)]
    struct Item {
        name: String,
    }
    let router = Router::new().get("/item", |ctx| async move {
        ctx.respond(
            StatusCode::OK,
            &Item {
                name: "widget".into(),
            },
        )
    });
    let base = spawn(router).await;

    async fn get_accept(base: &str, accept: &str) -> ferroly::http::Response {
        Client::new()
            .send(
                Request::builder(Method::Get, &format!("{base}/item"))
                    .unwrap()
                    .header("accept", accept)
                    .build(),
            )
            .await
            .unwrap()
    }

    let resp = get_accept(&base, "application/json").await;
    assert!(resp.headers().get("content-type").unwrap().contains("json"));
    assert_eq!(resp.text().await.unwrap(), r#"{"name":"widget"}"#);

    let resp = get_accept(&base, "application/xml").await;
    assert!(resp.headers().get("content-type").unwrap().contains("xml"));

    let resp = get_accept(&base, "application/toml").await;
    assert!(resp.headers().get("content-type").unwrap().contains("toml"));
    assert_eq!(resp.text().await.unwrap(), "name = \"widget\"\n");

    // `yml` is accepted as an alias and served as YAML.
    let resp = get_accept(&base, "application/yml").await;
    assert!(resp.headers().get("content-type").unwrap().contains("yaml"));

    let resp = get_accept(&base, "text/html").await;
    assert_eq!(resp.status().as_u16(), 406);
}

#[tokio::test]
async fn strict_slash_and_rate_limit() {
    // strict slash: /users and /users/ are distinct.
    let router = Router::new()
        .strict_slash(true)
        .get("/users", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "list")
        });
    let base = spawn(router).await;
    assert_eq!(send(&base, Method::Get, "/users").await.0, 200);
    assert_eq!(send(&base, Method::Get, "/users/").await.0, 404);

    // rate limit: burst 2, no refill -> third request is 429.
    let router = Router::new()
        .rate_limit(0.0, 2.0, |_ctx| "shared".to_string())
        .get("/", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "ok")
        });
    let base = spawn(router).await;
    assert_eq!(send(&base, Method::Get, "/").await.0, 200);
    assert_eq!(send(&base, Method::Get, "/").await.0, 200);
    assert_eq!(send(&base, Method::Get, "/").await.0, 429);
}

#[tokio::test]
async fn metrics_middleware_and_endpoint() {
    let router = Router::new()
        .metrics()
        .metrics_route("/metrics")
        .get("/hello", |_ctx| async move {
            HttpResponse::text(StatusCode::OK, "hi")
        });
    let base = spawn(router).await;

    // Drive a couple of requests so the RED metrics have samples.
    assert_eq!(send(&base, Method::Get, "/hello").await.0, 200);
    assert_eq!(send(&base, Method::Get, "/hello").await.0, 200);

    let (status, body) = send(&base, Method::Get, "/metrics").await;
    assert_eq!(status, 200);
    // Prometheus exposition surface for the RED metrics.
    assert!(
        body.contains("# TYPE http_requests_total counter"),
        "body={body}"
    );
    assert!(
        body.contains(r#"http_requests_total{method="GET",status="200"}"#),
        "body={body}"
    );
    assert!(
        body.contains("# TYPE http_request_duration_seconds histogram"),
        "body={body}"
    );
    assert!(
        body.contains("http_request_duration_seconds_bucket"),
        "body={body}"
    );
    assert!(body.contains("http_requests_in_flight"), "body={body}");
}
