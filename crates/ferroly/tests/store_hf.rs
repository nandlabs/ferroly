#![cfg(feature = "store-hf")]
//! Hugging Face adapter against a loopback Hub and a separate loopback
//! "CDN": resolve, bearer auth, redirects, ETag keying, commit pinning.

mod store_common;

use std::sync::{Arc, Mutex};

use ferroly::http::{HttpResponse, Method, StatusCode};
use ferroly::store::hf::{HfClient, HfFile};
use ferroly::store::{Cache, Digest, StoreError};
use store_common::{content, serve_range, spawn, Scratch};

const TOKEN: &str = "hf_testtoken";
const COMMIT: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

#[derive(Default)]
struct Log {
    hub: Vec<(String, String)>,
    cdn: Vec<(String, Option<String>, bool)>,
}

/// Hub at the returned base URL; LFS downloads redirect to the CDN.
async fn hub() -> (String, Arc<Mutex<Log>>, Vec<u8>) {
    let weights = content(60_000);
    let log = Arc::new(Mutex::new(Log::default()));

    let cdn_log = log.clone();
    let cdn_body = weights.clone();
    let cdn = spawn(move |req| {
        cdn_log.lock().unwrap().cdn.push((
            req.uri.request_target(),
            req.headers.get("range").map(str::to_string),
            req.headers.contains("authorization"),
        ));
        if req.uri.path != "/blobs/weights" {
            return HttpResponse::new(StatusCode::NOT_FOUND);
        }
        serve_range(&req, &cdn_body)
    })
    .await;

    let sha = Digest::of(&weights).hex();
    let size = weights.len();
    let hub_log = log.clone();
    let base = spawn(move |req| {
        let target = req.uri.request_target();
        hub_log
            .lock()
            .unwrap()
            .hub
            .push((req.method.as_str().to_string(), target.clone()));
        let authorized = req.headers.get("authorization") == Some(&format!("Bearer {TOKEN}"));
        if target.starts_with("/org/gated/") && !authorized {
            return HttpResponse::new(StatusCode::UNAUTHORIZED)
                .header("x-error-code", "GatedRepo")
                .header("x-error-message", "Access to this repo is restricted.");
        }
        let commit_ok = |rev: &str| rev == "main" || rev == COMMIT;
        let parts: Vec<&str> = target.trim_start_matches('/').split('/').collect();
        // [datasets/] org / name / resolve / rev / file...
        let parts = if parts.first() == Some(&"datasets") {
            &parts[1..]
        } else {
            &parts[..]
        };
        match parts {
            ["old", "model", "resolve", rev, file] => {
                HttpResponse::new(StatusCode::TEMPORARY_REDIRECT)
                    .header("location", format!("/org/model/resolve/{rev}/{file}"))
            }
            [_, _, "resolve", rev, "weights.bin"] if commit_ok(rev) => {
                HttpResponse::new(StatusCode::FOUND)
                    .header("location", format!("{cdn}/blobs/weights"))
                    .header("x-linked-etag", format!("\"{sha}\""))
                    .header("x-linked-size", size.to_string())
                    .header("etag", "W/\"redirect-etag\"")
                    .header("x-repo-commit", COMMIT)
            }
            [_, _, "resolve", rev, "config.json"] if commit_ok(rev) => {
                let mut r = HttpResponse::new(StatusCode::OK)
                    .header("etag", "\"0123456789abcdef0123456789abcdef01234567\"")
                    .header("x-repo-commit", COMMIT)
                    .body("{\"hidden_size\":64}");
                if req.method == Method::Head {
                    r = r.header("content-type", "application/json");
                }
                r
            }
            _ => HttpResponse::new(StatusCode::NOT_FOUND)
                .header("x-error-code", "EntryNotFound")
                .header("x-error-message", "Entry not found"),
        }
    })
    .await;
    (base, log, weights)
}

/// The loopback hub is plain http, so sending the token needs the opt-in.
fn client(base: &str) -> HfClient {
    HfClient::from_lookup(|name| match name {
        "HF_ENDPOINT" => Some(base.to_string()),
        "HF_TOKEN" => Some(TOKEN.to_string()),
        _ => None,
    })
    .unwrap()
    .plain_http(true)
}

