#![cfg(feature = "store-s3")]
//! S3 adapter: the AWS SigV4 documentation examples, and a loopback mock S3
//! that re-derives every signature independently.

mod store_common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroly::hash::{base64, hex, hmac_sha256, sha256};
use ferroly::http::{percent_decode, HttpResponse, Method, Request, StatusCode};
use ferroly::store::s3::{sigv4, Credentials, ListObjects, PayloadHash, S3Client};
use ferroly::store::{Cache, Digest, StoreError};
use store_common::{content, serve_range, spawn, Scratch};

// ---- AWS documentation vectors ---------------------------------------
//
// The values below are the worked examples from the AWS S3 API reference,
// "Signature Calculations for the Authorization Header: Transferring
// Payload in a Single Chunk (AWS Signature Version 4)": bucket
// `examplebucket`, key id `AKIAIOSFODNN7EXAMPLE`, region `us-east-1`,
// timestamp 2013-05-24T00:00:00Z.

const EXAMPLE_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
const EXAMPLE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

fn example_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_369_353_600)
}

fn example_creds() -> Credentials {
    Credentials::new(EXAMPLE_KEY_ID, EXAMPLE_SECRET)
}

#[test]
fn aws_example_get_object() {
    let mut req = Request::builder(
        Method::Get,
        "https://examplebucket.s3.amazonaws.com/test.txt",
    )
    .unwrap()
    .header("Range", "bytes=0-9")
    .build();
    let sig = sigv4::sign(
        &mut req,
        &example_creds(),
        "us-east-1",
        "s3",
        example_time(),
        &PayloadHash::empty(),
    )
    .unwrap();
    assert_eq!(
        sig.canonical_request,
        "GET\n/test.txt\n\n\
         host:examplebucket.s3.amazonaws.com\n\
         range:bytes=0-9\n\
         x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
         x-amz-date:20130524T000000Z\n\n\
         host;range;x-amz-content-sha256;x-amz-date\n\
         e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sig.string_to_sign,
        "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n\
         7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
    );
    assert_eq!(
        sig.signature,
        "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
    assert_eq!(
        req.headers.get("authorization"),
        Some(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        )
    );
    assert_eq!(req.headers.get("x-amz-date"), Some("20130524T000000Z"));
}

#[test]
fn aws_example_put_object() {
    let body = b"Welcome to Amazon S3.";
    let payload = PayloadHash::of(body);
    assert_eq!(
        payload.as_str(),
        "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
    );
    let mut req = Request::builder(
        Method::Put,
        "https://examplebucket.s3.amazonaws.com/test%24file.text",
    )
    .unwrap()
    .header("Date", "Fri, 24 May 2013 00:00:00 GMT")
    .header("x-amz-storage-class", "REDUCED_REDUNDANCY")
    .build();
    let sig = sigv4::sign(
        &mut req,
        &example_creds(),
        "us-east-1",
        "s3",
        example_time(),
        &payload,
    )
    .unwrap();
    assert_eq!(
        sig.signed_headers,
        "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class"
    );
    assert_eq!(
        sig.signature,
        "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
    );
}

#[test]
fn aws_example_get_bucket_lifecycle() {
    let mut req = Request::builder(
        Method::Get,
        "https://examplebucket.s3.amazonaws.com/?lifecycle",
    )
    .unwrap()
    .build();
    let sig = sigv4::sign(
        &mut req,
        &example_creds(),
        "us-east-1",
        "s3",
        example_time(),
        &PayloadHash::empty(),
    )
    .unwrap();
    assert!(sig.canonical_request.starts_with("GET\n/\nlifecycle=\n"));
    assert_eq!(
        sig.signature,
        "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
    );
}

#[test]
fn aws_example_list_objects() {
    let mut req = Request::builder(
        Method::Get,
        "https://examplebucket.s3.amazonaws.com/?max-keys=2&prefix=J",
    )
    .unwrap()
    .build();
    let sig = sigv4::sign(
        &mut req,
        &example_creds(),
        "us-east-1",
        "s3",
        example_time(),
        &PayloadHash::empty(),
    )
    .unwrap();
    assert!(sig
        .canonical_request
        .starts_with("GET\n/\nmax-keys=2&prefix=J\n"));
    assert_eq!(
        sig.signature,
        "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
    );
}

#[test]
fn aws_example_signing_key() {
    // The signing-key derivation example from the IAM SigV4 guide.
    let key = sigv4::signing_key(
        b"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        "20120215",
        "us-east-1",
        "iam",
    );
    assert_eq!(
        hex::encode(&key),
        "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
    );
}

#[test]
fn session_token_and_unsigned_payload() {
    let creds = example_creds().with_session_token("FwoGZXIvYXdzEXAMPLE");
    let mut req = Request::builder(Method::Get, "https://examplebucket.s3.amazonaws.com/a")
        .unwrap()
        .header("User-Agent", "not-signed")
        .build();
    let sig = sigv4::sign(
        &mut req,
        &creds,
        "us-east-1",
        "s3",
        example_time(),
        &PayloadHash::Unsigned,
    )
    .unwrap();
    assert_eq!(
        sig.signed_headers,
        "host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
    );
    assert_eq!(
        req.headers.get("x-amz-content-sha256"),
        Some("UNSIGNED-PAYLOAD")
    );
    assert_eq!(
        req.headers.get("x-amz-security-token"),
        Some("FwoGZXIvYXdzEXAMPLE")
    );
    assert!(sig.canonical_request.ends_with("\nUNSIGNED-PAYLOAD"));
    assert!(sigv4::sign(
        &mut req,
        &creds,
        "us east",
        "s3",
        example_time(),
        &PayloadHash::Unsigned
    )
    .is_err());
}

#[test]
fn credentials_from_lookup() {
    let env = |name: &str| match name {
        "AWS_ACCESS_KEY_ID" => Some("AKIDEXAMPLE".to_string()),
        "AWS_SECRET_ACCESS_KEY" => Some("very-secret".to_string()),
        "AWS_SESSION_TOKEN" => Some("session-tok".to_string()),
        _ => None,
    };
    let creds = Credentials::from_lookup(env).unwrap();
    assert_eq!(creds.access_key_id(), "AKIDEXAMPLE");
    assert!(creds.has_session_token());
    let shown = format!("{creds:?}");
    assert!(
        !shown.contains("very-secret") && !shown.contains("session-tok"),
        "{shown}"
    );
    assert!(!shown.contains("AKIDEXAMPLE"), "{shown}");
    assert!(Credentials::from_lookup(|_| None).is_none());
    assert!(Credentials::from_lookup(|n| (n == "AWS_ACCESS_KEY_ID").then(|| "x".into())).is_none());
}

// ---- A mock S3 that verifies signatures on its own -------------------

const KEY_ID: &str = "AKIDTEST";
const SECRET: &str = "test-secret-key";
const REGION: &str = "eu-central-1";

/// RFC 3986 encoding, written independently of the library's.
fn uri_encode(s: &[u8]) -> String {
    let mut out = String::new();
    for &b in s {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Recomputes the SigV4 signature of a received request. `Err` carries the
/// reason it is not valid.
fn verify_signature(req: &Request, secret: &str) -> Result<(), String> {
    let auth = req.headers.get("authorization").ok_or("no authorization")?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("wrong algorithm")?;
    let mut fields = BTreeMap::new();
    for part in rest.split(", ") {
        let (k, v) = part.split_once('=').ok_or("bad field")?;
        fields.insert(k, v);
    }
    let credential = fields.get("Credential").ok_or("no credential")?;
    let parts: Vec<&str> = credential.split('/').collect();
    if parts.len() != 5 || parts[0] != KEY_ID || parts[4] != "aws4_request" {
        return Err(format!("bad credential {credential}"));
    }
    let (date, region, service) = (parts[1], parts[2], parts[3]);
    let signed: Vec<&str> = fields
        .get("SignedHeaders")
        .ok_or("no list")?
        .split(';')
        .collect();
    if !signed.contains(&"host") || !signed.contains(&"x-amz-date") {
        return Err("host and x-amz-date must be signed".into());
    }
    let amz_date = req.headers.get("x-amz-date").ok_or("no x-amz-date")?;
    if !amz_date.starts_with(date) {
        return Err("scope date differs from x-amz-date".into());
    }
    let mut query: Vec<(String, String)> = req
        .uri
        .query
        .as_deref()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (
                uri_encode(&percent_decode(k).unwrap()),
                uri_encode(&percent_decode(v).unwrap()),
            )
        })
        .collect();
    query.sort();
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut headers = String::new();
    for name in &signed {
        let value = req
            .headers
            .get(name)
            .ok_or(format!("signed header {name} missing"))?;
        headers.push_str(&format!("{name}:{}\n", value.trim()));
    }
    let payload = req
        .headers
        .get("x-amz-content-sha256")
        .ok_or("no payload hash")?;
    let canonical = format!(
        "{}\n{}\n{query}\n{headers}\n{}\n{payload}",
        req.method.as_str(),
        req.uri.path,
        signed.join(";")
    );
    let sts = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date}/{region}/{service}/aws4_request\n{}",
        sha256(canonical.as_bytes()).to_hex()
    );
    let k = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac_sha256(k.as_bytes(), region.as_bytes());
    let k = hmac_sha256(k.as_bytes(), service.as_bytes());
    let k = hmac_sha256(k.as_bytes(), b"aws4_request");
    let expected = hmac_sha256(k.as_bytes(), sts.as_bytes()).to_hex();
    if fields.get("Signature") != Some(&expected.as_str()) {
        return Err("SignatureDoesNotMatch".into());
    }
    Ok(())
}

