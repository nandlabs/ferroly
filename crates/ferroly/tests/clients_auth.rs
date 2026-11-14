#![cfg(feature = "clients")]
//! The auth providers: header application, secret redaction (no credential in
//! `Debug` output or error text) and the async refresh path.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferroly::clients::{
    authorize, AccessToken, ApiKeyAuth, AuthError, AuthProvider, BasicAuth, BearerAuth,
    RefreshingAuth, Secret, TokenSource,
};
use ferroly::http::{BoxFuture, Method, Request};

fn blank() -> Request {
    Request::builder(Method::Get, "http://example.test/")
        .unwrap()
        .build()
}

#[test]
fn bearer_sets_authorization() {
    let auth = BearerAuth::new("tok123");
    let mut req = blank();
    auth.apply(&mut req);
    assert_eq!(req.headers.get("authorization"), Some("Bearer tok123"));

    let from_secret = BearerAuth::from_secret(Secret::from("tok456"));
    let mut req = blank();
    from_secret.apply(&mut req);
    assert_eq!(req.headers.get("authorization"), Some("Bearer tok456"));
}

#[test]
fn api_key_sets_custom_header() {
    let auth = ApiKeyAuth::new("X-Api-Key", "secret");
    let mut req = blank();
    auth.apply(&mut req);
    assert_eq!(req.headers.get("x-api-key"), Some("secret"));
}

#[test]
fn basic_base64_encodes_credentials() {
    let auth = BasicAuth::new("aladdin", "opensesame");
    let mut req = blank();
    auth.apply(&mut req);
    // RFC 7617 canonical example
    assert_eq!(
        req.headers.get("authorization"),
        Some("Basic YWxhZGRpbjpvcGVuc2VzYW1l")
    );

    // padding paths: "a:" (2-byte tail -> single '=') and "ab:c" (1-byte tail -> "==")
    let two = BasicAuth::new("a", "");
    let mut r2 = blank();
    two.apply(&mut r2);
    let h2 = r2.headers.get("authorization").unwrap();
    assert!(h2.ends_with('=') && !h2.ends_with("=="));

    let one = BasicAuth::new("ab", "c");
    let mut r1 = blank();
    one.apply(&mut r1);
    assert!(r1.headers.get("authorization").unwrap().ends_with("=="));

    // RFC 7617 §2.1: non-ASCII credentials are sent as UTF-8.
    let utf8 = BasicAuth::new("test", "123\u{a3}");
    let mut req = blank();
    utf8.apply(&mut req);
    assert_eq!(
        req.headers.get("authorization"),
        Some("Basic dGVzdDoxMjPCow==")
    );
}

// ---- G10: credentials never appear in Debug output ----------------------------

#[test]
fn debug_output_redacts_every_credential() {
    let bearer = BearerAuth::new("sk-live-BEARER-TOKEN-0123456789");
    let api_key = ApiKeyAuth::new("x-api-key", "ak-live-API-KEY-0123456789");
    let basic = BasicAuth::new("alice", "hunter2-PASSWORD");

    let outputs = [
        format!("{bearer:?}"),
        format!("{bearer:#?}"),
        format!("{api_key:?}"),
        format!("{api_key:#?}"),
        format!("{basic:?}"),
        format!("{basic:#?}"),
        // Nested in containers, as it would be in a config struct's Debug.
        format!("{:?}", Some(&bearer)),
        format!("{:?}", vec![basic.clone()]),
        format!("{:?}", (api_key.clone(), "ctx")),
    ];
    for text in &outputs {
        for secret in [
            "BEARER-TOKEN",
            "API-KEY",
            "hunter2",
            "PASSWORD",
            "sk-live",
            "ak-live",
        ] {
            assert!(!text.contains(secret), "{secret} leaked in {text}");
        }
        assert!(text.contains("[REDACTED]"), "{text}");
        // Not even an encoded form of the Basic credentials.
        assert!(!text.contains("YWxpY2U6aHVudGVyMi1QQVNTV09SRA"), "{text}");
    }
    // The non-secret parts stay visible, for diagnostics.
    assert!(format!("{api_key:?}").contains("x-api-key"));
    assert!(format!("{basic:?}").contains("alice"));
    assert!(format!("{bearer:?}").starts_with("BearerAuth"));
}

