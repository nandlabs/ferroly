#![cfg(feature = "clients")]
//! Unit coverage for the auth providers: header application.

use ferroly::clients::{ApiKeyAuth, AuthProvider, BasicAuth, BearerAuth};
use ferroly::http::{Method, Request};

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
}
