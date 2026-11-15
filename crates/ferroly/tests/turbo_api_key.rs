#![cfg(all(feature = "turbo", feature = "hash"))]
//! API-key authentication in the router: hashed keys, revocation, scopes,
//! the header and bearer forms, optional authentication and store failures.

use std::net::SocketAddr;
use std::sync::Arc;

use ferroly::http::{BoxFuture, HttpResponse, ServerConfig, StatusCode};
use ferroly::turbo::{
    require_api_key_scopes, ApiKey, ApiKeyAuth, ApiKeyStore, ApiKeyStoreError, Ctx,
    MemoryApiKeyStore, Route, Router,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CI_KEY: &str = "k-ci-3q2+7v0Yd1mJ9aX4bW8cZ5eR6tU1iO0p";
const OPS_KEY: &str = "k-ops-Vb9nM2xL7kJ4hG1fD8sA5qW3eR6tY0u";

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

async fn get(addr: SocketAddr, target: &str, headers: &[(&str, &str)]) -> Raw {
    let mut request = format!("GET {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Raw {
        status: head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        head: head.to_string(),
        body: body.to_string(),
    }
}

async fn whoami(ctx: Ctx) -> HttpResponse {
    let id = ctx.api_key().map(|k| k.id().to_string());
    HttpResponse::text(StatusCode::OK, id.unwrap_or_else(|| "anonymous".into()))
}

fn store() -> Arc<MemoryApiKeyStore> {
    Arc::new(
        MemoryApiKeyStore::new()
            .with_key(ApiKey::from_key("ci", CI_KEY).with_scopes(["deploy", "read"]))
            .with_key(ApiKey::from_key("ops", OPS_KEY).with_scopes(["read"])),
    )
}

#[tokio::test]
async fn keys_are_checked_against_the_hashed_store() {
    let store = store();
    let router = Router::new().group("/api", |g| {
        g.api_key(ApiKeyAuth::new(store.clone()))
            .get("/me", whoami)
            .route(
                "/deploy",
                Route::get(whoami).filter(require_api_key_scopes(&["deploy"])),
            )
    });
    let addr = spawn(router).await;

    let ok = get(addr, "/api/me", &[("X-API-Key", CI_KEY)]).await;
    assert_eq!((ok.status, ok.body.as_str()), (200, "ci"));
    // Header names are case-insensitive; surrounding blanks are trimmed.
    let ok = get(addr, "/api/me", &[("x-api-key", &format!(" {OPS_KEY} "))]).await;
    assert_eq!((ok.status, ok.body.as_str()), (200, "ops"));

    // Missing, wrong, truncated or case-changed keys: 401 with a challenge
    // that never says which.
    let missing = get(addr, "/api/me", &[]).await;
    assert_eq!(missing.status, 401);
    assert!(missing
        .head
        .to_ascii_lowercase()
        .contains("www-authenticate: apikey header=\"x-api-key\""));
    for bad in [
        "nope",
        &CI_KEY[..CI_KEY.len() - 1],
        &CI_KEY.to_ascii_uppercase(),
        &format!("{CI_KEY}x"),
    ] {
        let resp = get(addr, "/api/me", &[("X-API-Key", bad)]).await;
        assert_eq!(resp.status, 401, "{bad}");
        assert_eq!(resp.body, "401 invalid API key");
    }
    // A bearer token is not read unless allowed.
    let bearer = get(
        addr,
        "/api/me",
        &[("Authorization", &format!("Bearer {CI_KEY}"))],
    )
    .await;
    assert_eq!(bearer.status, 401);

    // Scope guard on one route.
    assert_eq!(
        get(addr, "/api/deploy", &[("X-API-Key", CI_KEY)])
            .await
            .status,
        200
    );
    assert_eq!(
        get(addr, "/api/deploy", &[("X-API-Key", OPS_KEY)])
            .await
            .status,
        403
    );

    // Revocation takes effect at once.
    assert!(store.remove("ci"));
    assert_eq!(
        get(addr, "/api/me", &[("X-API-Key", CI_KEY)]).await.status,
        401
    );
    assert_eq!(
        get(addr, "/api/me", &[("X-API-Key", OPS_KEY)]).await.status,
        200
    );
}

#[tokio::test]
async fn required_scopes_custom_header_bearer_and_optional() {
    let router = Router::new()
        .group("/deploy", |g| {
            g.api_key(
                ApiKeyAuth::new(store())
                    .header("X-Service-Token")
                    .allow_bearer(true)
                    .require_scopes(&["deploy"]),
            )
            .get("/", whoami)
        })
        .group("/public", |g| {
            g.api_key(ApiKeyAuth::new(store()).optional(true))
                .get("/", whoami)
        });
    let addr = spawn(router).await;

    let custom = get(addr, "/deploy/", &[("X-Service-Token", CI_KEY)]).await;
    assert_eq!((custom.status, custom.body.as_str()), (200, "ci"));
    let bearer = get(
        addr,
        "/deploy/",
        &[("Authorization", &format!("Bearer {CI_KEY}"))],
    )
    .await;
    assert_eq!(bearer.status, 200);
    // The default header is not read once another one is configured.
    assert_eq!(
        get(addr, "/deploy/", &[("X-API-Key", CI_KEY)]).await.status,
        401
    );
    // Valid key, missing scope: 403.
    assert_eq!(
        get(addr, "/deploy/", &[("X-Service-Token", OPS_KEY)])
            .await
            .status,
        403
    );

    // Optional: anonymous passes, a presented key is still verified.
    let anon = get(addr, "/public/", &[]).await;
    assert_eq!((anon.status, anon.body.as_str()), (200, "anonymous"));
    assert_eq!(
        get(addr, "/public/", &[("X-API-Key", "bad")]).await.status,
        401
    );
    let known = get(addr, "/public/", &[("X-API-Key", OPS_KEY)]).await;
    assert_eq!(known.body, "ops");
}

#[tokio::test]
async fn a_failing_store_answers_503_and_the_router_form_works() {
    struct Down;
    impl ApiKeyStore for Down {
        fn find<'a>(
            &'a self,
            _digest: &'a [u8; 32],
        ) -> BoxFuture<'a, Result<Option<ApiKey>, ApiKeyStoreError>> {
            Box::pin(async { Err(ApiKeyStoreError("database offline".into())) })
        }
    }
    let down = spawn(
        Router::new()
            .api_key(ApiKeyAuth::new(Down))
            .get("/", whoami),
    )
    .await;
    let resp = get(down, "/", &[("X-API-Key", CI_KEY)]).await;
    assert_eq!(resp.status, 503);
    assert!(!resp.body.contains("database"), "{}", resp.body);

    let up = spawn(
        Router::new()
            .api_key(ApiKeyAuth::new(store()))
            .get("/", whoami),
    )
    .await;
    assert_eq!(get(up, "/", &[("X-API-Key", OPS_KEY)]).await.body, "ops");
    assert_eq!(get(up, "/", &[]).await.status, 401);
    // The guard without an authenticator: 401.
    let unguarded = spawn(Router::new().route(
        "/",
        Route::get(whoami).filter(require_api_key_scopes(&["read"])),
    ))
    .await;
    assert_eq!(get(unguarded, "/", &[]).await.status, 401);
}