#[test]
fn cloned_providers_still_authenticate() {
    let original = BasicAuth::new("alice", "s3cret");
    let copy = original.clone();
    drop(original); // wipes the original's buffer, not the clone's
    let mut req = blank();
    copy.apply(&mut req);
    assert_eq!(
        req.headers.get("authorization"),
        Some("Basic YWxpY2U6czNjcmV0")
    );
    let key = ApiKeyAuth::new("k", "v1").clone();
    let mut req = blank();
    key.apply(&mut req);
    assert_eq!(req.headers.get("k"), Some("v1"));
}

#[test]
fn header_injection_through_credentials_is_neutralised() {
    let auth = BearerAuth::new("tok\r\nX-Injected: 1");
    let mut req = blank();
    auth.apply(&mut req);
    assert_eq!(
        req.headers.get("authorization"),
        Some("Bearer tokX-Injected: 1")
    );
    assert!(req.headers.get("x-injected").is_none());
}

// ---- the async path -----------------------------------------------------------

#[tokio::test]
async fn authorize_works_for_static_providers() {
    let providers: Vec<Arc<dyn AuthProvider>> = vec![
        Arc::new(BearerAuth::new("t")),
        Arc::new(ApiKeyAuth::new("x-key", "k")),
        Arc::new(BasicAuth::new("u", "p")),
    ];
    for provider in &providers {
        // The default prepare() is a no-op that succeeds.
        assert_eq!(provider.prepare().await, Ok(()));
        let mut req = blank();
        authorize(provider.as_ref(), &mut req).await.unwrap();
        assert!(req.headers.contains("authorization") || req.headers.contains("x-key"));
    }
}

/// A token source that hands out `token-1`, `token-2`, … and can be told to
/// fail, to be slow, or to issue tokens with a given lifetime.
struct FakeSource {
    fetches: AtomicU32,
    failing: AtomicBool,
    lifetime: Option<Duration>,
    latency: Duration,
}

impl FakeSource {
    fn new(lifetime: Option<Duration>) -> Arc<Self> {
        Arc::new(FakeSource {
            fetches: AtomicU32::new(0),
            failing: AtomicBool::new(false),
            lifetime,
            latency: Duration::from_millis(50),
        })
    }

    fn fetches(&self) -> u32 {
        self.fetches.load(Ordering::SeqCst)
    }
}

impl TokenSource for FakeSource {
    fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>> {
        Box::pin(async move {
            let n = self.fetches.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::time::sleep(self.latency).await;
            if self.failing.load(Ordering::SeqCst) {
                return Err(AuthError::Transport("token endpoint unreachable".into()));
            }
            let token = AccessToken::new(format!("token-{n}"));
            Ok(match self.lifetime {
                Some(lifetime) => token.expires_in(lifetime),
                None => token,
            })
        })
    }
}

async fn header_after_authorize(auth: &RefreshingAuth) -> Result<String, AuthError> {
    let mut req = blank();
    authorize(auth, &mut req).await?;
    Ok(req
        .headers
        .get("authorization")
        .unwrap_or_default()
        .to_string())
}

#[tokio::test(start_paused = true)]
async fn refreshing_auth_caches_until_shortly_before_expiry() {
    let source = FakeSource::new(Some(Duration::from_secs(600)));
    let auth = RefreshingAuth::from_arc(source.clone()).skew(Duration::from_secs(60));

    // Before prepare(): nothing to apply.
    let mut req = blank();
    auth.apply(&mut req);
    assert!(req.headers.get("authorization").is_none());

    assert_eq!(
        header_after_authorize(&auth).await.unwrap(),
        "Bearer token-1"
    );
    assert_eq!(auth.token().await.unwrap().expose_str(), Some("token-1"));
    for _ in 0..10 {
        tokio::time::advance(Duration::from_secs(50)).await;
        assert_eq!(
            header_after_authorize(&auth).await.unwrap(),
            "Bearer token-1"
        );
    }
    // ~500 s in: still more than the 60 s skew away from expiry.
    assert_eq!(source.fetches(), 1);

    // Inside the skew window the token is refreshed ahead of time.
    tokio::time::advance(Duration::from_secs(45)).await;
    assert_eq!(
        header_after_authorize(&auth).await.unwrap(),
        "Bearer token-2"
    );
    assert_eq!(source.fetches(), 2);
}

