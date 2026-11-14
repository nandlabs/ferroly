#![cfg(all(feature = "clients", feature = "codec"))]
//! OAuth 2.0 client-credentials against a local token endpoint served by
//! `ferroly::http`: the request that is sent, token caching, single-flight
//! refresh, expiry, error mapping — and that no credential leaks into errors.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::clients::{
    authorize, AuthError, AuthProvider, ClientAuthStyle, OAuth2ClientCredentials, TokenSource,
};
use ferroly::http::{
    serve, BoxFuture, Client, HttpHandler, HttpResponse, Method, Request, StatusCode,
};
use tokio::net::TcpListener;

const CLIENT_SECRET: &str = "s3cr3t";

/// What the token endpoint received.
#[derive(Debug, Clone, Default)]
struct Seen {
    method: String,
    path: String,
    content_type: Option<String>,
    accept: Option<String>,
    authorization: Option<String>,
    body: String,
}

/// A scripted token endpoint. `{n}` in the body is replaced by the request
/// number, so each response carries a distinct token.
struct TokenEndpoint {
    status: AtomicUsize,
    body: Mutex<String>,
    delay: Mutex<Duration>,
    hits: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
}

impl HttpHandler for TokenEndpoint {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            let n = self.hits.fetch_add(1, Ordering::SeqCst) + 1;
            self.seen.lock().unwrap().push(Seen {
                method: req.method.as_str().to_string(),
                path: req.uri.path.clone(),
                content_type: req.headers.get("content-type").map(str::to_string),
                accept: req.headers.get("accept").map(str::to_string),
                authorization: req.headers.get("authorization").map(str::to_string),
                body: String::from_utf8_lossy(&req.body).into_owned(),
            });
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let body = self.body.lock().unwrap().replace("{n}", &n.to_string());
            HttpResponse::new(StatusCode(self.status.load(Ordering::SeqCst) as u16))
                .header("content-type", "application/json")
                .body(body)
        })
    }
}

impl TokenEndpoint {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request")
    }

    fn respond(&self, status: usize, body: &str) {
        self.status.store(status, Ordering::SeqCst);
        *self.body.lock().unwrap() = body.to_string();
    }
}

const TOKEN_BODY: &str = r#"{"access_token":"tok-{n}","token_type":"Bearer","expires_in":3600}"#;