#[tokio::test]
async fn lfs_file_is_keyed_by_its_sha256_and_pinned_to_the_commit() {
    let dir = Scratch::new("hf");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, log, weights) = hub().await;
    let hf = client(&base);
    let file = HfFile::model("org/model", "weights.bin");

    let info = hf.file_info(&file).await.unwrap();
    assert_eq!(info.commit.as_deref(), Some(COMMIT));
    assert_eq!(info.digest, Some(Digest::of(&weights)));
    assert_eq!(info.size, Some(60_000));
    assert!(info.location.unwrap().ends_with("/blobs/weights"));

    let got = hf.download(&cache, &file).await.unwrap();
    assert_eq!(got.digest, Digest::of(&weights));
    assert_eq!(got.commit.as_deref(), Some(COMMIT));
    assert_eq!(std::fs::read(&got.path).unwrap(), weights);

    {
        let log = log.lock().unwrap();
        // The GET went to the pinned commit, not to `main`.
        assert!(log
            .hub
            .iter()
            .any(|(m, t)| m == "GET" && t == &format!("/org/model/resolve/{COMMIT}/weights.bin")));
        assert_eq!(log.cdn.len(), 1);
        assert!(!log.cdn[0].2, "the token must not reach the CDN");
    }

    // Second download: a HEAD, then a cache hit.
    let again = hf.download(&cache, &file).await.unwrap();
    assert_eq!(again, got);
    assert_eq!(log.lock().unwrap().cdn.len(), 1);
}

#[tokio::test]
async fn non_lfs_file_is_hashed_on_store() {
    let dir = Scratch::new("hf-small");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, _, _) = hub().await;
    let hf = client(&base);
    let file = HfFile::model("org/model", "config.json");
    let info = hf.file_info(&file).await.unwrap();
    assert_eq!(info.digest, None);
    assert_eq!(
        info.etag.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    let got = hf.download(&cache, &file).await.unwrap();
    assert_eq!(got.digest, Digest::of(b"{\"hidden_size\":64}"));
    assert_eq!(std::fs::read(got.path).unwrap(), b"{\"hidden_size\":64}");
}

#[tokio::test]
async fn token_is_required_for_gated_repos() {
    let dir = Scratch::new("hf-gated");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, _, weights) = hub().await;
    let anonymous = HfClient::new().with_endpoint(&base).unwrap();
    let file = HfFile::model("org/gated", "weights.bin");
    let err = anonymous.download(&cache, &file).await.unwrap_err();
    match err {
        StoreError::Remote {
            status,
            code,
            message,
        } => {
            assert_eq!((status, code.as_str()), (401, "GatedRepo"));
            assert!(message.contains("restricted"));
        }
        other => panic!("{other:?}"),
    }
    let got = client(&base).download(&cache, &file).await.unwrap();
    assert_eq!(std::fs::read(got.path).unwrap(), weights);

    let missing = client(&base)
        .file_info(&HfFile::model("org/model", "nope.txt"))
        .await
        .unwrap_err();
    assert!(missing.is_not_found(), "{missing:?}");
}

#[tokio::test]
async fn same_origin_redirects_are_followed() {
    let dir = Scratch::new("hf-renamed");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, _, weights) = hub().await;
    let hf = client(&base);
    // A renamed repository answers with a relative redirect.
    let info = hf
        .file_info(&HfFile::model("old/model", "weights.bin"))
        .await
        .unwrap();
    assert_eq!(info.digest, Some(Digest::of(&weights)));
    let mut resp = hf
        .open(&HfFile::model("old/model", "weights.bin"))
        .await
        .unwrap();
    resp.set_max_body_bytes(None);
    assert_eq!(resp.bytes().await.unwrap(), weights);
    let _ = cache;
}

#[tokio::test]
async fn partial_hf_download_resumes_at_the_cdn() {
    let dir = Scratch::new("hf-resume");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, log, weights) = hub().await;
    let digest = Digest::of(&weights);

    // Leave the first 25 000 bytes parked, as an interrupted run would.
    let head = weights[..25_000].to_vec();
    let _ = cache
        .get_or_fetch(&digest, |_| {
            let head = head.clone();
            async move {
                Ok(
                    ferroly::store::Fetched::from_reader(std::io::Cursor::new(head))
                        .with_total(60_000),
                )
            }
        })
        .await
        .unwrap_err();
    assert_eq!(cache.partial_len(&digest), Some(25_000));

    let got = client(&base)
        .download(&cache, &HfFile::dataset("org/data", "weights.bin"))
        .await
        .unwrap();
    assert_eq!(std::fs::read(got.path).unwrap(), weights);
    let cdn = &log.lock().unwrap().cdn;
    assert_eq!(cdn.len(), 1);
    assert_eq!(cdn[0].1.as_deref(), Some("bytes=25000-"));
}

#[tokio::test]
async fn redirect_loops_are_bounded() {
    let hits = Arc::new(Mutex::new(0usize));
    let counter = hits.clone();
    let base = spawn(move |req| {
        *counter.lock().unwrap() += 1;
        HttpResponse::new(StatusCode::FOUND).header("location", req.uri.request_target())
    })
    .await;
    let hf = HfClient::new().with_endpoint(&base).unwrap();
    let err = hf
        .open(&HfFile::model("org/model", "loop.bin"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Protocol(_)), "{err:?}");
    assert_eq!(*hits.lock().unwrap(), 11, "the first request plus ten hops");
}