#[tokio::test(start_paused = true)]
async fn refreshing_auth_shares_one_fetch_between_concurrent_callers() {
    let source = FakeSource::new(Some(Duration::from_secs(600)));
    let auth = Arc::new(RefreshingAuth::from_arc(source.clone()));
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let auth = auth.clone();
        tasks.push(tokio::spawn(
            async move { header_after_authorize(&auth).await },
        ));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), "Bearer token-1");
    }
    assert_eq!(source.fetches(), 1);

    // The same holds for the refresh after expiry.
    tokio::time::advance(Duration::from_secs(601)).await;
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let auth = auth.clone();
        tasks.push(tokio::spawn(
            async move { header_after_authorize(&auth).await },
        ));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), "Bearer token-2");
    }
    assert_eq!(source.fetches(), 2);
}

#[tokio::test(start_paused = true)]
async fn refreshing_auth_keeps_an_unexpired_token_when_refresh_fails() {
    let source = FakeSource::new(Some(Duration::from_secs(600)));
    let auth = RefreshingAuth::from_arc(source.clone()).skew(Duration::from_secs(60));
    assert_eq!(
        header_after_authorize(&auth).await.unwrap(),
        "Bearer token-1"
    );

    // Refresh is due (inside the skew window) but the endpoint is down: the
    // token has not actually expired, so requests keep working.
    source.failing.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(560)).await;
    assert_eq!(
        header_after_authorize(&auth).await.unwrap(),
        "Bearer token-1"
    );
    assert_eq!(source.fetches(), 2); // it did try

    // Once the token is really expired, the failure surfaces.
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        header_after_authorize(&auth).await,
        Err(AuthError::Transport("token endpoint unreachable".into()))
    );

    // And it recovers as soon as the endpoint does.
    source.failing.store(false, Ordering::SeqCst);
    assert!(header_after_authorize(&auth)
        .await
        .unwrap()
        .starts_with("Bearer token-"));
}

#[tokio::test(start_paused = true)]
async fn refreshing_auth_first_fetch_failure_is_an_error() {
    let source = FakeSource::new(None);
    source.failing.store(true, Ordering::SeqCst);
    let auth = RefreshingAuth::from_arc(source.clone());
    assert!(matches!(auth.prepare().await, Err(AuthError::Transport(_))));
    let mut req = blank();
    assert!(authorize(&auth, &mut req).await.is_err());
    // Nothing was applied to the request.
    assert!(req.headers.get("authorization").is_none());
}

#[tokio::test(start_paused = true)]
async fn tokens_without_expiry_are_kept_until_invalidated() {
    let source = FakeSource::new(None);
    let auth = RefreshingAuth::from_arc(source.clone());
    assert_eq!(
        header_after_authorize(&auth).await.unwrap(),
        "Bearer token-1"
    );
    tokio::time::advance(Duration::from_secs(86_400 * 365)).await;
    assert_eq!(
        header_after_authorize(&auth).await.unwrap(),
        "Bearer token-1"
    );
    assert_eq!(source.fetches(), 1);

    // The server rejected the token (401): drop it and fetch a new one.
    auth.invalidate();
    let mut req = blank();
    auth.apply(&mut req);
    assert!(req.headers.get("authorization").is_none());
    assert_eq!(
        header_after_authorize(&auth).await.unwrap(),
        "Bearer token-2"
    );
}

#[tokio::test(start_paused = true)]
async fn refreshing_auth_never_prints_its_token() {
    struct Leaky;
    impl TokenSource for Leaky {
        fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>> {
            Box::pin(async {
                Ok(AccessToken::new("at-SUPER-SECRET-ACCESS-TOKEN")
                    .expires_in(Duration::from_secs(60)))
            })
        }
    }
    let auth = RefreshingAuth::new(Leaky);
    auth.prepare().await.unwrap();
    let debug = format!("{auth:?}");
    assert!(debug.contains("has_token: true"), "{debug}");
    assert!(!debug.contains("SUPER-SECRET"), "{debug}");

    let token = AccessToken::new("at-SUPER-SECRET-ACCESS-TOKEN").expires_in(Duration::from_secs(5));
    assert_eq!(token.lifetime(), Some(Duration::from_secs(5)));
    assert!(!format!("{token:?}").contains("SUPER-SECRET"));
    assert_eq!(
        token.token().expose_str(),
        Some("at-SUPER-SECRET-ACCESS-TOKEN")
    );

    // A token that is not text cannot become a header value.
    struct Binary;
    impl TokenSource for Binary {
        fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>> {
            Box::pin(async { Ok(AccessToken::new(Secret::new(vec![0xff, 0xfe]))) })
        }
    }
    assert!(matches!(
        RefreshingAuth::new(Binary).prepare().await,
        Err(AuthError::InvalidResponse(_))
    ));
}