fn xml_error(status: StatusCode, code: &str, message: &str) -> HttpResponse {
    HttpResponse::new(status)
        .header("content-type", "application/xml")
        .body(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>{code}</Code><Message>{message}</Message></Error>"
        ))
}

#[derive(Default)]
struct MockState {
    requests: Vec<(String, Option<String>)>,
    tokens_seen: Vec<Option<String>>,
}

/// Mock S3 (path style): bucket `bucket` holding `objects`.
async fn mock_s3(
    objects: BTreeMap<String, Vec<u8>>,
    require_token: Option<&'static str>,
) -> (String, Arc<Mutex<MockState>>) {
    let state = Arc::new(Mutex::new(MockState::default()));
    let st = state.clone();
    let base = spawn(move |req| {
        {
            let mut s = st.lock().unwrap();
            s.requests.push((
                req.uri.request_target(),
                req.headers.get("range").map(str::to_string),
            ));
            s.tokens_seen
                .push(req.headers.get("x-amz-security-token").map(str::to_string));
        }
        if let Err(why) = verify_signature(&req, SECRET) {
            return xml_error(StatusCode::FORBIDDEN, "SignatureDoesNotMatch", &why);
        }
        if let Some(token) = require_token {
            let signed = req.headers.get("authorization").unwrap_or("");
            if req.headers.get("x-amz-security-token") != Some(token)
                || !signed.contains("x-amz-security-token")
            {
                return xml_error(StatusCode::FORBIDDEN, "InvalidToken", "bad token");
            }
        }
        let path = req.uri.path.clone();
        let Some(rest) = path.strip_prefix("/bucket") else {
            return xml_error(StatusCode::NOT_FOUND, "NoSuchBucket", "no such bucket");
        };
        if rest.is_empty() || rest == "/" {
            return list(&req, &objects);
        }
        let key = String::from_utf8(percent_decode(&rest[1..]).unwrap()).unwrap();
        let Some(body) = objects.get(&key) else {
            return xml_error(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            );
        };
        let checksum = base64::encode(sha256(body).as_bytes());
        let resp = match req.method {
            Method::Head => {
                let mut r = HttpResponse::new(StatusCode::OK)
                    .header("etag", "\"abc123\"")
                    .header("x-amz-meta-owner", "tests")
                    .body(body.clone());
                if req.headers.get("x-amz-checksum-mode") == Some("ENABLED") && key != "nosum" {
                    r = r.header("x-amz-checksum-sha256", checksum);
                }
                r
            }
            _ => serve_range(&req, body).header("etag", "\"abc123\""),
        };
        resp
    })
    .await;
    (base, state)
}

