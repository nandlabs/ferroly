# Artifact store: content-addressed cache and fetch adapters

[← Docs index](README.md) · [← Project README](../README.md)

**Features:** `store` (the cache), `store-s3`, `store-hf`, `store-oci` (one per adapter) ·
**Module:** `ferroly::store`

Fetching remote artifacts — model weights, container layers, datasets, release
bundles — into a verified local cache. The core is [`Cache`](#cache), a
directory of files keyed by SHA-256 that verifies every file before it becomes
visible, resumes interrupted downloads, and is a no-op on a hit. Three adapters
feed it:

| Feature | Module | Source | Auth |
|---|---|---|---|
| `store` | `ferroly::store` | anything that yields bytes; plain HTTP(S) URLs | headers you add |
| `store-s3` | `ferroly::store::s3` | S3 and S3-compatible storage (MinIO, LocalStack, R2, Ceph) | AWS Signature Version 4 |
| `store-hf` | `ferroly::store::hf` | Hugging Face Hub files (models, datasets, spaces) | bearer token |
| `store-oci` | `ferroly::store::oci` | OCI / Docker registries (Distribution Spec v2) | registry token flow, basic |

Everything is built on [`http`](http.md) (streaming responses, ranges) and
[`hash`](hash.md) (streaming SHA-256, HMAC-SHA256, hex, base64, `Secret`). The
S3 and OCI adapters also use [`codec`](codec.md) to read XML listings and JSON
manifests. No dependency beyond those.

## Contents

- [Enabling](#enabling)
- [Quick start](#quick-start)
- [Cache](#cache) — layout, [semantics](#semantics), [API](#cache-api), [fetch callbacks](#fetch-callbacks-fetched)
- [Digest](#digest)
- [S3 adapter](#s3-adapter-store-s3) — [SigV4](#sigv4-signing), [credentials](#credentials)
- [Hugging Face adapter](#hugging-face-adapter-store-hf)
- [OCI adapter](#oci-adapter-store-oci)
- [Errors](#errors)
- [Retries](#retries)
- [Limits](#limits)
- [Limitations and what is not included](#limitations-and-what-is-not-included)

## Enabling

```toml
# the cache only (with plain-URL fetching)
ferroly = { version = "0.6", features = ["store"] }
# plus the adapters you use
ferroly = { version = "0.6", features = ["store-s3", "store-hf", "store-oci"] }
```

`store` pulls in `http` and `hash`; `store-s3` and `store-oci` add `codec`.

## Quick start

```rust,no_run
use ferroly::http::Client;
use ferroly::store::{Cache, Digest};

# async fn demo() -> Result<(), ferroly::store::StoreError> {
let cache = Cache::open("/var/cache/my-app")?;
let digest: Digest =
    "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".parse()?;

// Miss: download (resuming a partial file), verify, commit. Hit: no I/O at all.
let path = cache
    .fetch_url(&Client::new(), "https://example.com/hello.txt", &digest)
    .await?;
println!("{}", path.display());
# Ok(())
# }
```

With an adapter:

```rust,no_run
use ferroly::store::Cache;
use ferroly::store::hf::{HfClient, HfFile};
use ferroly::store::oci::{OciClient, Reference};
use ferroly::store::s3::S3Client;

# async fn demo() -> Result<(), ferroly::store::StoreError> {
let cache = Cache::open("/var/cache/artifacts")?;

let s3 = S3Client::from_env()?; // AWS_REGION, AWS_ACCESS_KEY_ID, …
let weights = s3.download(&cache, "my-bucket", "models/weights.bin", None).await?;

let hf = HfClient::from_env()?; // HF_ENDPOINT, HF_TOKEN
let model = hf.download(&cache, &HfFile::model("openai-community/gpt2", "config.json")).await?;

let oci = OciClient::new();
let image = oci.pull(&cache, &Reference::parse("ghcr.io/org/model:v1")?).await?;
# let _ = (weights, model, image);
# Ok(())
# }
```

## Cache

```text
<root>/sha256/ab/abcdef0123…   committed objects; the file name is the full hex digest
<root>/tmp/                    downloads in progress, one uniquely named file each
<root>/partial/                interrupted downloads, parked for resumption
```

### Semantics

| Situation | What happens |
|---|---|
| **Hit** | `get_or_fetch` returns the path; the fetch callback is never called. |
| **Miss** | The callback's bytes are streamed into a new file under `tmp/` and hashed as they arrive. Only when the SHA-256 matches (and the size, when known) is the file `fsync`ed and **renamed** into `sha256/` — so a committed object always holds exactly the bytes its digest names. |
| **Mismatch** | Wrong digest, a body longer than declared or than the size cap, or one that ends at the wrong size: the file is deleted and the call fails with `DigestMismatch` / `SizeMismatch`. Nothing is parked. |
| **Interrupted** | The source failed part-way (a dropped connection, a timeout, an error from your stream, a body that ends before its declared size, a stall or deadline set on the cache) or the future was **cancelled**: what arrived is moved to `partial/`. A short body is `StoreError::Interrupted` (retryable). |
| **Process death** | A process killed mid-download (SIGKILL, out of memory, a crash) parks nothing: its bytes stay in `tmp/`, are never resumed, and are removed by `purge_temp`. Resume survives errors and cancellation, not process death. |
| **Resume** | The next fetch of that digest claims the parked file, re-hashes it, and calls the callback with its length as the offset. Over HTTP that is `Range: bytes=<n>-`. A `206` continues the file; a `200` (range ignored) starts it over; a `416` means it was already complete. |
| **Stale partial** | If an attempt that resumed a parked prefix is rejected (mismatch, a range that starts elsewhere, a total smaller than the prefix), the cache retries **once** from byte 0 before failing. An attempt the source already started over (it ignored the range) is not retried. |
| **Concurrent, one process** | Fetches of the same digest through one `Cache` (or its clones) run one at a time: the first downloads, the others wait and get the hit. Different digests run in parallel. A waiter that is cancelled leaves the queue cleanly. A source that stalls holds the queue until it fails: bound it with `with_stall_timeout` / `with_fetch_timeout` (an HTTP source is also bounded by its client's timeouts). |
| **Concurrent, several processes** | No cross-process lock. Each fetch writes its own temp file (`create_new`, named from the process id, a counter and the time — no randomness needed, so it works on every platform). The first to finish renames its file into place; the others see the object and delete theirs. A parked partial is claimed by renaming it, which only one claimant can win. Nobody reads or writes another fetch's file. |
| **Permissions** | On Unix the directories the cache creates are `0700` and its files `0600` (before the umask): cached models, layers and objects are often private. An existing root keeps its own permissions; `Cache::open` removes group and other access from the `sha256/`, `tmp/` and `partial/` directories it owns, so a cache created by 0.5 or earlier (directories `0755`, objects `0644`) is no longer readable by others (skipped, without error, where this user may not change them, and never through a symlink). |
| **Durability** | Content is `fsync`ed before the rename, and on Unix the directory after it. A crash can lose a just-committed entry, never corrupt one. |

### Cache API

| Method | Description |
|---|---|
| `Cache::open(dir) -> Result<Cache>` | Opens or creates the cache (creates `sha256/`, `tmp/`, `partial/`). Sync. |
| `.with_max_object_bytes(Option<u64>)` | Cap on any one download (default none); a larger one is stopped as soon as it passes the cap. Without a cap, a source that declares no size can fill the disk: set one for untrusted sources. |
| `.with_stall_timeout(Option<Duration>)` | Stop a download when the source sends nothing for this long (default none). The bytes are parked; the error is `Interrupted`. |
| `.with_fetch_timeout(Option<Duration>)` | Bound each miss, all attempts included, from the first request to the last byte (default none). Time queued behind another fetch of the digest does not count. |
| `.with_progress(fn(&Digest, done, Option<total>))` | Called after each piece written (and once after a resumed prefix is re-hashed). |
| `root()`, `path_for(&Digest)` | Where things are (or would be). |
| `get(&Digest) -> Option<PathBuf>` / `contains` | The committed object, if present (an existence check; verification happened on write). |
| `get_or_fetch(&Digest, fetch) -> Result<PathBuf>` | The core operation; see [fetch callbacks](#fetch-callbacks-fetched). |
| `get_or_fetch_sized(&Digest, size, fetch)` | The same for content of a known size (an OCI descriptor, `X-Linked-Size`): stopped as soon as it runs past `size`, rejected at any other size. |
| `fetch_url(&Client, url, &Digest)` | `get_or_fetch` over a plain `GET`. |
| `insert(fetch) -> Result<Stored>` / `insert_bytes(data)` | Store content whose digest is not known in advance; it is hashed on the way in and committed under the digest it has. No resume (nothing to key a partial by). |
| `verify(&Digest) -> Result<bool>` | Re-hash a committed object; a corrupted one is deleted (`false`). |
| `remove(&Digest)` | Delete an object and its parked partial. |
| `partial_len(&Digest) -> Option<u64>` | Size of a parked partial download. |
| `purge_temp(older_than)` | Delete temp and partial files not modified for `older_than` — leftovers of processes that died mid-download. Objects are never touched. A download running in this process is never removed, whatever its age; one in another process is judged by its modification time (refreshed when a partial is claimed, and by every write), so pick an age well above the longest a source may stall. |
| `usage() -> Result<u64>` | Bytes held by committed objects. |
| `evict_to(max_bytes) -> Result<Evicted>` | Delete committed objects oldest first (by modification time) until the rest fit in `max_bytes`. `Evicted { removed, freed, remaining }`. |

`Stored { digest, path }` is what `insert` (and the adapters' `download`s)
return.

### Fetch callbacks: `Fetched`

`get_or_fetch` takes `FnMut(u64) -> impl Future<Output = Result<Fetched, StoreError>>`.
The argument is the byte offset to start from (`0`, or the size of a parked
partial). It is called at most twice per miss (the second time from 0 after a
rejected resume) and never on a hit.

```rust,no_run
use ferroly::http::{Client, Method, Request};
use ferroly::store::{fetch_http, Cache, Digest};

# async fn demo(cache: Cache, digest: Digest) -> Result<(), ferroly::store::StoreError> {
let client = Client::new();
let req = Request::builder(Method::Get, "https://artifacts.example/model.bin")?
    .header("authorization", "Bearer …")
    .build();
let path = cache
    .get_or_fetch(&digest, |offset| fetch_http(&client, &req, offset))
    .await?;
# let _ = path; Ok(())
# }
```

| Constructor | Meaning |
|---|---|
| `Fetched::new(stream)` | The whole content from byte 0 (the offset was ignored). |
| `Fetched::resumed(start, stream)` | The content from byte `start`. |
| `.with_total(n)` | The total size of the whole content; a body that runs past it is rejected, one that ends short is an interruption (parked; `StoreError::Interrupted`). |
| `Fetched::from_reader(r)` / `from_bytes(v)` | Any `AsyncRead`, or bytes in memory. |
| `Fetched::from_response(resp, offset)` | Interprets an HTTP answer: `206` with a valid `Content-Range`, any other `2xx` as the whole content, `416` (when `offset > 0`) as "nothing left", anything else as `StoreError::Remote`. |
| `fetch_http(&Client, &Request, offset)` | Clones the request, adds `Range: bytes=<offset>-` (when `offset > 0`) and `Accept-Encoding: identity`, sends it with the client's decompression turned off (a body served with `Content-Encoding` is cached as served) and calls `from_response`. Redirects are followed only as far as the client's `max_redirects` says. |

A stream is anything implementing `ByteStream` (`next_chunk() -> BoxFuture<Result<Option<Vec<u8>>>>`);
`http::Response` implements it.

> **Why not `http::download_to_file`?** It is the same resume logic, but it
> takes a URL only (no auth headers, no per-attempt signing) and writes to a
> caller-chosen path. The cache needs the request under its control and a hash
> of every byte, so it reimplements the `Range` / `Content-Range` handling
> (with the same validation rules) on top of `Client` and `Response`.

## Digest

`Digest` is a SHA-256 written `sha256:<64 hex>` — the OCI form, and the
cache key. `Digest::parse` / `FromStr` accept either hex case and any case of
`sha256`; `Display` writes the canonical lower-case form. Also `from_hex`
(bare hex), `of(&[u8])`, `from_bytes`, `as_bytes`, `hex`, `ct_eq`, and
`From<hash::Digest<32>>`. Other algorithms are rejected.

## S3 adapter (`store-s3`)

```rust,no_run
use ferroly::store::s3::{Credentials, ListObjects, S3Client};

# async fn demo() -> Result<(), ferroly::store::StoreError> {
let s3 = S3Client::builder()
    .region("us-east-1")
    .endpoint("http://127.0.0.1:9000")      // MinIO; omit for AWS
    .credentials(Credentials::new("minioadmin", "minioadmin"))
    .build()?;

let meta = s3.head_object("bucket", "data/train.parquet").await?;
let mut obj = s3.get_object_range("bucket", "data/train.parquet", 0, Some(1023)).await?;
while let Some(chunk) = obj.chunk().await? { /* … */ let _ = chunk; }

let page = s3.list_objects_v2("bucket", &ListObjects::new().prefix("data/").max_keys(100)).await?;
let all = s3.list_all("bucket", &ListObjects::new().prefix("data/"), 100_000).await?;
# let _ = (meta, page, all); Ok(())
# }
```

| Method | Description |
|---|---|
| `get_object(bucket, key) -> GetObject` | Streaming body (`chunk`, `bytes`, `into_response`) plus `meta()`. |
| `get_object_range(bucket, key, start, end)` | `Range: bytes=start-end` (`end: None` for open-ended). |
| `head_object(bucket, key) -> ObjectMeta` | Length, ETag, last-modified, content type, version id, user metadata (`x-amz-meta-*`), and `x-amz-checksum-sha256` / `x-amz-checksum-type` (requested with `x-amz-checksum-mode: ENABLED`). `sha256_digest()` uses the checksum only when it is a full-object one (no `-N` suffix, and a type that is absent or `FULL_OBJECT`). A missing object is a `404` `Remote` error (`is_not_found()`). |
| `list_objects_v2(bucket, &ListObjects) -> ListPage` | One page: `objects` (key, size, etag, last-modified, storage class), `common_prefixes`, `is_truncated`, `next_continuation_token`. `ListObjects` has `prefix`, `delimiter`, `max_keys`, `continuation_token`, `start_after`. |
| `list_all(bucket, &ListObjects, max_objects)` | Follows continuation tokens; fails rather than return more than `max_objects`, and fails on a repeated token or a truncated page without one. |
| `fetch(bucket, key, offset) -> Fetched` | A freshly signed ranged `GET`, for `Cache::get_or_fetch`. |
| `download(&Cache, bucket, key, Option<&Digest>) -> Stored` | With a digest: `get_or_fetch` (resumes; a hit makes no request). Without: `HeadObject` for a full-object SHA-256 checksum, used as the key when present; otherwise the object is hashed as it is stored. The `GET` then carries `If-Match` with the `HEAD`'s ETag, so an object replaced in between fails at once with a `412` `Remote` error. A weak ETag (`W/"…"`, which a strong `If-Match` never matches) is not sent; the content is then checked by its digest only. |
| `object_url(bucket, key)` | The URL a request goes to. |

**Addressing.** Virtual-hosted (`https://bucket.s3.<region>.amazonaws.com/key`)
by default; `.path_style(true)` for `https://host/bucket/key`. Path style is
also used automatically for bucket names that are not DNS-compatible, for an
IP-address endpoint, for a dotted bucket over HTTPS (it would not match the
wildcard certificate), and for a custom endpoint whose host has no dot
(`localhost`-style). Keys are percent-encoded per segment (`/` kept); bucket
names are restricted to `A-Z a-z 0-9 . - _`.

**Endpoint override.** `.endpoint(url)` (or `AWS_ENDPOINT_URL_S3` /
`AWS_ENDPOINT_URL`) for MinIO, LocalStack, Cloudflare R2 and other
S3-compatible services. It must be an `http(s)` URL without a path.

**Redirects** are never followed for S3, whatever the `Client` passed to
`S3ClientBuilder::client` is configured to do: a signature (and the session
token sent with it) is valid for one host, so a `3xx` is a `Remote` error.
That client's response decompression is also turned off, so an object stored
with `Content-Encoding: gzip` is fetched and verified exactly as stored.

**Wrong region.** When S3 answers with `x-amz-bucket-region` (a `301
PermanentRedirect`, a `400`), the error message names that region; build a
client for it. The request is not re-sent to the other region automatically.

**Clock skew.** A signed request refused with `403` `RequestTimeTooSkewed`
by a service whose `Date` header is more than 5 minutes from local time (for a
`HEAD`, which has no error body: any `403` more than 15 minutes off) is signed
again, once, at the service's time. A `Date` more than 24 hours away is not
believed. The correction is kept by the client and its clones for later
requests, unless the re-signed request is refused too. This is on by default
for `https://` endpoints and **off for `http://` ones**, where anyone on the
path could forge the refusal and obtain a request signed for a later time;
`S3ClientBuilder::correct_clock_skew(bool)` overrides it (for a local MinIO on
a trusted network). *(Changed in 0.6.0: the correction followed any `403`'s
`Date`, over http too.)*

### SigV4 signing

`ferroly::store::s3::sigv4` is usable on its own:

```rust
use std::time::{Duration, UNIX_EPOCH};
use ferroly::http::{Method, Request};
use ferroly::store::s3::{sigv4, Credentials, PayloadHash};

let mut req = Request::builder(Method::Get, "https://examplebucket.s3.amazonaws.com/test.txt")
    .unwrap()
    .header("Range", "bytes=0-9")
    .build();
let creds = Credentials::new("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
let time = UNIX_EPOCH + Duration::from_secs(1_369_353_600); // 2013-05-24T00:00:00Z
let sig = sigv4::sign(&mut req, &creds, "us-east-1", "s3", time, &PayloadHash::empty()).unwrap();
// The documented signature of the AWS "GET Object" example:
assert_eq!(sig.signature, "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
```

| Item | Description |
|---|---|
| `sign(&mut Request, &Credentials, region, service, SystemTime, &PayloadHash) -> Signature` | Sets `x-amz-date`, `x-amz-content-sha256`, `x-amz-security-token` (temporary credentials) and `Authorization`. Signs `host` plus every header already on the request except hop-by-hop ones and `User-Agent` — add `Range` etc. **before** signing. Returns the canonical request, string to sign, signed headers, signature and header value for inspection. |
| `PayloadHash::{empty(), of(body), Unsigned}` | The payload hash; `Unsigned` is `UNSIGNED-PAYLOAD` (`S3ClientBuilder::unsigned_payload(true)` uses it for every request). |
| `signing_key(secret, date, region, service) -> [u8; 32]` | The derived key (`HMAC("AWS4"+secret, date)` → region → service → `aws4_request`). The intermediate keys are wiped; the returned one is valid for a day — wipe your copy (`sign` wipes its own). |
| `canonical_query(Option<&str>)`, `amz_datetime(SystemTime)` | The pieces. |

The canonical URI is the request path encoded once — the S3 rule
(`sigv4::canonical_path`): each segment is percent-decoded and re-encoded with
only the unreserved characters kept and upper-case hex, as S3 and
S3-compatible servers rebuild it. So a caller's URL with raw `+`, `!`, `:` or
lower-case `%2f` signs the way the server will check it, and two spellings of
one key sign the same. Other AWS services double-encode it; this signer does
not, so use it for S3. The implementation is checked against the worked examples in the
AWS documentation (GET Object with `Range`, PUT Object, GET bucket lifecycle,
ListObjects, and the IAM signing-key example) and against an independent
verifier in a loopback mock S3.

### Credentials

`Credentials::new(access_key_id, secret)`, `.with_session_token(token)`,
`Credentials::from_env()` (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
`AWS_SESSION_TOKEN`) or `from_lookup(fn)`. The secret and token are
[`Secret`](hash.md#secret)s; `Debug` prints only the first four characters of
the key id. Without credentials an `S3Client` sends unsigned requests (public
buckets).

`S3Client::from_env()` reads the region from `AWS_REGION` or
`AWS_DEFAULT_REGION` (required), the credentials as above, and the endpoint
from `AWS_ENDPOINT_URL_S3` / `AWS_ENDPOINT_URL`. `S3ClientBuilder::from_lookup`
does the same through your function.

## Hugging Face adapter (`store-hf`)

```rust,no_run
use ferroly::store::Cache;
use ferroly::store::hf::{HfClient, HfFile};

# async fn demo() -> Result<(), ferroly::store::StoreError> {
let hf = HfClient::new().with_token("hf_…");
let cache = Cache::open("/var/cache/hf")?;
let file = HfFile::model("meta-llama/Llama-3.2-1B", "model.safetensors").revision("main");
let got = hf.download(&cache, &file).await?;
println!("{} (commit {:?}) at {}", got.digest, got.commit, got.path.display());
# Ok(())
# }
```

- **Resolve.** `HfFile::model / dataset / space(repo, path)` with
  `.revision(r)` (default `main`; a branch, tag, commit id or `refs/pr/<n>`)
  maps to `<endpoint>/[datasets/|spaces/]<repo>/resolve/<revision>/<path>`.
  The revision is percent-encoded as one segment; path segments one by one.
  Repository ids are `name` or `org/name`; `.`/`..`/empty path segments are
  rejected.
- **Endpoint** `https://huggingface.co`, or `HF_ENDPOINT` / `with_endpoint`.
- **Auth.** `HF_TOKEN` / `with_token` sends `Authorization: Bearer …` to the Hub
  only. When the Hub redirects to its CDN (another origin), the header is
  dropped. An `http://` endpoint with a token is refused (`StoreError::Config`,
  before anything is sent) unless `.plain_http(true)` allows sending the token
  unencrypted.
- **Client.** `with_client(Client)` sets timeouts, proxy and TLS; that client's
  own redirect following and decompression are turned off, because the adapter
  follows redirects itself (so the `HEAD` reads the Hub's answer, with the
  commit and LFS digest, not the CDN's) and verifies the bytes as served. A
  `HEAD` answered from another origin is a `Protocol` error.
- **`file_info(&HfFile) -> HfFileInfo`** — a `HEAD` that follows same-origin
  redirects (a renamed repository) but not the CDN redirect, and reports
  `commit` (`X-Repo-Commit`), `etag` (`X-Linked-Etag`, else `ETag`), `size`
  (`X-Linked-Size`, else `Content-Length`), `location`, and `digest` — the
  SHA-256 when the ETag is one (files stored in LFS).
- **`download(&Cache, &HfFile) -> HfDownload { digest, path, commit }`** — the
  `HEAD`, then a `GET` **pinned to the reported commit** (so a branch moving in
  between cannot mix versions). An LFS file is keyed by its SHA-256 and size:
  a hit costs only the `HEAD`, and an interrupted download resumes with a
  `Range` request to the CDN. A non-LFS file (its ETag is a git object id) is
  hashed as it is stored.
- **`open(&HfFile) -> Response`** and **`fetch(&HfFile, offset) -> Fetched`** —
  streaming and cache building blocks.
- Errors carry the Hub's `X-Error-Code` (`RepoNotFound`, `EntryNotFound`,
  `GatedRepo`, …) and `X-Error-Message`.

## OCI adapter (`store-oci`)

```rust,no_run
use ferroly::store::Cache;
use ferroly::store::oci::{OciClient, Reference};

# async fn demo() -> Result<(), ferroly::store::StoreError> {
let oci = OciClient::new().with_basic_auth("user", "token");
let cache = Cache::open("/var/cache/oci")?;
let reference = Reference::parse("ghcr.io/org/weights:v1")?;

let manifest = oci.manifest(&reference).await?;        // verified against its digest
for layer in &manifest.layers {
    let path = oci.blob_to_cache(&cache, &reference, layer).await?;
    println!("{:?} {} -> {}", layer.title, layer.digest, path.display());
}
# Ok(())
# }
```

- **`Reference::parse("[registry/]repo[:tag][@sha256:…]")`** — a first
  component with a `.` or `:` (or `localhost`) is the registry; otherwise it is
  Docker Hub (`ubuntu` → `docker.io/library/ubuntu:latest`, API host
  `registry-1.docker.io`). No tag and no digest means `latest`. Repository
  names and tags are validated against the OCI grammar.
- **Auth.** A `401` with `WWW-Authenticate: Bearer realm=…,service=…,scope=…`
  is answered by fetching a token from the realm (`GET ?service=&scope=`),
  anonymously or with the basic credentials, and retrying once. Only a
  challenge from the registry's own origin is answered: a `401` from the
  storage backend a blob redirected to is a `Remote { status: 401 }` error,
  and the realm it names is never contacted (so the credentials never reach
  it). Tokens are cached per registry and repository for their `expires_in`
  (default 60 s). `with_basic_auth` gives the client a token cache of its own,
  so a clone with credentials never shares tokens with the anonymous client it
  came from.
  A `Basic` challenge is answered with the credentials. Without a usable
  challenge, or when the retry is refused, the error is `StoreError::Auth`. A
  plain-`http` realm is refused for an `https` registry.
- **`manifest(&Reference) -> Manifest`** — by tag or digest, with `Accept`
  listing the OCI image manifest and index and the Docker v2 manifest and
  manifest list. The SHA-256 of the body must equal the reference's digest
  and `Docker-Content-Digest` (when sent). `Manifest` has `media_type`,
  `digest`, `bytes` (exactly as served), `config`, `layers`, `manifests` (an
  index's entries, with `platform`) and `is_index()`. Descriptors carry
  `media_type`, `digest`, `size`, `platform` and the ORAS file name
  (`org.opencontainers.image.title`) as `title`.
- **`blob_to_cache(&Cache, &Reference, &Descriptor)`** — `get_or_fetch_sized`
  with the descriptor's digest and size. The registry's redirect to its
  storage backend is followed without the `Authorization` header; resumption
  sends the `Range` through the redirect.
- **`pull(&Cache, &Reference) -> Pulled`** — the manifest (stored too), the
  config and every layer. An index is a `Config` error: choose an entry from
  `manifest.manifests` and pull `reference.with_digest(entry.digest)`. A
  manifest that is not an image (`Manifest::is_image()`: an OCI image or Docker
  v2 schema 2 manifest with a config) — Docker schema 1, an OCI artifact
  manifest, an unknown media type — is a `Protocol` error, not an empty pull.
- **`blob`, `fetch_blob`** — streaming and cache building blocks.
- **`plain_http(true)`** talks `http://` to the registry, for a local
  development registry.
- **`with_client(Client)`** sets timeouts, proxy and TLS; that client's own
  redirect following and decompression are turned off, as for the other
  adapters.

## Errors

`StoreError` (`#[non_exhaustive]`), also wrapped by `ferroly::Error::Store`:

| Variant | Meaning |
|---|---|
| `Io(std::io::Error)` | A cache file operation failed. |
| `Http(HttpError)` | The exchange failed (connect, TLS, timeout, truncated body). |
| `InvalidDigest(String)` | Not `sha256:<64 hex>`. |
| `DigestMismatch { expected, actual }` | Content did not hash to the digest; it was deleted. |
| `SizeMismatch(String)` | Wrong size, over a cap, or a listing over `max_objects`. |
| `Interrupted(String)` | A body that ended before its declared size, or a stall / fetch deadline set on the cache. The bytes of a digest-keyed download are parked, so a retry resumes. |
| `Remote { status, code, message }` | The service answered with an error status. `code` is the S3 `<Code>`, the OCI `errors[0].code` or the Hub's `X-Error-Code`. `is_not_found()`, `status()`. |
| `Protocol(String)` | An unusable response (bad `Content-Range`, malformed or non-image manifest, malformed listing, too many redirects, an `https` → `http` redirect). Deterministic. |
| `Auth(String)` | Credentials missing or refused, or a token could not be obtained. |
| `Config(String)` | A bad argument: bucket, key, region, endpoint, reference, repository id. |

## Retries

The adapters make one attempt per call. Because an interrupted download is
parked, wrapping a call in [`clients::retry`](clients.md#retry) resumes rather
than restarts. As a `ferroly::Error`, `Interrupted` classifies as
`ErrorKind::Unavailable` (retryable), while `Protocol` is `InvalidInput` and
`DigestMismatch` / `SizeMismatch` are `Internal` (not retryable):

```rust,ignore
use ferroly::clients::{retry, RetryPolicy};
use ferroly::store::StoreError;

// Retry transport failures; never a verification failure.
let path = retry(
    &RetryPolicy::default(),
    |e: &StoreError| matches!(e, StoreError::Http(_) | StoreError::Interrupted(_)),
    || cache.fetch_url(&client, url, &digest),
)
.await?;
```

## Limits

| What | Limit |
|---|---|
| One object | `Cache::with_max_object_bytes` (default none); an expected size or a source's declared total is enforced as it streams |
| One fetch | `Cache::with_stall_timeout` / `with_fetch_timeout` (default none; HTTP sources keep their client's timeouts) |
| Error bodies read for a message | 64 KiB; messages truncated to 512 bytes |
| S3 listing page | 16 MiB, XML depth 16; keys ≤ 1024 bytes |
| `list_all` | `max_objects` entries; a repeated token or > 100 consecutive empty pages is an error |
| OCI manifest | 4 MiB, JSON depth 32, ≤ 10 000 descriptors per list |
| OCI token response | 64 KiB; ≤ 256 cached tokens |
| `WWW-Authenticate` | ≤ 8 challenges, ≤ 16 parameters each |
| Redirects (HF, OCI) | 10 hops, followed by the adapter whatever the client's own setting; S3 follows none |
| S3 clock skew | corrected once per request from a `RequestTimeTooSkewed` refusal's `Date` when 5 minutes to 24 hours off; https endpoints only unless enabled |
| Digest / reference / repository strings | bounded before parsing; never echoed back unbounded |

The HTTP client's own limits (timeouts, header sizes) apply to every request;
its in-memory body cap is lifted only for bodies written to the cache.

## Limitations and what is not included

- **S3:** no instance-metadata (IMDS), SSO, profile-file (`~/.aws`),
  web-identity or ECS credential providers — pass credentials or use the
  environment variables. No uploads (`PutObject`, multipart), presigned URLs,
  SigV4a, or path double-encoding for non-S3 services. `HeadObject` errors have
  no body, so their `code` is `NotFound` for a 404 and empty otherwise.
- **Hugging Face:** no Hub JSON API (listings, file trees, model cards), no
  uploads, no Xet storage, and not the `huggingface_hub` cache layout — files
  go into a `Cache`. `HUGGING_FACE_HUB_TOKEN` (the legacy name) is not read.
  A file not stored in LFS is not verified against anything (its ETag is a git
  object id): it is stored under the SHA-256 of what arrived.
- **OCI:** pull only. No OAuth2 `POST` token flow (the `GET` endpoint is used),
  no `~/.docker/config.json` or credential helpers, no automatic platform
  selection from an index, no signature verification (cosign, notation), no
  referrers API, no chunked/parallel blob download. Only `sha256` digests.
- **Cache:** eviction is by size budget and modification time only
  (`evict_to`: oldest committed first; a hit does not refresh it), not LRU. No
  cross-process lock (concurrent processes may download the same object twice,
  safely). A resume re-hashes the whole parked prefix. Resume survives errors
  and cancellation, not process death. No `If-Range` on resume: a source whose
  content changed surfaces as `DigestMismatch` after the download, and the
  cache then retries once from byte 0. `get` does not re-verify (`verify`
  does). Objects are plain files: anything with write access to the directory
  can change them.
- **Credentials in headers are not wiped.** Secrets are held in `Secret` and
  the SigV4 intermediate keys are wiped, but the `Authorization` values sent
  (`Basic …`, `Bearer …`, the SigV4 header) are ordinary strings once they
  are in a request: its header map, its clones and the connection's write
  buffer cannot be wiped, so these copies are out of scope.
- **One attempt per call** — compose with [`clients`](clients.md) for retries,
  circuit breaking or rate limits.

## See also

- [http](http.md) — the client, streaming `Response`, ranges and `download_to_file`.
- [hash](hash.md) — `Sha256`, `hmac_sha256`, `Secret`, base64 and hex.
- [clients](clients.md) — retry with backoff around any of these calls.
- [vfs](vfs.md) — the virtual filesystem trait (object-store backends are an extension point there).