/// SEC-11: a repeated key header is ambiguous (a gateway in front may have
/// checked the other instance), so it is refused, even when optional.
#[tokio::test]
async fn a_repeated_key_header_is_refused() {
    let router = Router::new()
        .group("/strict", |g| {
            g.api_key(ApiKeyAuth::new(store()).allow_bearer(true))
                .get("/", whoami)
        })
        .group("/optional", |g| {
            g.api_key(ApiKeyAuth::new(store()).optional(true))
                .get("/", whoami)
        });
    let addr = spawn(router).await;

    // One instance works.
    assert_eq!(
        get(addr, "/strict/", &[("X-API-Key", CI_KEY)]).await.body,
        "ci"
    );
    for path in ["/strict/", "/optional/"] {
        for pair in [[CI_KEY, OPS_KEY], [CI_KEY, CI_KEY], ["bad", CI_KEY]] {
            let headers = [("X-API-Key", pair[0]), ("x-api-key", pair[1])];
            let resp = get(addr, path, &headers).await;
            assert_eq!(resp.status, 401, "{path} {pair:?}");
            assert!(resp.head.contains("ApiKey header="), "{}", resp.head);
        }
    }
    // A repeated Authorization header, when it may carry the key.
    let bearer = format!("Bearer {CI_KEY}");
    assert_eq!(
        get(addr, "/strict/", &[("Authorization", &bearer)])
            .await
            .status,
        200
    );
    let twice = [
        ("Authorization", bearer.as_str()),
        ("Authorization", "Bearer x"),
    ];
    assert_eq!(get(addr, "/strict/", &twice).await.status, 401);
}