/// `ListObjectsV2` with `max-keys` pagination; the continuation token is
/// the next index, disguised.
fn list(req: &Request, objects: &BTreeMap<String, Vec<u8>>) -> HttpResponse {
    let mut params = BTreeMap::new();
    for pair in req.uri.query.as_deref().unwrap_or("").split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(
            k.to_string(),
            String::from_utf8(percent_decode(v).unwrap()).unwrap(),
        );
    }
    if params.get("list-type").map(String::as_str) != Some("2") {
        return xml_error(StatusCode::BAD_REQUEST, "InvalidArgument", "list-type");
    }
    let prefix = params.get("prefix").cloned().unwrap_or_default();
    let max: usize = params
        .get("max-keys")
        .and_then(|m| m.parse().ok())
        .unwrap_or(1000);
    let start: usize = params
        .get("continuation-token")
        .map(|t| t.trim_start_matches("tok-").parse().unwrap())
        .unwrap_or(0);
    let keys: Vec<&String> = objects.keys().filter(|k| k.starts_with(&prefix)).collect();
    let page = &keys[start.min(keys.len())..(start + max).min(keys.len())];
    let truncated = start + max < keys.len();
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bucket</Name>",
    );
    xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
    if truncated {
        xml.push_str(&format!(
            "<NextContinuationToken>tok-{}</NextContinuationToken>",
            start + max
        ));
    }
    for key in page {
        xml.push_str(&format!(
            "<Contents><Key>{}</Key><Size>{}</Size><ETag>&quot;e&quot;</ETag>\
             <StorageClass>STANDARD</StorageClass></Contents>",
            key.replace('&', "&amp;"),
            objects[*key].len()
        ));
    }
    xml.push_str("</ListBucketResult>");
    HttpResponse::new(StatusCode::OK)
        .header("content-type", "application/xml")
        .body(xml)
}

