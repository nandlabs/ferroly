#![cfg(feature = "store-oci")]
//! OCI adapter against a loopback registry with a token service and a
//! separate loopback blob store it redirects to.

mod store_common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use ferroly::hash::base64;
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::store::oci::{OciClient, Reference, MANIFEST_ACCEPT, OCI_INDEX, OCI_MANIFEST};
use ferroly::store::{Cache, Digest, StoreError};
use store_common::{content, serve_range, spawn, Scratch};

#[derive(Default)]
struct Log {
    token_requests: Vec<String>,
    storage: Vec<(String, Option<String>, bool)>,
    accept: Vec<String>,
}

struct Registry {
    base: String,
    log: Arc<Mutex<Log>>,
    config: Vec<u8>,
    layer: Vec<u8>,
    manifest: Vec<u8>,
}

fn manifest_for(config: &[u8], layer: &[u8]) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}",
"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},
"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{},
"annotations":{{"org.opencontainers.image.title":"weights.bin"}}}}]}}"#,
        Digest::of(config),
        config.len(),
        Digest::of(layer),
        layer.len()
    )
    .into_bytes()
}

fn basic(user: &str, pass: &str) -> String {
    format!(
        "Basic {}",
        base64::encode(format!("{user}:{pass}").as_bytes())
    )
}

