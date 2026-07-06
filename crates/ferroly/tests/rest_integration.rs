#![cfg(feature = "rest")]
//! End-to-end: start a real server on an ephemeral port, drive it with the
//! client, then stop it through the lifecycle component.

use std::sync::Arc;

use ferroly::codec::{json, Decode, Encode};
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::lifecycle::{Component, ComponentState};
use ferroly::rest::{Client, Server, ServerOptions};

#[derive(Encode, Decode, Debug, PartialEq)]
struct Echo {
    message: String,
}

fn json_response(value: &impl Encode) -> HttpResponse {
    HttpResponse::new(StatusCode::OK)
        .header("content-type", "application/json")
        .body(json::encode(value).into_bytes())
}

fn build_server() -> Server {
    Server::builder(ServerOptions {
        listen_host: "127.0.0.1".into(),
        listen_port: 0,
        ..Default::default()
    })
    .post("/echo", |ctx| async move {
        let body: Echo = ctx.read().unwrap();
        json_response(&Echo {
            message: format!("echo: {}", body.message),
        })
    })
    .get("/greet/:name", |ctx| async move {
        let name = ctx.param("name").unwrap_or("world").to_string();
        json_response(&Echo {
            message: format!("hi {name}"),
        })
    })
    .build()
}

#[tokio::test]
async fn client_server_json_round_trip() {
    let server = Arc::new(build_server());
    Component::start(server.as_ref()).await.unwrap();
    assert_eq!(server.state(), ComponentState::Running);
    let addr = server.local_addr().expect("bound address");

    let client = Client::new();

    // POST with a codec-encoded body, decode the JSON response.
    let resp = client
        .post(format!("http://{addr}/echo"))
        .body(&Echo {
            message: "hello".into(),
        })
        .send()
        .await
        .unwrap();
    assert!(resp.is_success());
    let decoded: Echo = resp.decode().unwrap();
    assert_eq!(
        decoded,
        Echo {
            message: "echo: hello".into()
        }
    );

    // GET with a ${param} path substitution.
    let resp = client
        .get("http://${host}/greet/ada")
        .path_param("host", addr.to_string())
        .send()
        .await
        .unwrap();
    let decoded: Echo = resp.decode().unwrap();
    assert_eq!(decoded.message, "hi ada");

    Component::stop(server.as_ref()).await.unwrap();
    assert_eq!(server.state(), ComponentState::Stopped);
}

#[tokio::test]
async fn unsubstituted_path_param_errors() {
    let client = Client::new();
    let err = client
        .get("http://host/items/${id}")
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err, ferroly::rest::ClientError::InvalidRequest(_)));
}

#[tokio::test]
async fn auth_prefix_and_error_status() {
    use ferroly::clients::BearerAuth;
    use ferroly::rest::ClientOptions;
    use std::time::Duration;

    let server = Arc::new(
        Server::builder(ServerOptions {
            id: "svc".into(),
            path_prefix: "/api".into(),
            listen_host: "127.0.0.1".into(),
            listen_port: 0,
        })
        .get("/whoami", |ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                ctx.header("authorization").unwrap_or("none").to_string(),
            )
        })
        .get("/bad", |_ctx| async move {
            HttpResponse::text(StatusCode::BAD_REQUEST, "nope")
        })
        .build(),
    );
    Component::start(server.as_ref()).await.unwrap();
    let addr = server.local_addr().unwrap();

    let client = Client::with_options(
        ClientOptions::builder()
            .auth(Arc::new(BearerAuth::new("tok")))
            .default_content_type("application/json")
            .request_timeout(Duration::from_secs(5))
            .build(),
    );
    let resp = client
        .get(format!("http://{addr}/api/whoami"))
        .send()
        .await
        .unwrap();
    assert!(resp.is_success());
    assert_eq!(resp.status_code(), 200);
    assert!(resp.content_type().is_some());
    assert!(!resp.raw().is_empty());
    assert_eq!(resp.text(), "Bearer tok");

    let resp = Client::new()
        .get(format!("http://{addr}/api/bad"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status_code(), 400);
    assert!(!resp.is_success());

    Component::stop(server.as_ref()).await.unwrap();
}

#[tokio::test]
async fn request_builder_query_header_and_raw_body() {
    let server = Arc::new(
        Server::builder(ServerOptions {
            listen_host: "127.0.0.1".into(),
            listen_port: 0,
            ..Default::default()
        })
        .get("/probe", |ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                format!(
                    "{}|{}",
                    ctx.query_param("q").unwrap_or_default(),
                    ctx.header("x-custom").unwrap_or("none")
                ),
            )
        })
        .post("/raw", |ctx| async move {
            HttpResponse::text(
                StatusCode::OK,
                String::from_utf8_lossy(ctx.body()).into_owned(),
            )
        })
        .build(),
    );
    Component::start(server.as_ref()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let client = Client::new();

    let resp = client
        .get(format!("http://{addr}/probe"))
        .query("q", "hi there")
        .header("x-custom", "yo")
        .content_type("text/plain")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text(), "hi there|yo");

    let resp = client
        .post(format!("http://{addr}/raw"))
        .body_bytes(b"raw-bytes".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text(), "raw-bytes");

    Component::stop(server.as_ref()).await.unwrap();
}

#[tokio::test]
async fn health_and_ready_endpoints() {
    use ferroly::lifecycle::{HealthRegistry, HealthStatus};

    let registry = HealthRegistry::new();
    registry.register("db", || HealthStatus::Up);

    let server = Arc::new(
        Server::builder(ServerOptions {
            listen_host: "127.0.0.1".into(),
            listen_port: 0,
            ..Default::default()
        })
        .health_endpoints(registry)
        .build(),
    );
    Component::start(server.as_ref()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let client = Client::new();

    let resp = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert!(resp.is_success());
    assert!(resp.text().contains(r#""overall":"up""#));

    let resp = client
        .get(format!("http://{addr}/ready"))
        .send()
        .await
        .unwrap();
    assert!(resp.is_success());
    assert_eq!(resp.text(), "ready");

    Component::stop(server.as_ref()).await.unwrap();
}

#[tokio::test]
async fn retry_exhausts_on_transport_error() {
    use ferroly::clients::RetryPolicy;
    use ferroly::rest::ClientOptions;
    use std::time::Duration;

    let client = Client::with_options(
        ClientOptions::builder()
            .retry_policy(RetryPolicy::fixed(2, Duration::from_millis(1)))
            .build(),
    );
    let err = client.get("http://127.0.0.1:1/x").send().await.unwrap_err();
    assert!(matches!(err, ferroly::rest::ClientError::Transport(_)));
}