fn client(base: &str, creds: Credentials) -> S3Client {
    S3Client::builder()
        .region(REGION)
        .endpoint(base)
        .credentials(creds)
        .build()
        .unwrap()
}

fn objects() -> BTreeMap<String, Vec<u8>> {
    let mut m = BTreeMap::new();
    m.insert("models/weights.bin".to_string(), content(70_000));
    m.insert("models/config.json".to_string(), b"{\"layers\":4}".to_vec());
    m.insert("dir/a & b+c.txt".to_string(), b"odd key".to_vec());
    m.insert("nosum".to_string(), b"no checksum stored".to_vec());
    for i in 0..5 {
        m.insert(format!("logs/{i:02}.log"), format!("log {i}").into_bytes());
    }
    m
}

#[tokio::test]
async fn signed_get_head_and_range() {
    let (base, state) = mock_s3(objects(), None).await;
    let s3 = client(&base, Credentials::new(KEY_ID, SECRET));

    let obj = s3.get_object("bucket", "models/config.json").await.unwrap();
    assert_eq!(obj.meta().etag.as_deref(), Some("abc123"));
    assert_eq!(obj.bytes().await.unwrap(), b"{\"layers\":4}");

    let odd = s3.get_object("bucket", "dir/a & b+c.txt").await.unwrap();
    assert_eq!(odd.bytes().await.unwrap(), b"odd key");

    let mut part = s3
        .get_object_range("bucket", "models/weights.bin", 100, Some(199))
        .await
        .unwrap();
    assert_eq!(part.meta().content_length, Some(100));
    let mut got = Vec::new();
    while let Some(c) = part.chunk().await.unwrap() {
        got.extend(c);
    }
    assert_eq!(got, content(70_000)[100..200]);

    let meta = s3
        .head_object("bucket", "models/weights.bin")
        .await
        .unwrap();
    assert_eq!(meta.content_length, Some(70_000));
    assert_eq!(
        meta.metadata,
        vec![("owner".to_string(), "tests".to_string())]
    );
    assert_eq!(meta.sha256_digest(), Some(Digest::of(&content(70_000))));

    let missing = s3.head_object("bucket", "nope").await.unwrap_err();
    assert!(missing.is_not_found(), "{missing:?}");
    let missing = s3.get_object("bucket", "nope").await.unwrap_err();
    match missing {
        StoreError::Remote {
            status,
            code,
            message,
        } => {
            assert_eq!((status, code.as_str()), (404, "NoSuchKey"));
            assert!(message.contains("does not exist"));
        }
        other => panic!("{other:?}"),
    }
    let reqs = state.lock().unwrap().requests.clone();
    assert!(reqs
        .iter()
        .any(|(_, r)| r.as_deref() == Some("bytes=100-199")));
    assert!(
        reqs.iter()
            .any(|(t, _)| t == "/bucket/dir/a%20%26%20b%2Bc.txt"),
        "{reqs:?}"
    );
}

#[tokio::test]
async fn wrong_secret_is_rejected_by_the_server() {
    let (base, _) = mock_s3(objects(), None).await;
    let s3 = client(&base, Credentials::new(KEY_ID, "not-the-secret"));
    let err = s3.get_object("bucket", "nosum").await.unwrap_err();
    match err {
        StoreError::Remote { status, code, .. } => {
            assert_eq!((status, code.as_str()), (403, "SignatureDoesNotMatch"))
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn session_token_and_unsigned_payload_against_the_server() {
    let (base, state) = mock_s3(objects(), Some("sess-123")).await;
    let s3 = S3Client::builder()
        .region(REGION)
        .endpoint(&base)
        .credentials(Credentials::new(KEY_ID, SECRET).with_session_token("sess-123"))
        .unsigned_payload(true)
        .build()
        .unwrap();
    let obj = s3.get_object("bucket", "nosum").await.unwrap();
    assert_eq!(obj.bytes().await.unwrap(), b"no checksum stored");
    assert_eq!(
        state.lock().unwrap().tokens_seen,
        vec![Some("sess-123".to_string())]
    );
}

#[tokio::test]
async fn list_objects_v2_pagination() {
    let (base, state) = mock_s3(objects(), None).await;
    let s3 = client(&base, Credentials::new(KEY_ID, SECRET));

    let first = s3
        .list_objects_v2("bucket", &ListObjects::new().prefix("logs/").max_keys(2))
        .await
        .unwrap();
    assert!(first.is_truncated);
    assert_eq!(first.objects.len(), 2);
    assert_eq!(first.objects[0].key, "logs/00.log");
    assert_eq!(first.objects[0].etag.as_deref(), Some("e"));
    assert_eq!(first.next_continuation_token.as_deref(), Some("tok-2"));

    let all = s3
        .list_all(
            "bucket",
            &ListObjects::new().prefix("logs/").max_keys(2),
            100,
        )
        .await
        .unwrap();
    let keys: Vec<_> = all.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "logs/00.log",
            "logs/01.log",
            "logs/02.log",
            "logs/03.log",
            "logs/04.log"
        ]
    );
    let targets: Vec<String> = state
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|(t, _)| t.clone())
        .collect();
    assert!(
        targets
            .iter()
            .any(|t| t.contains("continuation-token=tok-4")),
        "{targets:?}"
    );

    let err = s3
        .list_all("bucket", &ListObjects::new().max_keys(3), 4)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");

    let everything = s3
        .list_all("bucket", &ListObjects::new(), 100)
        .await
        .unwrap();
    assert_eq!(everything.len(), objects().len());
    assert!(everything.iter().any(|o| o.key == "dir/a & b+c.txt"));
}