/// A hub whose CDN serves `served` for an LFS file whose SHA-256 is that of
/// `real`; logs whether the CDN saw an `Authorization` header.
async fn tampering_hub(real: Vec<u8>, served: Vec<u8>) -> (String, Arc<Mutex<Vec<bool>>>) {
    let cdn_auth = Arc::new(Mutex::new(Vec::new()));
    let log = cdn_auth.clone();
    let cdn = spawn(move |req| {
        log.lock()
            .unwrap()
            .push(req.headers.contains("authorization"));
        serve_range(&req, &served).header("content-encoding", "gzip")
    })
    .await;
    let sha = Digest::of(&real).hex();
    let size = real.len();
    let base = spawn(move |_req| {
        HttpResponse::new(StatusCode::FOUND)
            .header("location", format!("{cdn}/blobs/weights"))
            .header("x-linked-etag", format!("\"{sha}\""))
            .header("x-linked-size", size.to_string())
            .header("x-repo-commit", COMMIT)
    })
    .await;
    (base, cdn_auth)
}

#[tokio::test]
async fn a_redirect_following_client_still_verifies_lfs_content() {
    // ST-4: with a client that follows redirects itself, the HEAD used to
    // land on the CDN, lose the LFS digest, and store tampered bytes.
    let real = content(4_000);
    let mut tampered = real.clone();
    tampered[100] ^= 0xff;
    let (base, cdn_auth) = tampering_hub(real.clone(), tampered).await;
    let dir = Scratch::new("hf-following");
    let cache = Cache::open(dir.path()).unwrap();
    let following = ferroly::http::Client::builder()
        .max_redirects(5)
        .build()
        .unwrap();
    let hf = client(&base).with_client(following);
    let file = HfFile::model("org/model", "weights.bin");

    let info = hf.file_info(&file).await.unwrap();
    assert_eq!(info.digest, Some(Digest::of(&real)));
    assert_eq!(info.commit.as_deref(), Some(COMMIT));

    let err = hf.download(&cache, &file).await.unwrap_err();
    assert!(matches!(err, StoreError::DigestMismatch { .. }), "{err:?}");
    assert!(!cache.contains(&Digest::of(&real)));
    assert_eq!(store_common::files_in(&dir.path().join("sha256")), 0);
    assert_eq!(store_common::files_in(&dir.path().join("tmp")), 0);
    assert!(
        cdn_auth.lock().unwrap().iter().all(|sent| !sent),
        "the token reached the CDN"
    );
}

#[tokio::test]
async fn a_decompressing_client_caches_the_bytes_as_served() {
    // ST-13: the CDN marks the body `Content-Encoding: gzip`; the digest is
    // of the bytes as stored, so they must not be decoded.
    let stored = b"\x1f\x8b\x08 not decoded by the cache".to_vec();
    let (base, _) = tampering_hub(stored.clone(), stored.clone()).await;
    let dir = Scratch::new("hf-decompress");
    let cache = Cache::open(dir.path()).unwrap();
    let decoding = ferroly::http::Client::builder()
        .decompress(true)
        .build()
        .unwrap();
    let hf = HfClient::new()
        .with_endpoint(&base)
        .unwrap()
        .with_client(decoding);
    let got = hf
        .download(&cache, &HfFile::model("org/model", "weights.bin"))
        .await
        .unwrap();
    assert_eq!(std::fs::read(got.path).unwrap(), stored);
}

#[tokio::test]
async fn the_token_is_not_sent_over_plain_http_without_the_opt_in() {
    // ST-15.
    let hits = Arc::new(Mutex::new(0usize));
    let counter = hits.clone();
    let base = spawn(move |_req| {
        *counter.lock().unwrap() += 1;
        HttpResponse::new(StatusCode::OK).body("x")
    })
    .await;
    let hf = HfClient::from_lookup(|name| match name {
        "HF_ENDPOINT" => Some(base.clone()),
        "HF_TOKEN" => Some(TOKEN.to_string()),
        _ => None,
    })
    .unwrap();
    let file = HfFile::model("org/model", "config.json");
    let err = hf.file_info(&file).await.unwrap_err();
    assert!(matches!(err, StoreError::Config(_)), "{err:?}");
    assert!(hf.open(&file).await.is_err());
    assert_eq!(*hits.lock().unwrap(), 0);
    assert!(hf.plain_http(true).file_info(&file).await.is_ok());
    assert_eq!(*hits.lock().unwrap(), 1);
}