async fn registry() -> Registry {
    let config = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}".to_vec();
    let layer = content(90_000);
    let manifest = manifest_for(&config, &layer);
    let log = Arc::new(Mutex::new(Log::default()));

    let mut blobs = HashMap::new();
    blobs.insert(Digest::of(&config).hex(), config.clone());
    blobs.insert(Digest::of(&layer).hex(), layer.clone());
    let blobs = Arc::new(blobs);

    let storage_log = log.clone();
    let storage_blobs = blobs.clone();
    let storage = spawn(move |req| {
        storage_log.lock().unwrap().storage.push((
            req.uri.path.clone(),
            req.headers.get("range").map(str::to_string),
            req.headers.contains("authorization"),
        ));
        let hex = req.uri.path.trim_start_matches("/storage/");
        match storage_blobs.get(hex) {
            Some(b) => serve_range(&req, b),
            None => HttpResponse::new(StatusCode::NOT_FOUND),
        }
    })
    .await;

    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{{"mediaType":"{OCI_MANIFEST}","digest":"{}","size":{},"platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#,
        Digest::of(&manifest),
        manifest.len()
    )
    .into_bytes();

    let me: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let realm_base = me.clone();
    let reg_log = log.clone();
    let reg_manifest = manifest.clone();
    let base = spawn(move |req| {
        let path = req.uri.path.clone();
        if path == "/token" {
            let query = req.uri.query.clone().unwrap_or_default();
            reg_log.lock().unwrap().token_requests.push(query.clone());
            if !query.contains("service=test-registry") {
                return HttpResponse::new(StatusCode::BAD_REQUEST);
            }
            let repo = if query.contains("repository%3Aorg%2Fapp%3Apull") {
                "org/app"
            } else if query.contains("repository%3Apublic%2Fapp%3Apull") {
                "public/app"
            } else if query.contains("repository%3Aorg%2Findex%3Apull") {
                "org/index"
            } else {
                return HttpResponse::new(StatusCode::BAD_REQUEST);
            };
            if repo != "public/app"
                && req.headers.get("authorization") != Some(basic("user", "pass").as_str())
            {
                return HttpResponse::new(StatusCode::UNAUTHORIZED);
            }
            return HttpResponse::new(StatusCode::OK)
                .header("content-type", "application/json")
                .body(format!(r#"{{"token":"tok-{repo}","expires_in":300}}"#));
        }
        let Some(rest) = path.strip_prefix("/v2/") else {
            return HttpResponse::new(StatusCode::NOT_FOUND);
        };
        let (repo, kind, reference) = if let Some((repo, r)) = rest.split_once("/manifests/") {
            (repo.to_string(), "manifests", r.to_string())
        } else if let Some((repo, r)) = rest.split_once("/blobs/") {
            (repo.to_string(), "blobs", r.to_string())
        } else {
            return HttpResponse::new(StatusCode::NOT_FOUND);
        };

        // Authentication.
        let auth = req.headers.get("authorization").unwrap_or("").to_string();
        if repo == "basic/app" {
            if auth != basic("user", "pass") {
                return HttpResponse::new(StatusCode::UNAUTHORIZED)
                    .header("www-authenticate", "Basic realm=\"Registry Realm\"");
            }
        } else if auth != format!("Bearer tok-{repo}") {
            let realm = realm_base.get().unwrap();
            return HttpResponse::new(StatusCode::UNAUTHORIZED)
                .header("content-type", "application/json")
                .header(
                    "www-authenticate",
                    format!(
                        "Bearer realm=\"{realm}/token\",service=\"test-registry\",scope=\"repository:{repo}:pull\""
                    ),
                )
                .body(r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#);
        }

        if kind == "manifests" {
            let accept = req.headers.get("accept").unwrap_or("").to_string();
            reg_log.lock().unwrap().accept.push(accept.clone());
            if !accept.contains(OCI_MANIFEST) || !accept.contains(OCI_INDEX) {
                return HttpResponse::new(StatusCode::NOT_ACCEPTABLE);
            }
            let body = if repo == "org/index" { index.clone() } else { reg_manifest.clone() };
            let digest = Digest::of(&body);
            if reference != "v1" && reference != digest.to_string() {
                return HttpResponse::new(StatusCode::NOT_FOUND)
                    .header("content-type", "application/json")
                    .body(r#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"manifest unknown"}]}"#);
            }
            return HttpResponse::new(StatusCode::OK)
                .header("content-type", OCI_MANIFEST)
                .header("docker-content-digest", digest.to_string())
                .body(body);
        }
        let Ok(digest) = reference.parse::<Digest>() else {
            return HttpResponse::new(StatusCode::BAD_REQUEST);
        };
        if repo == "basic/app" {
            return match blobs.get(&digest.hex()) {
                Some(b) => serve_range(&req, b),
                None => HttpResponse::new(StatusCode::NOT_FOUND),
            };
        }
        HttpResponse::new(StatusCode::TEMPORARY_REDIRECT)
            .header("location", format!("{storage}/storage/{}", digest.hex()))
    })
    .await;
    me.set(base.clone()).unwrap();
    let base = base.trim_start_matches("http://").to_string();
    Registry {
        base,
        log,
        config,
        layer,
        manifest,
    }
}

fn reference(reg: &Registry, repo: &str) -> Reference {
    Reference::parse(&format!("{}/{repo}:v1", reg.base)).unwrap()
}

fn authed() -> OciClient {
    OciClient::new()
        .plain_http(true)
        .with_basic_auth("user", "pass")
}

#[tokio::test]
async fn token_flow_manifest_and_blobs() {
    let dir = Scratch::new("oci");
    let cache = Cache::open(dir.path()).unwrap();
    let reg = registry().await;
    let oci = authed();
    let r = reference(&reg, "org/app");

    let pulled = oci.pull(&cache, &r).await.unwrap();
    assert_eq!(pulled.manifest.digest, Digest::of(&reg.manifest));
    assert_eq!(
        pulled.manifest.layers[0].title.as_deref(),
        Some("weights.bin")
    );
    assert_eq!(std::fs::read(&pulled.manifest_path).unwrap(), reg.manifest);
    assert_eq!(pulled.blobs.len(), 2);
    assert_eq!(std::fs::read(&pulled.blobs[0].path).unwrap(), reg.config);
    assert_eq!(std::fs::read(&pulled.blobs[1].path).unwrap(), reg.layer);

    {
        let log = reg.log.lock().unwrap();
        assert_eq!(
            log.token_requests.len(),
            1,
            "the token is cached: {:?}",
            log.token_requests
        );
        assert_eq!(log.accept[0], MANIFEST_ACCEPT);
        assert_eq!(log.storage.len(), 2);
        assert!(
            log.storage.iter().all(|s| !s.2),
            "credentials leaked to storage"
        );
    }

    // Everything is cached: pulling again fetches only the manifest.
    let again = oci.pull(&cache, &r).await.unwrap();
    assert_eq!(again.blobs, pulled.blobs);
    assert_eq!(reg.log.lock().unwrap().storage.len(), 2);

    // By digest.
    let by_digest = r.clone().with_digest(pulled.manifest.digest);
    let m = oci.manifest(&by_digest).await.unwrap();
    assert_eq!(m.bytes, reg.manifest);
}

#[tokio::test]
async fn anonymous_token() {
    let dir = Scratch::new("oci-anon");
    let cache = Cache::open(dir.path()).unwrap();
    let reg = registry().await;
    let oci = OciClient::new().plain_http(true);
    let pulled = oci
        .pull(&cache, &reference(&reg, "public/app"))
        .await
        .unwrap();
    assert_eq!(pulled.blobs.len(), 2);

    // A repository whose token needs credentials.
    let err = oci.manifest(&reference(&reg, "org/app")).await.unwrap_err();
    assert!(matches!(err, StoreError::Auth(_)), "{err:?}");
    let shown = format!("{:?}", authed());
    assert!(!shown.contains("pass\""), "{shown}");
}

#[tokio::test]
async fn wrong_credentials_fail_cleanly() {
    let reg = registry().await;
    let oci = OciClient::new()
        .plain_http(true)
        .with_basic_auth("user", "wrong");
    let err = oci.manifest(&reference(&reg, "org/app")).await.unwrap_err();
    assert!(matches!(err, StoreError::Auth(_)), "{err:?}");
    let err = oci
        .manifest(&reference(&reg, "basic/app"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn basic_challenge() {
    let dir = Scratch::new("oci-basic");
    let cache = Cache::open(dir.path()).unwrap();
    let reg = registry().await;
    let pulled = authed()
        .pull(&cache, &reference(&reg, "basic/app"))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&pulled.blobs[1].path).unwrap(), reg.layer);
    let anon = OciClient::new().plain_http(true);
    let err = anon
        .manifest(&reference(&reg, "basic/app"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn unknown_manifest_is_a_registry_error() {
    let reg = registry().await;
    let oci = authed();
    let wrong = reference(&reg, "org/app").with_digest(Digest::of(b"not the manifest"));
    let err = oci.manifest(&wrong).await.unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
    match err {
        StoreError::Remote { code, .. } => assert_eq!(code, "MANIFEST_UNKNOWN"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn index_is_not_pulled_blindly() {
    let dir = Scratch::new("oci-index");
    let cache = Cache::open(dir.path()).unwrap();
    let reg = registry().await;
    let oci = authed();
    let r = reference(&reg, "org/index");
    let index = oci.manifest(&r).await.unwrap();
    assert!(index.is_index());
    assert_eq!(index.manifests[0].digest, Digest::of(&reg.manifest));
    assert_eq!(
        index.manifests[0].platform.as_ref().unwrap().architecture,
        "amd64"
    );
    let err = oci.pull(&cache, &r).await.unwrap_err();
    assert!(matches!(err, StoreError::Config(_)), "{err:?}");
}

#[tokio::test]
async fn interrupted_blob_resumes_through_the_redirect() {
    let dir = Scratch::new("oci-resume");
    let cache = Cache::open(dir.path()).unwrap();
    let reg = registry().await;
    let oci = authed();
    let r = reference(&reg, "org/app");
    let manifest = oci.manifest(&r).await.unwrap();
    let layer = &manifest.layers[0];

    let head = reg.layer[..50_000].to_vec();
    let _ = cache
        .get_or_fetch_sized(&layer.digest, layer.size, |_| {
            let head = head.clone();
            async move {
                Ok(
                    ferroly::store::Fetched::from_reader(std::io::Cursor::new(head))
                        .with_total(90_000),
                )
            }
        })
        .await
        .unwrap_err();
    assert_eq!(cache.partial_len(&layer.digest), Some(50_000));

    let path = oci.blob_to_cache(&cache, &r, layer).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), reg.layer);
    let log = reg.log.lock().unwrap();
    assert_eq!(log.storage.len(), 1);
    assert_eq!(log.storage[0].1.as_deref(), Some("bytes=50000-"));
}

#[tokio::test]
async fn header_digest_mismatch_is_rejected() {
    // A registry whose Docker-Content-Digest disagrees with the body.
    let body = manifest_for(b"c", b"l");
    let base = spawn(move |_req| {
        HttpResponse::new(StatusCode::OK)
            .header("content-type", OCI_MANIFEST)
            .header("docker-content-digest", Digest::of(b"lies").to_string())
            .body(body.clone())
    })
    .await;
    let host = base.trim_start_matches("http://");
    let r = Reference::parse(&format!("{host}/any/repo:v1")).unwrap();
    let err = OciClient::new()
        .plain_http(true)
        .manifest(&r)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::DigestMismatch { .. }), "{err:?}");
}

#[tokio::test]
async fn a_challenge_from_blob_storage_gets_no_credentials() {
    // ST-2: storage answering 401 with a realm of its choosing used to get
    // the registry's basic credentials sent there.
    let realm_log = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let log = realm_log.clone();
    let realm = spawn(move |req| {
        log.lock()
            .unwrap()
            .push(req.headers.get("authorization").map(str::to_string));
        HttpResponse::new(StatusCode::OK).body(r#"{"token":"stolen"}"#)
    })
    .await;
    let storage = spawn(move |_req| {
        HttpResponse::new(StatusCode::UNAUTHORIZED).header(
            "www-authenticate",
            format!("Bearer realm=\"{realm}/token\",service=\"storage\""),
        )
    })
    .await;
    let config = b"{}".to_vec();
    let layer = content(1_000);
    let manifest = manifest_for(&config, &layer);
    let base = spawn(move |req| {
        if req.uri.path.contains("/manifests/") {
            return HttpResponse::new(StatusCode::OK)
                .header("content-type", OCI_MANIFEST)
                .body(manifest.clone());
        }
        HttpResponse::new(StatusCode::TEMPORARY_REDIRECT)
            .header("location", format!("{storage}{}", req.uri.path))
    })
    .await;
    let host = base.trim_start_matches("http://");
    let r = Reference::parse(&format!("{host}/org/app:v1")).unwrap();
    let dir = Scratch::new("oci-foreign-401");
    let cache = Cache::open(dir.path()).unwrap();
    let err = authed().pull(&cache, &r).await.unwrap_err();
    assert_eq!(err.status(), Some(401), "{err:?}");
    assert!(
        realm_log.lock().unwrap().is_empty(),
        "the storage's realm was contacted: {:?}",
        realm_log.lock().unwrap()
    );
}

#[tokio::test]
async fn manifests_that_are_not_images_are_not_pulled() {
    // ST-7: Docker schema 1 and OCI artifact manifests used to "pull" as
    // zero blobs.
    let d = Digest::of(b"x");
    let schema1 = r#"{"schemaVersion":1,"name":"org/old","tag":"v1","fsLayers":[]}"#.to_string();
    let artifact = format!(
        r#"{{"mediaType":"application/vnd.oci.artifact.manifest.v1+json","artifactType":"t","blobs":[{{"digest":"{d}","size":1}}]}}"#
    );
    let base = spawn(move |req| {
        let (body, kind) = if req.uri.path.starts_with("/v2/org/old/") {
            (
                schema1.clone(),
                "application/vnd.docker.distribution.manifest.v1+prettyjws",
            )
        } else {
            (artifact.clone(), "application/json")
        };
        HttpResponse::new(StatusCode::OK)
            .header("content-type", kind)
            .body(body)
    })
    .await;
    let host = base.trim_start_matches("http://");
    let dir = Scratch::new("oci-not-image");
    let cache = Cache::open(dir.path()).unwrap();
    let oci = OciClient::new().plain_http(true);
    for repo in ["org/old", "org/artifact"] {
        let r = Reference::parse(&format!("{host}/{repo}:v1")).unwrap();
        let err = oci.pull(&cache, &r).await.unwrap_err();
        assert!(matches!(err, StoreError::Protocol(_)), "{repo}: {err:?}");
    }
}

#[tokio::test]
async fn tokens_are_not_shared_across_credentials() {
    // ST-8: a clone given credentials used to share its token cache with
    // the anonymous client it came from.
    let reg = registry().await;
    let anon = OciClient::new().plain_http(true);
    let user = anon.clone().with_basic_auth("user", "pass");
    let r = reference(&reg, "org/app");
    user.manifest(&r).await.unwrap();
    let err = anon.manifest(&r).await.unwrap_err();
    assert!(matches!(err, StoreError::Auth(_)), "{err:?}");
    // Clones without new credentials still share: no second token request.
    let requests = reg.log.lock().unwrap().token_requests.len();
    user.clone().manifest(&r).await.unwrap();
    assert_eq!(reg.log.lock().unwrap().token_requests.len(), requests);
}

#[tokio::test]
async fn a_redirect_following_client_is_handled_by_the_adapter() {
    let dir = Scratch::new("oci-following");
    let cache = Cache::open(dir.path()).unwrap();
    let reg = registry().await;
    let following = ferroly::http::Client::builder()
        .max_redirects(5)
        .decompress(true)
        .build()
        .unwrap();
    let oci = authed().with_client(following);
    let pulled = oci.pull(&cache, &reference(&reg, "org/app")).await.unwrap();
    assert_eq!(std::fs::read(&pulled.blobs[1].path).unwrap(), reg.layer);
    let log = reg.log.lock().unwrap();
    assert!(
        log.storage.iter().all(|s| !s.2),
        "credentials leaked to storage"
    );
}