#[tokio::test]
async fn download_into_the_cache() {
    let dir = Scratch::new("s3");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, state) = mock_s3(objects(), None).await;
    let s3 = client(&base, Credentials::new(KEY_ID, SECRET));
    let data = content(70_000);
    let digest = Digest::of(&data);

    // Keyed by the checksum HeadObject reports.
    let stored = s3
        .download(&cache, "bucket", "models/weights.bin", None)
        .await
        .unwrap();
    assert_eq!(stored.digest, digest);
    assert_eq!(std::fs::read(&stored.path).unwrap(), data);

    // With the digest known up front, a hit costs no request at all.
    let before = state.lock().unwrap().requests.len();
    let again = s3
        .download(&cache, "bucket", "models/weights.bin", Some(&digest))
        .await
        .unwrap();
    assert_eq!(again, stored);
    assert_eq!(state.lock().unwrap().requests.len(), before);

    // No checksum stored: hashed while it is stored.
    let plain = s3.download(&cache, "bucket", "nosum", None).await.unwrap();
    assert_eq!(plain.digest, Digest::of(b"no checksum stored"));

    // The wrong expected digest is rejected and nothing is kept.
    let wrong = Digest::of(b"something else");
    let err = s3
        .download(&cache, "bucket", "models/config.json", Some(&wrong))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::DigestMismatch { .. }), "{err:?}");
    assert!(!cache.contains(&wrong));
}

#[tokio::test]
async fn resumed_s3_download_signs_the_range() {
    let dir = Scratch::new("s3resume");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, state) = mock_s3(objects(), None).await;
    let s3 = client(&base, Credentials::new(KEY_ID, SECRET));
    let data = content(70_000);
    let digest = Digest::of(&data);

    // Interrupt a first attempt after 30 000 bytes.
    let served = AtomicUsize::new(0);
    let err = cache
        .get_or_fetch(&digest, |offset| {
            served.fetch_add(1, Ordering::SeqCst);
            let head = data[..30_000].to_vec();
            async move {
                assert_eq!(offset, 0);
                struct Cut(Option<Vec<u8>>);
                impl ferroly::store::ByteStream for Cut {
                    fn next_chunk(
                        &mut self,
                    ) -> ferroly::store::BoxFuture<'_, Result<Option<Vec<u8>>, StoreError>>
                    {
                        let next = self.0.take();
                        Box::pin(async move {
                            match next {
                                Some(c) => Ok(Some(c)),
                                None => Err(StoreError::Protocol("cut".into())),
                            }
                        })
                    }
                }
                Ok(ferroly::store::Fetched::new(Cut(Some(head))))
            }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Protocol(_)));
    assert_eq!(cache.partial_len(&digest), Some(30_000));

    let stored = s3
        .download(&cache, "bucket", "models/weights.bin", Some(&digest))
        .await
        .unwrap();
    assert_eq!(std::fs::read(stored.path).unwrap(), data);
    let reqs = state.lock().unwrap().requests.clone();
    assert_eq!(
        reqs,
        vec![(
            "/bucket/models/weights.bin".to_string(),
            Some("bytes=30000-".to_string())
        )],
        "one signed, ranged request"
    );
}

#[tokio::test]
async fn anonymous_requests_are_unsigned() {
    let base = spawn(|req| {
        if req.headers.contains("authorization") || req.headers.contains("x-amz-date") {
            HttpResponse::new(StatusCode::BAD_REQUEST)
        } else {
            HttpResponse::new(StatusCode::OK).body("public")
        }
    })
    .await;
    let s3 = S3Client::builder()
        .region("us-east-1")
        .endpoint(&base)
        .build()
        .unwrap();
    let obj = s3.get_object("public-bucket", "file").await.unwrap();
    assert_eq!(obj.bytes().await.unwrap(), b"public");
}

