#![cfg(all(feature = "turbo", feature = "auth"))]
//! JWT authentication in the router: the legacy and the validating HS256
//! authenticators, other HMAC algorithms, JWKS (fixed and remote) key sets,
//! claims handed to handlers, and scope / role guards.

use std::net::SocketAddr;
use std::sync::Arc;

use ferroly::auth::{encode_hmac, encode_hs256, Algorithm, Jwk, Jwks, RemoteJwks, Validation};
use ferroly::codec::Value;
use ferroly::http::{HttpResponse, ServerConfig, StatusCode};
use ferroly::turbo::{
    require_any_role, require_any_scope, require_roles, require_scopes, JwtAuth, Route, RouteError,
    Router,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// An RSA-2048 public key and an RS256 token signed with the OpenSSL
// command-line tool (kid "rsa-1", iss https://issuer.example, aud
// orders-api and billing-api, sub user-1, scope "orders:read orders:write",
// exp in the year 2100). The same fixture as in `auth_jwks.rs`.
const OPENSSL_RSA_N: &str = "uOZtUwSf139eYpboPCSTbEcmsJhZkvFJJ3Zr2Ipn-1b71vaGZFfsld0ytcC0UHGG\
     dRsvSGw0_NFGAN3gR8w7CGA1MAMYvtpuZs12jRUzhlmD2-ud4ZcDkji9e6FkZ2e5\
     8FsjH5Bi6E_nQiPoMdg1LpjbPqcAaMpm92DDhMs1Qz53uoXsk7-rJCCWqW-T82lw\
     f4c3F59Di8uso3i8uPEdxcFhMiXmKWO-nd2d5gnv4qT9HkfwnJiCRyPP7frSdWMu\
     VT9EuOxU0EWhG3e3SHxLWQ9ivLFN_mO-H5VLEyachrcNIRi5rTwCFnsYSi-xMc5L\
     6sTAVzxMvQQb0ylk4wSEnQ";
const OPENSSL_RSA_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6InJzYS0xIn0.eyJpc3Mi\
     OiJodHRwczovL2lzc3Vlci5leGFtcGxlIiwiYXVkIjpbIm9yZGVycy1hcGkiLCJi\
     aWxsaW5nLWFwaSJdLCJzdWIiOiJ1c2VyLTEiLCJzY29wZSI6Im9yZGVyczpyZWFk\
     IG9yZGVyczp3cml0ZSIsImlhdCI6MTcwMDAwMDAwMCwiZXhwIjo0MTAyNDQ0ODAw\
     fQ.UoNJweNTPUYgO_s21uutCaPFrnkHC8ykGIgZ04bOR2-PAtDBZuDVJ9HoC0w1e\
     0HhvDlSsItePNe--p3BkKzLSr5YilqAkm-tJFOkcmduKLEQsrtPkWHQ_iskg3k2C\
     B8wZ57DbmmSGQLXxFUFyKhJTlXH3odT2FIjSzjZ5zj64FYtyYTAjmeaayHxB3tL8\
     yL_Q_nd_pt-IAPZbEPXyXm2X0r0oYzxA579kg5jKL81Vg5WjcJ9nmlRd-wgF2kTj\
     IxoRdRupJNweJOvaZSTHu6zge7ySGD85AyJIt1IAKl5onQlSSo_Nc0Ipe7jEQmPj\
     wkMgj37H450eRJK_tTyy01L3A";

const SECRET: &[u8] = b"a-shared-secret-of-32-bytes-long";
const FAR_FUTURE: i64 = 4_102_444_800; // 2100-01-01

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

async fn request(addr: SocketAddr, target: &str, authorization: Option<&str>) -> Raw {
    let mut request = format!("GET {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    if let Some(value) = authorization {
        request.push_str(&format!("Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Raw {
        status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head: head.to_string(),
        body: body.to_string(),
    }
}

async fn with_token(addr: SocketAddr, target: &str, token: &str) -> Raw {
    request(addr, target, Some(&format!("Bearer {token}"))).await
}

fn claims(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
    )
}

/// A handler that reports the verified subject.
async fn whoami(ctx: ferroly::turbo::Ctx) -> HttpResponse {
    let subject = ctx
        .claims()
        .and_then(|c| c.subject().map(str::to_string))
        .unwrap_or_else(|| "anonymous".to_string());
    HttpResponse::text(StatusCode::OK, subject)
}

#[tokio::test]
async fn legacy_jwt_auth_behaviour_is_unchanged() {
    // A short secret and a token without `exp`: accepted, as before.
    let router = Router::new().jwt_auth("secret").get("/me", whoami);
    assert!(router.validate().is_ok());
    let addr = spawn(router).await;

    let missing = request(addr, "/me", None).await;
    assert_eq!(
        (missing.status, missing.body.as_str()),
        (401, "401 missing bearer token")
    );
    assert_eq!(
        missing.header("www-authenticate").as_deref(),
        Some("Bearer")
    );

    let token = encode_hs256(&claims(&[("sub", "u1".into())]), b"secret");
    let accepted = with_token(addr, "/me", &token).await;
    // New: the verified claims reach the handler.
    assert_eq!((accepted.status, accepted.body.as_str()), (200, "u1"));

    // An `exp` that is present is still enforced.
    let expired = encode_hs256(
        &claims(&[("sub", "u1".into()), ("exp", Value::Int(1))]),
        b"secret",
    );
    assert_eq!(with_token(addr, "/me", &expired).await.status, 401);
    let forged = encode_hs256(&claims(&[("sub", "u1".into())]), b"other");
    let rejected = with_token(addr, "/me", &forged).await;
    assert_eq!(
        (rejected.status, rejected.body.as_str()),
        (401, "401 invalid token")
    );
    assert_eq!(
        rejected.header("www-authenticate").as_deref(),
        Some("Bearer error=\"invalid_token\"")
    );
}

#[tokio::test]
async fn jwt_auth_with_requires_exp_and_validates_claims() {
    let validation = Validation::new()
        .issuer("https://issuer.test")
        .audience("orders-api");
    let router = Router::new()
        .jwt_auth_with(SECRET, validation)
        .get("/me", whoami)
        .get("/claims", |ctx| async move {
            let claims = ctx.claims().expect("verified claims");
            HttpResponse::text(
                StatusCode::OK,
                format!(
                    "{}|{}",
                    claims.get("tenant").and_then(Value::as_str).unwrap_or(""),
                    claims.scopes().iter().collect::<Vec<_>>().join("+")
                ),
            )
        });
    assert!(router.validate().is_ok());
    let addr = spawn(router).await;

    let mint = |extra: &[(&str, Value)]| {
        let mut all = vec![
            ("sub", Value::from("user-7")),
            ("iss", Value::from("https://issuer.test")),
            ("aud", Value::from("orders-api")),
            ("exp", Value::Int(FAR_FUTURE)),
        ];
        for (k, v) in extra {
            all.retain(|(name, _)| name != k);
            if !v.is_null() {
                all.push((k, v.clone()));
            }
        }
        encode_hmac(Algorithm::Hs256, &claims(&all), SECRET).unwrap()
    };

    let good = mint(&[("tenant", "acme".into()), ("scope", "a b".into())]);
    assert_eq!(with_token(addr, "/me", &good).await.body, "user-7");
    assert_eq!(with_token(addr, "/claims", &good).await.body, "acme|a+b");
    // The scheme is case-insensitive.
    assert_eq!(
        request(addr, "/me", Some(&format!("bearer {good}")))
            .await
            .status,
        200
    );

    for (why, token) in [
        ("no exp", mint(&[("exp", Value::Null)])),
        ("expired", mint(&[("exp", Value::Int(1_000))])),
        ("wrong audience", mint(&[("aud", "billing-api".into())])),
        ("wrong issuer", mint(&[("iss", "https://evil.test".into())])),
        (
            "wrong secret",
            encode_hmac(
                Algorithm::Hs256,
                &claims(&[("sub", "x".into()), ("exp", Value::Int(FAR_FUTURE))]),
                b"another-secret-of-32-bytes-long!",
            )
            .unwrap(),
        ),
        (
            "wrong algorithm",
            encode_hmac(
                Algorithm::Hs512,
                &claims(&[
                    ("sub", "x".into()),
                    ("iss", "https://issuer.test".into()),
                    ("aud", "orders-api".into()),
                    ("exp", Value::Int(FAR_FUTURE)),
                ]),
                SECRET,
            )
            .unwrap(),
        ),
        ("garbage", "not.a.token".to_string()),
    ] {
        let resp = with_token(addr, "/me", &token).await;
        assert_eq!(resp.status, 401, "{why}");
        // The response never says why.
        assert_eq!(resp.body, "401 invalid token", "{why}");
    }

    for header in ["Basic dXNlcjpwYXNz", "Bearer", "Bearer ", "Token abc"] {
        assert_eq!(
            request(addr, "/me", Some(header)).await.status,
            401,
            "{header:?}"
        );
    }
}

#[tokio::test]
async fn weak_jwt_configuration_fails_closed_and_is_reported() {
    let router = Router::new()
        .jwt_auth_with("short", Validation::new())
        .get("/me", whoami);
    assert!(matches!(router.validate(), Err(RouteError::Config(_))));
    let addr = spawn(router).await;
    // Even a token signed with that very secret is refused.
    let token = encode_hs256(
        &claims(&[("sub", "u".into()), ("exp", Value::Int(FAR_FUTURE))]),
        b"short",
    );
    assert_eq!(with_token(addr, "/me", &token).await.status, 401);
}

#[tokio::test]
async fn hmac_variants_pin_the_algorithm() {
    let secret = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let body = claims(&[("sub", "u512".into()), ("exp", Value::Int(FAR_FUTURE))]);
    for alg in [Algorithm::Hs384, Algorithm::Hs512] {
        let router = Router::new()
            .jwt_auth_hmac_with(alg, secret.to_vec(), Validation::new())
            .get("/me", whoami);
        assert!(router.validate().is_ok());
        let addr = spawn(router).await;
        let token = encode_hmac(alg, &body, secret).unwrap();
        assert_eq!(with_token(addr, "/me", &token).await.body, "u512");
        // The same claims under another algorithm are not accepted.
        let other = encode_hmac(Algorithm::Hs256, &body, secret).unwrap();
        assert_eq!(with_token(addr, "/me", &other).await.status, 401);
    }
}

fn rsa_jwks() -> Jwks {
    let key = Jwk::from_json(&format!(
        r#"{{"kty":"RSA","kid":"rsa-1","use":"sig","alg":"RS256","n":"{OPENSSL_RSA_N}","e":"AQAB"}}"#
    ))
    .unwrap();
    Jwks::from_keys(vec![key])
}

#[tokio::test]
async fn jwks_authenticator_and_scope_guards() {
    let validation = Validation::new()
        .issuer("https://issuer.example")
        .audience("orders-api");
    let router = Router::new()
        .jwt(JwtAuth::jwks(rsa_jwks(), validation))
        .get("/me", whoami)
        .route(
            "/read",
            Route::get(whoami).filter(require_scopes(&["orders:read", "orders:write"])),
        )
        .route(
            "/any",
            Route::get(whoami).filter(require_any_scope(&["admin", "orders:read"])),
        )
        .route(
            "/admin",
            Route::get(whoami).filter(require_scopes(&["admin"])),
        );
    assert!(router.validate().is_ok());
    let addr = spawn(router).await;

    // The fixture token carries `scope: "orders:read orders:write"`.
    assert_eq!(
        with_token(addr, "/me", OPENSSL_RSA_TOKEN).await.body,
        "user-1"
    );
    assert_eq!(
        with_token(addr, "/read", OPENSSL_RSA_TOKEN).await.status,
        200
    );
    assert_eq!(
        with_token(addr, "/any", OPENSSL_RSA_TOKEN).await.status,
        200
    );
    let denied = with_token(addr, "/admin", OPENSSL_RSA_TOKEN).await;
    assert_eq!(denied.status, 403);
    assert_eq!(
        denied.header("www-authenticate").as_deref(),
        Some("Bearer error=\"insufficient_scope\", scope=\"admin\"")
    );

    // A tampered signature, and an HS256 token "signed" with the public
    // modulus (algorithm confusion), are both refused.
    let mut tampered = OPENSSL_RSA_TOKEN.to_string();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert_eq!(with_token(addr, "/me", &tampered).await.status, 401);
    let confused = encode_hs256(
        &claims(&[
            ("sub", "attacker".into()),
            ("iss", "https://issuer.example".into()),
            ("aud", "orders-api".into()),
            ("exp", Value::Int(FAR_FUTURE)),
        ]),
        OPENSSL_RSA_N.as_bytes(),
    );
    assert_eq!(with_token(addr, "/me", &confused).await.status, 401);
    assert_eq!(request(addr, "/me", None).await.status, 401);

    // An empty key set can accept nothing; that is reported.
    let empty = Router::new().jwt(JwtAuth::jwks(Jwks::from_keys(vec![]), Validation::new()));
    assert!(matches!(empty.validate(), Err(RouteError::Config(_))));
}

#[tokio::test]
async fn remote_jwks_authenticator_is_asynchronous() {
    // A JWKS endpoint served by another router.
    let document =
        format!(r#"{{"keys":[{{"kty":"RSA","kid":"rsa-1","n":"{OPENSSL_RSA_N}","e":"AQAB"}}]}}"#);
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    let issuer = Router::new().get("/.well-known/jwks.json", move |_| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let document = document.clone();
        async move {
            HttpResponse::new(StatusCode::OK)
                .header("content-type", "application/json")
                .body(document.into_bytes())
        }
    });
    let issuer_addr = spawn(issuer).await;

    let keys = Arc::new(RemoteJwks::new(format!(
        "http://{issuer_addr}/.well-known/jwks.json"
    )));
    let router = Router::new()
        .jwt_auth_jwks(keys, Validation::new().audience("orders-api"))
        .get("/me", whoami);
    let addr = spawn(router).await;

    assert_eq!(
        with_token(addr, "/me", OPENSSL_RSA_TOKEN).await.body,
        "user-1"
    );
    assert_eq!(
        with_token(addr, "/me", OPENSSL_RSA_TOKEN).await.body,
        "user-1"
    );
    assert_eq!(request(addr, "/me", None).await.status, 401);
    // The key set was fetched once and cached.
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn group_scoped_optional_auth_and_role_guards() {
    let auth = || JwtAuth::hs256(SECRET, Validation::new());
    let router = Router::new()
        .get("/public", whoami)
        .group("/app", |g| {
            g.jwt(auth().optional(true))
                .get("/home", whoami)
                .route(
                    "/staff",
                    Route::get(whoami).filter(require_any_role(&["admin", "staff"])),
                )
                .route(
                    "/root",
                    Route::get(whoami).filter(require_roles(&["admin", "auditor"])),
                )
        })
        .group("/strict", |g| g.jwt(auth()).get("/x", whoami));
    assert!(router.validate().is_ok());
    let addr = spawn(router).await;

    let token = |roles: &[&str]| {
        encode_hmac(
            Algorithm::Hs256,
            &claims(&[
                ("sub", "u".into()),
                ("exp", Value::Int(FAR_FUTURE)),
                (
                    "roles",
                    Value::Array(roles.iter().map(|r| Value::from(*r)).collect()),
                ),
            ]),
            SECRET,
        )
        .unwrap()
    };

    // Outside the groups nothing is required, and a token is not even read.
    assert_eq!(request(addr, "/public", None).await.body, "anonymous");
    assert_eq!(
        with_token(addr, "/public", "garbage").await.body,
        "anonymous"
    );

    // Optional: anonymous requests pass, but a presented token must be valid.
    assert_eq!(request(addr, "/app/home", None).await.body, "anonymous");
    assert_eq!(with_token(addr, "/app/home", &token(&[])).await.body, "u");
    assert_eq!(with_token(addr, "/app/home", "garbage").await.status, 401);

    // Role guards: 401 without identity, 403 without the role.
    assert_eq!(request(addr, "/app/staff", None).await.status, 401);
    let user = with_token(addr, "/app/staff", &token(&["user"])).await;
    assert_eq!(user.status, 403);
    assert_eq!(user.header("www-authenticate"), None);
    assert_eq!(
        with_token(addr, "/app/staff", &token(&["staff"]))
            .await
            .status,
        200
    );
    assert_eq!(
        with_token(addr, "/app/root", &token(&["admin"]))
            .await
            .status,
        403
    );
    assert_eq!(
        with_token(addr, "/app/root", &token(&["auditor", "admin"]))
            .await
            .status,
        200
    );

    // The strict group demands a token, however the path is spelled.
    assert_eq!(request(addr, "/strict/x", None).await.status, 401);
    assert_eq!(request(addr, "/app/../strict/x", None).await.status, 401);
    assert_eq!(request(addr, "//strict/x", None).await.status, 400);
    assert_eq!(with_token(addr, "/strict/x", &token(&[])).await.body, "u");
}