async fn spawn_endpoint() -> (Arc<TokenEndpoint>, String) {
    let endpoint = Arc::new(TokenEndpoint {
        status: AtomicUsize::new(200),
        body: Mutex::new(TOKEN_BODY.to_string()),
        delay: Mutex::new(Duration::ZERO),
        hits: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/oauth2/token", listener.local_addr().unwrap());
    let handler: Arc<dyn HttpHandler> = endpoint.clone();
    tokio::spawn(async move {
        let _ = serve(listener, handler, std::future::pending::<()>()).await;
    });
    (endpoint, url)
}

fn source(url: &str) -> OAuth2ClientCredentials {
    OAuth2ClientCredentials::new(url, "my-client", CLIENT_SECRET)
}

async fn bearer(auth: &dyn AuthProvider) -> Result<String, AuthError> {
    let mut req = Request::builder(Method::Get, "http://api.test/v1")
        .unwrap()
        .build();
    authorize(auth, &mut req).await?;
    Ok(req
        .headers
        .get("authorization")
        .unwrap_or_default()
        .to_string())
}

// ---- the request ---------------------------------------------------------------

#[tokio::test]
async fn sends_a_form_post_with_basic_client_authentication() {
    let (endpoint, url) = spawn_endpoint().await;
    let auth = source(&url)
        .scope("orders:read")
        .scope("orders:write")
        .audience("https://api.example/")
        .param("resource", "a&b=c")
        .into_provider();
    assert_eq!(bearer(&auth).await.unwrap(), "Bearer tok-1");

    let seen = endpoint.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/oauth2/token");
    assert_eq!(
        seen.content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(seen.accept.as_deref(), Some("application/json"));
    // base64("my-client:s3cr3t")
    assert_eq!(
        seen.authorization.as_deref(),
        Some("Basic bXktY2xpZW50OnMzY3IzdA==")
    );
    assert_eq!(
        seen.body,
        "grant_type=client_credentials&scope=orders%3Aread+orders%3Awrite\
         &audience=https%3A%2F%2Fapi.example%2F&resource=a%26b%3Dc"
    );
    // With the Basic header, the secret is not repeated in the body.
    assert!(!seen.body.contains("client_secret"));
}

#[tokio::test]
async fn basic_credentials_are_form_encoded_before_base64() {
    let (endpoint, url) = spawn_endpoint().await;
    let auth = OAuth2ClientCredentials::new(&url, "client id", "p@ss:w+rd/&=").into_provider();
    assert_eq!(bearer(&auth).await.unwrap(), "Bearer tok-1");
    // base64("client+id:p%40ss%3Aw%2Brd%2F%26%3D") — RFC 6749 §2.3.1.
    assert_eq!(
        endpoint.last().authorization.as_deref(),
        Some("Basic Y2xpZW50K2lkOnAlNDBzcyUzQXclMkJyZCUyRiUyNiUzRA==")
    );
}

#[tokio::test]
async fn request_body_style_puts_the_credentials_in_the_form() {
    let (endpoint, url) = spawn_endpoint().await;
    let auth = OAuth2ClientCredentials::new(&url, "client id", "p@ss:w+rd/&=")
        .auth_style(ClientAuthStyle::RequestBody)
        .into_provider();
    assert_eq!(bearer(&auth).await.unwrap(), "Bearer tok-1");
    let seen = endpoint.last();
    assert_eq!(seen.authorization, None);
    assert_eq!(
        seen.body,
        "grant_type=client_credentials&client_id=client+id\
         &client_secret=p%40ss%3Aw%2Brd%2F%26%3D"
    );
    assert_eq!(ClientAuthStyle::default(), ClientAuthStyle::BasicHeader);
}

// ---- caching and refresh ---------------------------------------------------------

#[tokio::test]
async fn the_token_is_cached_between_requests() {
    let (endpoint, url) = spawn_endpoint().await;
    let auth = source(&url).into_provider();
    for _ in 0..10 {
        assert_eq!(bearer(&auth).await.unwrap(), "Bearer tok-1");
    }
    assert_eq!(endpoint.hits(), 1);
    // The raw source does not cache: each call is a token request.
    let raw = source(&url);
    let first = raw.fetch_token().await.unwrap();
    let second = raw.fetch_token().await.unwrap();
    assert_eq!(first.token().expose_str(), Some("tok-2"));
    assert_eq!(second.token().expose_str(), Some("tok-3"));
    assert_eq!(first.lifetime(), Some(Duration::from_secs(3600)));
}

#[tokio::test]
async fn concurrent_callers_share_a_single_token_request() {
    let (endpoint, url) = spawn_endpoint().await;
    *endpoint.delay.lock().unwrap() = Duration::from_millis(50);
    let auth = Arc::new(source(&url).into_provider());
    let mut tasks = Vec::new();
    for _ in 0..25 {
        let auth = auth.clone();
        tasks.push(tokio::spawn(async move { bearer(auth.as_ref()).await }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), "Bearer tok-1");
    }
    assert_eq!(endpoint.hits(), 1);
}

#[tokio::test]
async fn an_expired_token_is_refreshed() {
    let (endpoint, url) = spawn_endpoint().await;
    // A 300 ms token; refresh as late as possible.
    endpoint.respond(
        200,
        r#"{"access_token":"tok-{n}","token_type":"bearer","expires_in":0.3}"#,
    );
    let auth = source(&url).into_provider().skew(Duration::ZERO);
    assert_eq!(bearer(&auth).await.unwrap(), "Bearer tok-1");
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(bearer(&auth).await.unwrap(), "Bearer tok-2");
    assert_eq!(endpoint.hits(), 2);

    // invalidate() forces a new token before expiry (e.g. after a 401).
    endpoint.respond(200, TOKEN_BODY);
    auth.invalidate();
    let hits = endpoint.hits();
    assert!(bearer(&auth).await.unwrap().starts_with("Bearer tok-"));
    assert_eq!(endpoint.hits(), hits + 1);
}

#[tokio::test]
async fn tolerated_response_variants() {
    let (endpoint, url) = spawn_endpoint().await;
    let raw = source(&url);
    // expires_in as a numeric string, token_type in any case.
    endpoint.respond(
        200,
        r#"{"access_token":"a","token_type":"BEARER","expires_in":"120","scope":"x"}"#,
    );
    let token = raw.fetch_token().await.unwrap();
    assert_eq!(token.lifetime(), Some(Duration::from_secs(120)));
    // No expires_in, no token_type.
    endpoint.respond(200, r#"{"access_token":"b"}"#);
    let token = raw.fetch_token().await.unwrap();
    assert_eq!(token.token().expose_str(), Some("b"));
    assert_eq!(token.lifetime(), None);
    endpoint.respond(200, r#"{"access_token":"c","expires_in":null}"#);
    assert_eq!(raw.fetch_token().await.unwrap().lifetime(), None);
}

// ---- errors ----------------------------------------------------------------------

#[tokio::test]
async fn rejected_requests_report_status_and_oauth_error_code() {
    let (endpoint, url) = spawn_endpoint().await;
    let auth = source(&url).into_provider();

    endpoint.respond(
        401,
        r#"{"error":"invalid_client","error_description":"bad secret s3cr3t for my-client"}"#,
    );
    let error = bearer(&auth).await.unwrap_err();
    assert_eq!(
        error,
        AuthError::Rejected {
            status: 401,
            code: Some("invalid_client".to_string())
        }
    );
    // The server echoed the secret in its description; we do not pass it on.
    let text = format!("{error} / {error:?}");
    assert!(!text.contains(CLIENT_SECRET), "{text}");
    assert_eq!(
        error.to_string(),
        "token endpoint rejected the request (status 401)"
    );

    for (status, body) in [
        (500usize, "<html>Internal Server Error</html>"),
        (503, ""),
        (400, r#"{"error":{"nested":"object"}}"#),
        (400, r#"{"error":"line one\nINFO forged log line"}"#),
        (429, r#"["not","an","object"]"#),
    ] {
        endpoint.respond(status, body);
        assert_eq!(
            bearer(&auth).await,
            Err(AuthError::Rejected {
                status: status as u16,
                code: None
            }),
            "{status} {body}"
        );
    }
}

#[tokio::test]
async fn unusable_success_responses_are_invalid() {
    let (endpoint, url) = spawn_endpoint().await;
    let raw = source(&url);
    for body in [
        "not json",
        "",
        "[]",
        "\"tok\"",
        "{}",
        r#"{"token":"x"}"#,
        r#"{"access_token":""}"#,
        r#"{"access_token":42}"#,
        r#"{"access_token":null}"#,
        r#"{"access_token":"x","token_type":"mac"}"#,
        r#"{"access_token":"x","token_type":7}"#,
        r#"{"access_token":"x","expires_in":-5}"#,
        r#"{"access_token":"x","expires_in":"soon"}"#,
        r#"{"access_token":"x","expires_in":[60]}"#,
    ] {
        endpoint.respond(200, body);
        let error = raw.fetch_token().await.unwrap_err();
        assert!(
            matches!(error, AuthError::InvalidResponse(_)),
            "{body} -> {error:?}"
        );
    }
    // A response that carried a token never ends up in the error text.
    endpoint.respond(200, r#"{"access_token":"tok-LEAKED","token_type":"mac"}"#);
    let error = raw.fetch_token().await.unwrap_err();
    assert!(!format!("{error} {error:?}").contains("LEAKED"));
}

#[tokio::test]
async fn transport_failures_do_not_leak_configuration() {
    // Nothing is listening on port 1.
    let dead = OAuth2ClientCredentials::new(
        "http://127.0.0.1:1/token?hint=URL-SECRET",
        "my-client",
        "CLIENT-SECRET-VALUE",
    )
    .with_client(Client::new().with_timeout(Some(Duration::from_secs(5))));
    let error = dead.fetch_token().await.unwrap_err();
    assert!(matches!(error, AuthError::Transport(_)), "{error:?}");
    let text = format!("{error} / {error:?}");
    assert!(!text.contains("CLIENT-SECRET-VALUE"), "{text}");
    assert!(!text.contains("URL-SECRET"), "{text}");

    // A malformed endpoint URL is a configuration error, reported without
    // echoing the URL.
    let bad = OAuth2ClientCredentials::new("not a url with PASSWORD", "id", "secret");
    let error = bad.fetch_token().await.unwrap_err();
    assert!(matches!(error, AuthError::Config(_)), "{error:?}");
    assert!(!format!("{error} {error:?}").contains("PASSWORD"));

    // A token endpoint that never answers is cut off by the client timeout.
    let (endpoint, url) = spawn_endpoint().await;
    *endpoint.delay.lock().unwrap() = Duration::from_secs(30);
    let slow =
        source(&url).with_client(Client::new().with_timeout(Some(Duration::from_millis(100))));
    assert_eq!(
        slow.fetch_token().await.unwrap_err(),
        AuthError::Transport("timed out".to_string())
    );
}

#[tokio::test]
async fn a_failed_first_fetch_can_be_retried() {
    let (endpoint, url) = spawn_endpoint().await;
    let auth = source(&url).into_provider();
    endpoint.respond(503, "");
    assert!(bearer(&auth).await.is_err());
    endpoint.respond(200, TOKEN_BODY);
    assert_eq!(bearer(&auth).await.unwrap(), "Bearer tok-2");
    let debug = format!("{auth:?} {:?}", source(&url));
    assert!(!debug.contains(CLIENT_SECRET), "{debug}");
}