#[tokio::test]
async fn hostile_listing_is_bounded() {
    // A listing that claims more pages but repeats its token.
    let base = spawn(|_req| {
        HttpResponse::new(StatusCode::OK)
            .header("content-type", "application/xml")
            .body(
                "<ListBucketResult><IsTruncated>true</IsTruncated>\
                 <NextContinuationToken>same</NextContinuationToken></ListBucketResult>",
            )
    })
    .await;
    let s3 = S3Client::builder()
        .region("us-east-1")
        .endpoint(&base)
        .build()
        .unwrap();
    let err = s3
        .list_all("bucket", &ListObjects::new(), 1000)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Protocol(_)), "{err:?}");

    // An oversized listing body is refused before it is buffered.
    let big =
        spawn(|_req| HttpResponse::new(StatusCode::OK).body(vec![b' '; 17 * 1024 * 1024])).await;
    let s3 = S3Client::builder()
        .region("us-east-1")
        .endpoint(&big)
        .build()
        .unwrap();
    let err = s3
        .list_objects_v2("bucket", &ListObjects::new())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Protocol(_)), "{err:?}");
}

// ---- the caller's client settings, clocks and conditional GETs --------

#[tokio::test]
async fn a_redirect_following_client_never_forwards_the_session_token() {
    // ST-3: a client that follows redirects itself used to carry the
    // session token and the signed headers to the redirect target.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let elsewhere = spawn(move |req| {
        log.lock().unwrap().push(req.uri.request_target());
        HttpResponse::new(StatusCode::OK).body("hijacked")
    })
    .await;
    let base = spawn(move |req| {
        HttpResponse::new(StatusCode::TEMPORARY_REDIRECT).header(
            "location",
            format!("{elsewhere}{}", req.uri.request_target()),
        )
    })
    .await;
    let following = ferroly::http::Client::builder()
        .max_redirects(5)
        .build()
        .unwrap();
    let s3 = S3Client::builder()
        .region(REGION)
        .endpoint(&base)
        .credentials(Credentials::new(KEY_ID, SECRET).with_session_token("SESSION-TOKEN"))
        .client(following)
        .build()
        .unwrap();
    let err = s3.get_object("bucket", "k").await.unwrap_err();
    assert_eq!(err.status(), Some(307), "{err:?}");
    let err = s3.head_object("bucket", "k").await.unwrap_err();
    assert_eq!(err.status(), Some(307), "{err:?}");
    let dir = Scratch::new("s3-redirect");
    let cache = Cache::open(dir.path()).unwrap();
    let err = s3
        .download(&cache, "bucket", "k", Some(&Digest::of(b"hijacked")))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(307), "{err:?}");
    assert!(
        seen.lock().unwrap().is_empty(),
        "the redirect target was contacted: {:?}",
        seen.lock().unwrap()
    );
}

#[tokio::test]
async fn an_object_stored_with_content_encoding_is_cached_as_stored() {
    // ST-13: a decompressing client used to decode the body, so the digest
    // of the stored object never matched.
    let stored = b"\x1f\x8b\x08 stored gzip-encoded, served as such".to_vec();
    let body = stored.clone();
    let base = spawn(move |req| serve_range(&req, &body).header("content-encoding", "gzip")).await;
    let s3 = S3Client::builder()
        .region(REGION)
        .endpoint(&base)
        .client(
            ferroly::http::Client::builder()
                .decompress(true)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let dir = Scratch::new("s3-encoded");
    let cache = Cache::open(dir.path()).unwrap();
    let got = s3
        .download(&cache, "bucket", "k.gz", Some(&Digest::of(&stored)))
        .await
        .unwrap();
    assert_eq!(std::fs::read(got.path).unwrap(), stored);
}

#[tokio::test]
async fn a_skewed_clock_is_corrected_from_the_server_date() {
    let ahead = Duration::from_secs(3600);
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let base = spawn(move |req| {
        counter.fetch_add(1, Ordering::SeqCst);
        let server_now = SystemTime::now() + ahead;
        let date = ferroly::http::format_http_date(server_now);
        if let Err(why) = verify_signature(&req, SECRET) {
            return xml_error(StatusCode::FORBIDDEN, "SignatureDoesNotMatch", &why)
                .header("date", date);
        }
        let amz = req.headers.get("x-amz-date").unwrap_or("").to_string();
        let earliest = sigv4::amz_datetime(server_now - Duration::from_secs(300));
        if amz < earliest {
            return xml_error(
                StatusCode::FORBIDDEN,
                "RequestTimeTooSkewed",
                "The difference between the request time and the current time is too large.",
            )
            .header("date", date);
        }
        HttpResponse::new(StatusCode::OK)
            .header("date", date)
            .body("on time")
    })
    .await;
    // The mock is plain http, where the correction is off unless asked for.
    let s3 = skew_client(&base, Some(true));
    let obj = s3.get_object("bucket", "k").await.unwrap();
    assert_eq!(obj.bytes().await.unwrap(), b"on time");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "one refusal, one re-signed retry"
    );
    // The correction is kept: the next request is on time at once.
    s3.head_object("bucket", "k").await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    // A plain 403 from a server whose clock agrees is not retried.
    let refusals = Arc::new(AtomicUsize::new(0));
    let counter = refusals.clone();
    let strict = spawn(move |_req| {
        counter.fetch_add(1, Ordering::SeqCst);
        xml_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied")
    })
    .await;
    let err = client(&strict, Credentials::new(KEY_ID, SECRET))
        .get_object("bucket", "k")
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert_eq!(refusals.load(Ordering::SeqCst), 1);
}

/// A mock that refuses every request with `403 <code>` and a `Date`
/// `ahead` of ours, recording each request's `x-amz-date`.
async fn refusing(code: &'static str, ahead: Duration) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let base = spawn(move |req| {
        log.lock()
            .unwrap()
            .push(req.headers.get("x-amz-date").unwrap_or("").to_string());
        let date = ferroly::http::format_http_date(SystemTime::now() + ahead);
        xml_error(StatusCode::FORBIDDEN, code, "refused").header("date", date)
    })
    .await;
    (base, seen)
}

fn skew_client(base: &str, correct: Option<bool>) -> S3Client {
    let builder = S3Client::builder()
        .region(REGION)
        .endpoint(base)
        .credentials(Credentials::new(KEY_ID, SECRET));
    match correct {
        Some(enabled) => builder.correct_clock_skew(enabled),
        None => builder,
    }
    .build()
    .unwrap()
}

/// SEC-3: the clock is corrected only for `RequestTimeTooSkewed`, by at most
/// a day, never by default over plain http, and kept only if the retry is
/// not refused as well.
#[tokio::test]
async fn clock_skew_is_learnt_only_from_a_believable_skew_refusal() {
    let on_time = |date: &str| {
        let late = sigv4::amz_datetime(SystemTime::now() + Duration::from_secs(5));
        let early = sigv4::amz_datetime(SystemTime::now() - Duration::from_secs(300));
        date >= early.as_str() && date <= late.as_str()
    };
    let hour = Duration::from_secs(3600);

    // AccessDenied from something with a stale clock: one request, and the
    // next request is still signed at local time.
    let (base, seen) = refusing("AccessDenied", hour).await;
    let s3 = skew_client(&base, Some(true));
    let err = s3.get_object("bucket", "k").await.unwrap_err();
    assert!(
        matches!(&err, StoreError::Remote { status: 403, code, .. } if code == "AccessDenied"),
        "{err:?}"
    );
    assert!(s3.get_object("bucket", "k").await.is_err());
    let dates = seen.lock().unwrap().clone();
    assert_eq!(dates.len(), 2, "{dates:?}");
    assert!(dates.iter().all(|d| on_time(d)), "{dates:?}");

    // A skew refusal whose retry is refused too: the correction is not kept.
    let (base, seen) = refusing("RequestTimeTooSkewed", hour).await;
    let s3 = skew_client(&base, Some(true));
    assert!(s3.get_object("bucket", "k").await.is_err());
    let dates = seen.lock().unwrap().clone();
    assert_eq!(dates.len(), 2, "one retry: {dates:?}");
    assert!(!on_time(&dates[1]), "the retry is re-dated: {dates:?}");
    assert!(s3.get_object("bucket", "k").await.is_err());
    let dates = seen.lock().unwrap().clone();
    assert_eq!(dates.len(), 4, "{dates:?}");
    assert!(on_time(&dates[2]), "{dates:?}");

    // A year-ahead Date is not followed.
    let (base, seen) = refusing("RequestTimeTooSkewed", hour * 24 * 365).await;
    let s3 = skew_client(&base, Some(true));
    assert!(s3.get_object("bucket", "k").await.is_err());
    assert_eq!(seen.lock().unwrap().len(), 1);

    // A HEAD has no error code: only a skew over 15 minutes counts.
    let ten_minutes = Duration::from_secs(10 * 60);
    let (base, seen) = refusing("RequestTimeTooSkewed", ten_minutes).await;
    let s3 = skew_client(&base, Some(true));
    assert!(s3.head_object("bucket", "k").await.is_err());
    assert_eq!(seen.lock().unwrap().len(), 1);

    // Over plain http the correction is off by default.
    let (base, seen) = refusing("RequestTimeTooSkewed", hour).await;
    let s3 = skew_client(&base, None);
    assert!(s3.get_object("bucket", "k").await.is_err());
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// SEC-2: a weak ETag is not sent as `If-Match` (a strong comparison never
/// matches it), so the download still succeeds.
#[tokio::test]
async fn download_with_a_weak_etag_skips_if_match() {
    let data = content(3_000);
    let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let log = seen.clone();
    let body = data.clone();
    let base = spawn(move |req| {
        let if_match = req.headers.get("if-match").map(str::to_string);
        log.lock().unwrap().push(if_match.clone());
        match (req.method.clone(), if_match) {
            (Method::Head, _) => HttpResponse::new(StatusCode::OK)
                .header("etag", "W/\"v1\"")
                .body(body.clone()),
            // What a strong comparison against a weak tag must answer.
            (_, Some(_)) => xml_error(StatusCode(412), "PreconditionFailed", "weak"),
            _ => serve_range(&req, &body),
        }
    })
    .await;
    let s3 = S3Client::builder()
        .region(REGION)
        .endpoint(&base)
        .build()
        .unwrap();
    let meta = s3.head_object("bucket", "k").await.unwrap();
    assert_eq!(meta.etag.as_deref(), Some("v1"));
    let dir = Scratch::new("s3-weak-etag");
    let cache = Cache::open(dir.path()).unwrap();
    let stored = s3.download(&cache, "bucket", "k", None).await.unwrap();
    assert_eq!(std::fs::read(stored.path).unwrap(), data);
    let log = seen.lock().unwrap();
    assert_eq!(log.len(), 3, "{log:?}");
    assert_eq!(log[2], None, "{log:?}");
}

#[tokio::test]
async fn download_sends_if_match_with_the_head_etag() {
    let data = content(5_000);
    let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let log = seen.clone();
    let body = data.clone();
    let base = spawn(move |req| {
        log.lock().unwrap().push((
            req.method.as_str().to_string(),
            req.headers.get("if-match").map(str::to_string),
        ));
        match (req.method.clone(), req.headers.get("if-match")) {
            (Method::Head, _) => HttpResponse::new(StatusCode::OK)
                .header("etag", "\"v1\"")
                .header(
                    "x-amz-checksum-sha256",
                    base64::encode(sha256(&body).as_bytes()),
                )
                .header("x-amz-checksum-type", "FULL_OBJECT")
                .body(body.clone()),
            // The object was replaced between the HEAD and the GET.
            (_, Some(tag)) if tag != "\"v2\"" => {
                xml_error(StatusCode(412), "PreconditionFailed", "changed")
            }
            _ => serve_range(&req, b"new content"),
        }
    })
    .await;
    let s3 = S3Client::builder()
        .region(REGION)
        .endpoint(&base)
        .build()
        .unwrap();
    let dir = Scratch::new("s3-ifmatch");
    let cache = Cache::open(dir.path()).unwrap();
    let err = s3.download(&cache, "bucket", "k", None).await.unwrap_err();
    assert_eq!(err.status(), Some(412), "{err:?}");
    let log = seen.lock().unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1], ("GET".to_string(), Some("\"v1\"".to_string())));
}

#[tokio::test]
async fn a_composite_checksum_is_not_used_as_the_key() {
    // ST-14: an undashed checksum marked COMPOSITE is not a content digest;
    // the object is hashed as it is stored instead of failing to verify.
    let data = b"multipart object".to_vec();
    let body = data.clone();
    let base = spawn(move |req| match req.method {
        Method::Head => HttpResponse::new(StatusCode::OK)
            .header(
                "x-amz-checksum-sha256",
                base64::encode(sha256(b"parts").as_bytes()),
            )
            .header("x-amz-checksum-type", "COMPOSITE")
            .body(body.clone()),
        _ => serve_range(&req, &body),
    })
    .await;
    let s3 = S3Client::builder()
        .region(REGION)
        .endpoint(&base)
        .build()
        .unwrap();
    let dir = Scratch::new("s3-composite");
    let cache = Cache::open(dir.path()).unwrap();
    let stored = s3.download(&cache, "bucket", "k", None).await.unwrap();
    assert_eq!(stored.digest, Digest::of(&data));
}

#[tokio::test]
async fn a_wrong_region_answer_names_the_bucket_region() {
    let base = spawn(|_req| {
        xml_error(
            StatusCode::MOVED_PERMANENTLY,
            "PermanentRedirect",
            "The bucket you are attempting to access must be addressed using the specified endpoint.",
        )
        .header("x-amz-bucket-region", "ap-south-1")
    })
    .await;
    let s3 = client(&base, Credentials::new(KEY_ID, SECRET));
    match s3.get_object("bucket", "k").await.unwrap_err() {
        StoreError::Remote {
            status,
            code,
            message,
        } => {
            assert_eq!((status, code.as_str()), (301, "PermanentRedirect"));
            assert!(message.contains("ap-south-1"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    let err = s3.head_object("bucket", "k").await.unwrap_err();
    assert!(err.to_string().contains("ap-south-1"), "{err}");
}
