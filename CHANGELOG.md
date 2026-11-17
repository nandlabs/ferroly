# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
crate is pre-1.0 (`0.x`), minor releases may contain breaking changes.

## [0.7.0]

Configuration and storage release. It closes 8 issues (#105 to #112): config
fixes for single unknown keys, environment variables that name a section, and
empty sections and files; an injectable environment for tests (`merge_env_from`)
and a file-format allow-list (`formats`); a boolean dynamic flag that takes
`true`/`false` (`DynamicArity::BoolSwitch`); `TempFile::persist_with_mode` and a
directory sync after `persist`; and a synchronous, HTTP-free content-addressed
store (`store-local`) with layouts, listing, short-digest lookup, a
non-destructive check and a cross-process lock, which `store::Cache` is now
built on. No public item was removed; read "Breaking / behaviour changes" before
upgrading from 0.6.

### Breaking / behaviour changes

**Upgrading from 0.6:**

- **config**: `ConfigError::UnknownKeys` reads `unknown config key: …` when
  there is exactly one key; zero, two or more keep `unknown config keys: …`. The
  50-key cap and the `, and N more` tail are unchanged. Match on the variant,
  not the text (#105).
- **config**: with a schema, an environment variable whose path names a section
  rather than a key (`APP_MODEL` against a `model` section, `APP_A__B` against
  `a.b.c`) is no longer configuration: it is skipped without being reported, in
  strict and lenient mode alike, and with `EnvOptions::secret_files()` the file
  an `APP_MODEL_FILE` names is not read. A field whose default encodes as a
  section (a data-carrying enum variant) can therefore no longer be set to a
  unit variant from the environment; set it from a file, a value layer or
  arguments (#107).
- **config**: with a schema, a `null` where the schema has a section (a YAML
  `engine:` heading with every key commented out) is dropped instead of
  replacing the section, and an empty layer, with or without a schema, is now
  read from a file of any format (JSON and XML included) that is empty or holds
  only whitespace, from a comment-only YAML, TOML or properties file, and from a
  JSON `null`; a section holding nothing but such null sections is dropped with
  them, so it creates no empty object and records no origin. `null` for an
  `Option`, a leaf or a free-form map, and any other scalar where a section is
  expected, are merged as before; `merge_value(Value::Null)` is unchanged
  (#109).
- **config / cli**: `config::dynamic_arity` returns the new
  `DynamicArity::BoolSwitch` (was `Switch`) for boolean keys, so a positional or
  variadic value that is literally `true` or `false`, written directly after a
  boolean dynamic flag, is now taken as the flag's value. To keep the old
  behaviour, wrap the function: `move |n| arity(n).map(|a| if a ==
  DynamicArity::BoolSwitch { DynamicArity::Switch } else { a })` (#108).
- **store**: `remove`, `evict_to` and the deletion of a corrupt object in
  `Cache::verify` take the new cross-process store lock when they have something
  to delete: they then wait while another holder has it (they never blocked
  before), and fail with `StoreError::Io` (`Unsupported`) on a platform where
  `fsutils::FileLock` is unsupported. With nothing to delete (`remove` of an
  absent object, `evict_to` when the objects already fit) they return as before
  without the lock, so they also work on a read-only store root. The async
  `Cache` forms wait in short slices; if the future is dropped while waiting,
  nothing is deleted. A `.lock` file appears in the cache root after the first
  call that deletes something, and is never deleted (#112).
- **store**: `StoreError::Http` exists only with the `store` feature, and
  `ferroly::Error::Store` with `store-local` (which `store` implies), so code
  using `store` compiles unchanged. `StoreError` gains `NotFound`,
  `AmbiguousPrefix` and `Locked`; it is `#[non_exhaustive]` (#112).

### Added

- **config**: `ConfigBuilder::merge_env_from(prefix, vars)` and
  `merge_env_from_with(prefix, vars, &options)` read environment variables from
  any iterator of `Into<OsString>` pairs instead of the process environment, so
  environment handling can be tested without `std::env::set_var` and without
  racing other tests. They follow every `merge_env` / `merge_env_with` rule:
  nesting, ignored keys, sorted application, `EnvOptions`, schema screening and
  provenance. `*_FILE` secret paths are still read from disk, and when a name is
  repeated the last value in input order wins; if that last value is not valid
  UTF-8 the variable is skipped, even when an earlier value was valid, as in the
  process environment (#110).
- **config**: `ConfigBuilder::formats(&[FileFormat])` and a public `FileFormat`
  enum (`all`, `from_path`, `extensions`) restrict which file formats a builder
  loads. `merge_file`, `merge_file_optional`, `merge_profile`, `merge_found` and
  the file and profile `Layer`s refuse a known extension that is not accepted,
  before touching the filesystem; with an extension-less base, `merge_profile`
  probes only the accepted extensions. New `ConfigError::FormatNotAllowed {
  path, allowed }`, classified `Unsupported` (#111).
- **cli**: `DynamicArity::BoolSwitch`, a dynamic boolean switch that also takes
  the next token when it is `true` or `false` (ASCII case-insensitive), so
  `--name false` works as well as `--name` and `--name=false`. Any other next
  token (`1`, `on`, `file.txt`, a flag, `--`, or a subcommand name, including a
  subcommand literally named `true`/`false`) is left alone, and `--name=value`
  reads the same boolean words as `Switch` (#108).
- **fsutils**: `TempFile::persist_with_mode(dest, mode)` sets the file's exact
  permission bits (Unix; the umask is not applied, and `mode` is ignored
  elsewhere) just before the flush and rename. The draft stays `0600` while it
  is written, so a shared cache or build output no longer has to `chmod` the
  temporary path (#106).
- **store**: new `store-local` feature (in `full`): `LocalStore`, a synchronous,
  HTTP-free content-addressed store that depends only on `hash` and `fsutils`
  (no tokio, rustls or HTTP client), with `open` / `open_with`, `get` /
  `contains` / `path_for`, `insert_reader` / `insert_bytes` / `insert_verified`
  / `insert_path`, `remove`, `usage`, `evict_to`, `purge_temp` and
  `with_max_object_bytes`. `store` now implies `store-local`, and `Cache` is
  layered on it (`Cache::from_local`, `Cache::local`, `From<LocalStore> for
  Cache`); both can share one directory. `Digest`, `Stored` and `Evicted` are
  available under `store-local` (#112).
- **store**: `Layout::{Fanout, Flat}` with `Layout::flat(prefix)` and
  `Cache::open_with` / `LocalStore::open_with`: objects go in `sha256/<2
  hex>/<hex>` (the default, unchanged) or `<prefix>/sha256/<hex>` (for example
  `blobs/sha256/<hex>`). Bad prefixes (empty, absolute, `..`, the store's own
  `tmp` / `partial` / `.lock`, or a component ending in `.` or a space) are
  refused with `StoreError::Config`. There is no migration between layouts
  (#112).
- **store**: `list()` (sorted `ObjectInfo { digest, path, size, modified }`;
  foreign entries are skipped), `resolve_prefix()` / `resolve_prefix_min()` for
  short digests (`MIN_PREFIX_HEX` = 12; like `list`, they ignore stray files and
  symlinks), and `check()` / `check_all()` returning `ObjectStatus::{Intact,
  Missing, Corrupt}`, which never modify the store; on `LocalStore` and, async,
  on `Cache` (#112).
- **store**: `StoreLock`, a cross-process store lock on `<root>/.lock` built on
  `fsutils::FileLock`, taken with `LocalStore::lock` / `try_lock` /
  `lock_timeout` or `Cache::lock().await`; its `remove`, `evict_to` and `repair`
  run under the lock already held. New `StoreError` variants `NotFound(String)`
  (`is_not_found()` is true; classified `NotFound`), `AmbiguousPrefix { prefix,
  candidates, matches }` (`InvalidInput`) and `Locked(String)` (`Conflict`), and
  `From<fsutils::FsError> for StoreError` (#112).

### Changed

- **ci**: publishing to crates.io is manual for now. `tag-on-merge` still tags
  each version merged to `main` but no longer starts `publish.yml`, which now
  runs only when a maintainer dispatches it; the `crates.io drift` check runs by
  hand and after a publish, with no daily schedule. CONTRIBUTING.md ("Publishing
  to crates.io") has the by-hand steps (#104).
- **ci**: `taiki-e/install-action` `v2.88.1` → `v2.90.3`, pinned by commit, in
  the coverage and fuzz workflows (Dependabot PR #103).
- **config**: `merge_env` and `merge_env_with` are thin wrappers over
  `merge_env_from` / `merge_env_from_with` with `std::env::vars_os()`; their
  behaviour is unchanged (#110).
- **fsutils**: `TempFile::persist` (and `persist_with_mode`) flushes the
  destination's directory after the rename on Unix, so the rename itself
  survives a crash, as `AtomicFile::commit` already did and with the same
  helper. It is best effort: errors are ignored, since the file is already in
  place (#106).
- **store**: `Cache::remove`, `verify`, `usage`, `evict_to` and `purge_temp`
  each run as one blocking-thread call on the `LocalStore` core instead of a
  series of `tokio::fs` calls when the store lock is free (while it is held
  elsewhere, `remove`, `evict_to` and `verify` retry in 50 ms slices, and delete
  nothing if cancelled); signatures and documented results are unchanged, and
  `verify` checks again under the lock before deleting (#112).

### Fixed

- **config**: `SharedConfig::watch_files` waits for a change that leaves a watched
  file empty or missing to stay that way for at least one second (and five polls)
  before reloading, so a non-atomic rewrite caught between its truncation and its
  write is not loaded as an empty layer now that an empty file is one (#109).
- **config**: an environment variable named like a schema section
  (`APP_MODEL=x`) no longer replaces the whole section and erases its defaults,
  and with `EnvOptions::secret_files()` the file an `APP_MODEL_FILE` names is no
  longer read; `APP_MODEL__PATH` still sets `model.path` (#107).
- **config**: a section left empty in a config file, at any depth, no longer
  erases the lower layers' values for that section; it is never reported as an
  unknown key and records no provenance. An empty or whitespace-only file of any
  format (a zero-byte `app.json` or `app.xml` was a `ConfigError::Parse`), or a
  comment-only YAML, TOML or properties file, no longer erases the whole
  configuration or fails through `merge_file`, `merge_file_optional` or
  `merge_profile`; a parent section left empty only by null sections no longer
  leaves an empty object with a file origin, and a layer that screens to nothing
  records no phantom `""` entry in `Config::sources()` (#109).

## [0.6.0]

New-capability and hardening release. It closes 21 issues: HTTP/2 and gRPC,
Protocol Buffers with `.proto` code generation, verify-only Ed25519 (`sig`), a
content-addressed artifact cache with S3, Hugging Face and OCI adapters
(`store`), a sandboxed Jinja2-subset template engine (`template`), JSON-Schema
`pattern`, a bridge from the `log` facade crate, configuration layers,
unknown-key checks, config file discovery and secret references, typed
argument merging and dynamic cli flags, cli occurrences and indices, decode
errors that name the failing field, shortest `f32` output, a plain-text span
exporter, real memory mapping on Windows, byte sizes, and an opt-out `ring`
crypto backend. It also automates publishing to crates.io (#90), and fixes
every finding of two deep reviews of the crate, many of them security fixes
(see "Security"). No public item was removed, but several behaviours and a few
types changed: **read "Breaking / behaviour changes" before upgrading from
0.5.**

### Breaking / behaviour changes

**Upgrading from 0.5:**

- **auth**: HMAC secrets must meet the RFC 7518 per-algorithm minimum: 32 bytes
  for HS256, 48 for HS384 and 64 for HS512 (it was 16 for all three).
  `MIN_SECRET_LEN` is now 32 and `Algorithm::min_secret_len()` gives each
  minimum; a shorter secret is `JwtError::WeakSecret`. Rotate short secrets;
  until then only the deprecated legacy functions accept them.
- **auth**: the `auth` feature now implies `sig` (EdDSA JWTs verify through
  `ferroly::sig`).
- **auth / turbo**: `auth::encode_hs256` / `decode_hs256`, `turbo::Router::jwt_auth`
  and `JwtAuth::legacy_hs256` are deprecated in favour of the validating API
  (`encode_hmac`, `decode_hs256_with`, `JwtAuth::hs256(secret, Validation::new())`,
  `Router::jwt_auth_with`). `encode_hs256` with an empty secret now returns an
  empty string (no token).
- **auth**: `RemoteJwks` refuses a non-`https://` key-set or issuer URL with
  `JwtError::KeyFetch`, sending nothing, unless `allow_insecure_http(true)` is
  set; it used to fetch `http://` URLs silently. A failed first fetch is
  retried at most once per `min_refresh_interval`, and a key set older than
  `max_stale` is no longer served.
- **turbo**: a token that cannot be checked (`JwtError::KeyFetch`,
  `NoCryptoProvider`) is answered `503 authentication unavailable` instead of
  `401`. A request with more than one `Authorization` header (or API-key
  header) is answered `401`, even by an optional authenticator.
- **turbo / rest**: `Router::serve*` validates the router before binding. A
  malformed pattern, conflicting routes or an unusable middleware configuration
  now stops start-up with `HttpError::Io` (`InvalidInput`) wrapping the
  `RouteError`, where the first route used to win silently.
  `rest::ServerBuilder::build` validates too; `ServerBuilder::build_lenient` and
  `Router::into_handler` keep the lenient behaviour.
- **codec**: a decode error below the root is wrapped in
  `CodecError::At { path, source }`, so code that matched a nested error's
  variant should match `err.root_cause()`. `Display` gains the path
  (`messages[1].role: expected string, found number`).
- **codec**: a value of the wrong type is the new
  `CodecError::TypeMismatch { expected, found }` on both decode paths, where it
  was `Expected("string")`, `Expected("integer")` and so on; `Expected` remains
  for a value of the right type that does not fit (`1.9` for an integer).
  Malformed JSON is always `Parse`, also on the streaming path. See the
  migration notes in `docs/codec.md`.
- **codec**: an `f32` encodes as the shortest text that reads back as the same
  `f32`, in JSON, YAML, TOML and XML (`0.1`, not `0.10000000149011612`), and
  `Value::from(f32)` holds the `f64` nearest that text. `f64` output is
  unchanged.
- **codec**: a JSON integer literal wider than 64 bits decodes into `i128` /
  `u128` exactly or fails. On the `Value` path, where such a literal is already
  a float, a float at or beyond 2^53 is rejected instead of decoding as a
  rounded number.
- **codec / turbo**: schema validation stops after `DEFAULT_MAX_VIOLATIONS`
  (100; `Schema::with_max_violations`) and appends a truncation marker
  (`Violation::is_truncation`); turbo's `422` problem lists at most that many
  and adds `"truncated": true`. Violation paths escape property names as JSON
  Pointer tokens.
- **hash**: `HashAlgorithm` gains a provided `wipe_output` method (a no-op by
  default) that `Hmac` and the KDFs use to wipe intermediate digests. A hash
  implemented outside the crate compiles unchanged, but should override it.
- **hash / http**: a PEM body accepts `=` only as final padding; it used to be
  dropped wherever it appeared.
- **genai**: `GenAiError::Api` is a `#[non_exhaustive]` variant with new
  `error_type` and `retry_after` fields. Match it with
  `GenAiError::Api { status, .. }` and build it with
  `GenAiError::api(status, message)` plus `with_error_type` /
  `with_retry_after`.
- **genai**: a transport failure after the connection was established
  (`GenAiError::Transport`) is no longer retried, and the router treats it like
  a response timeout (no fail-over unless `fallback_on_timeout`), so a billed
  completion is never repeated. A failure before any byte was sent is the new
  `GenAiError::Connect` and stays retryable; the decision uses
  `HttpError::is_connect_failure`, not the error's text.
- **genai**: an `Image` part with a non-image MIME type, and a non-image
  `FileRef` sent to OpenAI (previously sent as text), are refused with
  `GenAiError::Config` before anything is sent. An empty `2xx` stream is a
  `ResponseParse` failure. The OpenAI catalog follows the 2026-11-15 model
  list, which can change the model the router picks.
- **ws**: `WsServerConfig::tls` is now `Option<ReloadableTlsConfig>` (it was
  `Option<TlsConfig>`), read once per accepted connection so `reload()` rotates
  the certificate. The `tls(..)` builder accepts either type; code that assigns
  the field directly converts with `.into()`.
- **config**: `Config::extract` reports a decode error inside a field as
  `ConfigError::InvalidValue { key, source }`, with the full key path and the
  root cause; `Config::get` joins the section path and the key. Errors about the
  root stay `ConfigError::Codec`.
- **config**: a key of more than 128 segments, from args, cli flags,
  environment variables, secret directories or `.properties` files, is ignored
  like a key with an empty segment.
- **lifecycle**: `RestartPolicy` is `#[non_exhaustive]`; build it with
  `RestartPolicy::new` and `with_backoff` / `reset_after`.
- **clients**: `RetryDecision::RetryAfter(d)` and `ErrorClass::retry_after(d)` wait
  `max(d, planned backoff)` instead of exactly `d`, so `Retry-After: 0` or a date
  in the past keeps the jittered backoff instead of firing the remaining retries
  at once.
- **vectorstore**: `MemoryStore`'s async `search` scans a namespace of at least
  `offload_threshold` (documents x dimension, default
  `DEFAULT_OFFLOAD_THRESHOLD` = 2^20) on a shared worker pool. At most 256
  offloaded searches may be queued or running; beyond that, `search` fails at
  once with `VectorStoreError::Backend` ("the search pool is saturated").
- **errutils**: module errors classify themselves, and several kinds change.
  `JwtError` is no longer always `Unauthenticated` (`KeyFetch` is `Unavailable`,
  `WeakSecret` `InvalidInput`, `InvalidKey` `Internal`, `NoCryptoProvider`
  `Unsupported`); `WsError` is no longer always `Unavailable` (a timeout is
  `Timeout`, a rejected upgrade takes its HTTP status's kind, `Protocol` is
  `Internal`); an `HttpError` connect timeout is `Timeout` and a handler panic
  `Internal`.
- **obs**: the OTLP exporter no longer retries TLS failures, and honours a
  `Retry-After` given as an HTTP date.
- **Struct literals**: `http::ServerConfig` gains `http2` and `proxy_protocol`,
  `genai::router::Task` gains `tenant`, and `RoutingConfig` gains `tenants`. A
  struct literal without `..Default::default()` no longer compiles.
- **http**: ferroly no longer selects a rustls crypto backend for its
  consumers. With `tls-ring` on (the default), a provider the application
  installs as the process default now takes precedence over `ring`, and
  `default-features = false` also turns `tls-ring` off: add it, or install a
  provider, if you use HTTPS.
- **http**: the default client builds its TLS configuration at the first
  `https` handshake, not in `build()`; with no crypto provider, TLS setup
  returns `HttpError::Tls` instead of panicking.
- **http**: the client keeps an HTTP/1.1 chunked trailer section (previously
  discarded) and reports it through `Response::trailers`.
- **codec**: `schema::validate` now enforces `pattern` (previously ignored) and
  reports an invalid pattern as a `schema error:` violation. Patterns follow the
  strict ECMA-262 u-flag rules, so an unescaped `}` or `\-` outside a class is
  an error.
- **New variants**: `ferroly::Error` gains `Sig`, `Store`, `Proto`, `Grpc` and
  `Template`; `JwtError` gains `NoCryptoProvider` and `Replayed`; `GenAiError`
  gains `Connect`; `Algorithm` and `KeyType` gain the new JWT algorithms and key
  types. All these enums are `#[non_exhaustive]`.

**Within the features new in 0.6** (changed since the first 0.6 preview in
PR #102, or defaults worth knowing):

- **config**: `${file:}` / `${env:}` interpolation is refused in values from
  the environment, arguments or cli unless trusted with `interpolate_with`;
  with a schema, cli arguments outside it are dropped, not reported.
- **http2**: `max_keep_alive_requests` no longer caps HTTP/2 connections; use
  `Http2Config::max_streams_per_connection`, which closes with the two-phase
  GOAWAY of RFC 9113 §6.8.
- **http2**: an HTTP/2 failure is `HttpError::Http2(Http2Error)` with a
  structured `Http2Cause`, not `HttpError::Protocol(String)`.
- **http**: over HTTP/1.1, a handler that returns `true` from
  `wants_body_stream` runs as soon as the request head arrives and reads the
  body as it is sent. A failed or abandoned body closes the connection after
  the response.
- **store**: a Hugging Face token is never sent to an `http://` endpoint unless
  `HfClient::plain_http(true)` is set.
- **store**: `StoreError::Protocol` is classified `InvalidInput`; a body that
  ends short is the new `StoreError::Interrupted` (`Unavailable`, retryable).
- **store**: the S3, HF and OCI adapters turn off the caller's client redirect
  following and decompression and apply their own rules (S3 follows no
  redirect); OCI `pull` requires an image manifest (`Manifest::is_image`); S3
  corrects clock skew only after `403 RequestTimeTooSkewed`, at most 24 hours,
  and by default only over `https://` (`S3ClientBuilder::correct_clock_skew`).
- **grpc**: generated code moves into `<service>_server` (trait and adapter)
  and `<service>_client` submodules. `GrpcServer::serve` uses
  `GrpcServer::server_config()` (no stall timeout, no per-connection stream
  cap, a 10 ms rapid-reset threshold). `Channel::from_client` returns a
  `ChannelBuilder` and requires a client with HTTP/2 enabled. A response
  without a gRPC content type is an error.
- **proto**: decoding charges `Limits::max_decoded_bytes` (default 256 MiB) for
  the memory it creates; generated struct fields follow field-number order;
  Rust name clashes in generated code are an `IdlError`; the parser rejects a
  proto3 field of a proto2 enum, a non-bool `packed`, enum values clashing in
  one scope, `allow_alias` without aliases and octal escapes above `\377`.
- **sig**: `SigError::MalformedSignature` is classified `Unauthenticated`, like
  `VerificationFailed`.
- **vectorstore**: the new `load_file` / `load_dir` loaders are bounded by
  default `LoadOptions`: 16 MiB per file; 256 MiB, 10 000 files and depth 32
  per directory load. They read only regular files.
- **genai**: in routing configuration, a present `allowed_providers` or
  `allowed_models` list, even `[]`, is an allow-list (`[]` allows nothing).

### Added

- **http2**: new `http2` feature: hand-rolled HTTP/2 (RFC 9113) for the `http`
  server and client: ALPN `h2` over TLS, h2c with prior knowledge, HPACK
  (RFC 7541), connection and stream flow control, multiplexing,
  SETTINGS / PING / GOAWAY / RST_STREAM, and a graceful GOAWAY on drain. Existing
  handlers, `turbo` routers and `rest` servers serve HTTP/2 unchanged (#7).
- **http2**: defences against rapid reset (CVE-2023-44487), MadeYouReset,
  CONTINUATION flood (CVE-2024-27316), HPACK bombs and SETTINGS / PING /
  PRIORITY / empty-DATA floods, tunable through `Http2Config`.
- **http2**: `ServerConfig::http2`, `ClientBuilder::http2` /
  `http2_prior_knowledge` / `http2_config`; the client multiplexes over a
  connection per origin, opens more (up to `max_connections_per_origin`) when
  one is saturated, and falls back to HTTP/1.1 when ALPN selects it.
- **http2**: new `Http2Config` limits: `rapid_reset_threshold`,
  `max_cancels_per_window` (client resets of incomplete streams of any age),
  `max_buffered_body_bytes` (a per-connection body budget),
  `max_streams_per_connection`, `stream_stall_timeout`, `keepalive_interval` /
  `keepalive_timeout` (PINGs, plus a SETTINGS ACK timeout) and
  `max_connections_per_origin`.
- **http2**: structured errors: `HttpError::Http2(Http2Error)` with
  `Http2Cause` (`Reset`, `Refused`, `GoAway`, `ConnectionClosed`, `Io`,
  `Timeout`, `LocalLimit`, `LocalReset`, `Protocol`), `Http2ErrorCode` and
  `HttpError::http2_cause()`. The server passes all 146 h2spec 2.6.0 cases.
- **http**: trailers: `HttpResponse::trailer` / `with_trailers` / `trailers`,
  `Response::trailers` / `bytes_with_trailers` (HTTP/2 trailing HEADERS and the
  HTTP/1.1 chunked trailer section).
- **http**: streamed bodies in both directions: `body_channel`, `BodySender`,
  `BodyStream`, `HttpResponse::body_stream`, `HttpHandler::wants_body_stream` with
  `Request::take_body_stream`, and `Client::send_body_stream`. Over HTTP/1.1 a
  streaming handler receives `Content-Length` and chunked bodies piece by piece
  under the existing caps, chunked request trailers included.
- **http**: `Protocol`, `Request::protocol` and `Response::protocol`;
  `BodySender::closed()`, so a producer notices that its reader went away.
- **http**: PROXY protocol v1 and v2 on the server listener:
  `ServerConfig::proxy_protocol` with a `ProxyProtocol` allow-list of trusted
  networks (`new`, `trust`, `trust_ip`, `from_cidrs`). The header's source and
  destination become `Request::peer_addr` / `local_addr`; untrusted sources are
  served directly and never parsed.
- **http**: `ClientBuilder::pool_max_active_per_host` caps the HTTP/1.1
  connections to one host, queueing requests in order (default unlimited);
  `Client::with_max_redirects` and `Client::with_decompress`, per-client
  overrides that keep the pool; `HttpError::is_connect_failure`, true only when
  no request byte was sent.
- **http**: `tls-ring` feature (on by default, in `full`) supplying `ring` as the
  rustls crypto backend (#58).
- **grpc**: new `grpc` feature and `ferroly::grpc` module: gRPC over the
  in-house HTTP/2 stack with `proto` messages. `GrpcServer` is an `HttpHandler`
  served by the `http` server over h2c or TLS + ALPN, with unary,
  server-streaming, client-streaming and bidirectional calls (#9).
- **grpc**: `Channel` client with per-call and default deadlines (sent as
  `grpc-timeout` and enforced locally), cancellation by dropping, multiplexed
  connections, and no silent fall back to HTTP/1.1.
- **grpc**: `Status` with the 17 standard codes, percent-encoded `grpc-message`,
  `grpc-status-details-bin` and the spec's HTTP-to-gRPC mapping; `Metadata` with
  ASCII and binary (`-bin`) keys and reserved-key filtering; typed
  `Streaming` / `StreamSender` streams; 4 MiB message limits checked before
  allocation.
- **grpc**: `codegen::GrpcGenerator` for `proto::codegen::Config::service_generator`,
  emitting a server trait, a `…Server` adapter (which accepts an
  `Arc<dyn Trait>`) and a typed `…Client` per service.
- **grpc**: the standard health service `grpc.health.v1.Health` (Check and
  Watch, `HealthReporter::shutdown`, run by `serve` on shutdown), and
  `GrpcServer::fallback` to serve gRPC and plain HTTP (for example a `turbo`
  router) on one port.
- **grpc**: transparent retry of refused calls (`REFUSED_STREAM`, beyond a
  GOAWAY's last stream id): once at once, then once after a random 25 to 75 ms
  pause, with the remaining `grpc-timeout`; unary and server-streaming requests
  are replayable, and streamed uploads keep up to 256 KiB for replay.
- **grpc**: the `Interceptor` trait, on `ChannelBuilder` and `GrpcServer`; request
  deadlines (`with_deadline`, `set_deadline`, `inherit_deadline`); `gzip` /
  `deflate` messages decoded on both sides; a per-message decoded-memory budget
  (`max_decoded_bytes` on the server and `ChannelBuilder`);
  `GrpcServer::server_config` and `require_te_trailers`;
  `Streaming::next` / `collect` / `poll_message` (cancel-safe) and a lazy
  `from_messages`.
- **sig**: new `sig` feature and `ferroly::sig` module: verify-only Ed25519
  (RFC 8032), hand-rolled on `hash`'s SHA-512: `Ed25519PublicKey::{from_bytes,
  from_slice, verify, verify_ctx, verify_ph, verify_prehashed}`,
  `Ed25519Signature`, and the fail-closed `verify_ed25519(key, msg, sig) -> bool`
  (#6).
- **sig**: streaming verifiers `Ed25519Verifier` (pure Ed25519, signature known
  up front) and `Ed25519phVerifier` (Ed25519ph with an optional context).
- **sig**: strict verification: `S >= L`, non-canonical or off-curve `A` / `R`
  and small-order `A` / `R` are rejected, with the cofactorless equation;
  `SigError` (`MalformedKey`, `MalformedSignature`, `InvalidContext`,
  `VerificationFailed`) is `#[non_exhaustive]`. Tested against every RFC 8032
  Ed25519 / Ed25519ctx / Ed25519ph vector, Wycheproof and the 12 speccheck edge
  cases.
- **sig**: keys from SPKI DER and PEM (RFC 8410) and OpenSSH lines (RFC 8709):
  `Ed25519PublicKey::from_spki_der` / `to_spki_der`, `from_pem` and
  `from_openssh`.
- **store**: new `store` feature and `ferroly::store::Cache`: a SHA-256-keyed
  artifact cache that verifies before committing (atomic rename), resumes
  interrupted downloads with range requests, is a no-op on a hit, deletes and
  reports mismatched content, and is safe for concurrent fetches of the same
  digest within and across processes. Also `Digest` (`sha256:<hex>`), `Fetched`
  / `ByteStream` / `fetch_http`, `insert`, `verify` and `purge_temp` (#8).
- **store**: `Cache::with_stall_timeout`, `with_fetch_timeout`, `with_progress`,
  `usage` and `evict_to` (`Evicted`); the directory is fsynced after a commit.
- **store**: `store-s3`: an S3-compatible client signed with AWS Signature
  Version 4 (checked against the AWS documentation examples): GetObject
  (streaming, ranges), HeadObject and ListObjectsV2 with continuation tokens;
  virtual-hosted and path-style addressing and endpoint override (MinIO,
  LocalStack, R2); static, session-token or environment credentials, redacted in
  `Debug`; `download` into the cache; `If-Match` between HEAD and GET;
  `ObjectMeta::checksum_type`; `sigv4::canonical_path`.
- **store**: `store-hf`: Hugging Face Hub files (models, datasets, spaces):
  revision resolution, `HF_ENDPOINT` / `HF_TOKEN`, a token never sent to the CDN,
  LFS files keyed by `X-Linked-Etag`, downloads pinned to `X-Repo-Commit`.
- **store**: `store-oci`: OCI Distribution v2 pull: the `WWW-Authenticate`
  Bearer token flow (anonymous or basic credentials) with token caching,
  manifests by tag or digest verified against `Docker-Content-Digest`, blobs
  verified into the cache through registry redirects, `Reference` parsing with
  Docker Hub defaults.
- **proto**: new `protobuf` feature and `ferroly::proto` module: the Protocol
  Buffers wire format (varint, zig-zag, fixed, length-delimited, packed and
  unpacked repeated, maps, oneof, open enums, proto3 implicit and explicit
  presence, proto2 optional / required) with unknown fields preserved across a
  decode and re-encode (#9).
- **proto**: `#[derive(ferroly::proto::Message)]`, `Oneof` and `Enumeration`,
  with compile errors for duplicate, zero or reserved field numbers and for
  discriminants outside `i32`.
- **proto**: bounded decoding through `proto::Limits` (depth, input size, field
  count, and `max_decoded_bytes`, the in-memory size of what decoding creates,
  charged before allocating); `Message::decode_length_delimited_with`.
- **proto**: `proto::idl`, a `.proto` parser (proto3 and the common proto2
  subset) with import resolution through `FsLoader` / `MemoryLoader` / closures,
  built-in well-known types, and errors with line and column; `proto::codegen`,
  a Rust code generator for `build.rs` (no `protoc`) that emits a
  `ServiceDescriptor` per service and has a `ServiceGenerator` hook (with
  `reserved_names`). Generated code builds under `#![deny(missing_docs)]`,
  rustdoc and clippy, and `compile_to_file` prints `cargo:rerun-if-changed` for
  every file it read (`FsLoader::locate`).
- **derive**: `ProtoMessage`, `ProtoOneof` and `ProtoEnumeration` derives
  (re-exported as `ferroly::proto::{Message, Oneof, Enumeration}`), and
  `JsonSchema`.
- **template**: new `template` feature (in `full`, needs only `codec`) and
  `ferroly::template` module: a sandboxed Jinja2-subset engine built to run LLM
  chat templates. `Template::parse(source, &Options)` once into an immutable,
  `Send + Sync` `Template`, then `render(&Value)` concurrently (#91).
- **template**: output, `if` / `elif` / `else`, `for` (filters, `else`, tuple
  unpacking, `break` / `continue`, `loop.*`), `set` (names, tuples, namespace
  attributes, blocks), macros, `raw`, comments, whitespace control with
  `trim_blocks` / `lstrip_blocks` on by default, Jinja2 operator precedence,
  lenient undefined values, the usual tests and filters, string, map and list
  methods, `range`, `namespace`, `dict`, `raise_exception`, and host functions
  (`Options::function`). `tojson` follows the Hugging Face convention.
- **template**: syntax errors, unknown filters, tests, functions and
  statements, and unsupported constructs (`include`, `import`, `extends`,
  `block`, `call`, …) fail at parse time with a `TemplateError`
  (`TemplateErrorKind`, byte offset, line and column); `Limits` cap source size,
  nesting depth, iterations, calls, output and allocated bytes.
  `ferroly::Error::Template`. Nine chat templates render byte for byte as
  reference Jinja2 3.1.4 does.
- **codec**: the JSON-Schema `pattern` keyword, with a built-in linear-time
  regex engine (Thompson NFA / Pike VM, no backtracking, so no ReDoS) for the
  interoperable ECMA-262 subset under u-flag rules. Unsupported syntax
  (backreferences, lookaround, flags, `\p{}`, unknown escapes) is a schema error;
  pattern length, nesting depth and compiled size are capped (#57).
- **codec**: `schema::Schema::compile` / `Schema::validate` compile a schema,
  every `pattern` included, once for repeated validation;
  `schema::SchemaError` names the offending keyword by JSON Pointer.
- **codec**: `#[derive(JsonSchema)]` and the `schema::JsonSchema` trait (with
  std impls) describe a type's encoded form from the same derive model as
  `Encode` / `Decode` (renames, optional and default fields, skip, flatten,
  `deny_unknown_fields`, all three enum representations); `Schema::of::<T>()`
  compiles it.
- **codec**: decode errors name the failing field: `CodecError::At` with
  `PathSegment` (`Key` / `Index`), and the helpers `in_field`, `in_index`,
  `path`, `path_string`, `root_cause` and `type_mismatch`. The derive adds the
  key the input used (after renames, or the matching alias), tuple indexes and
  externally tagged variant keys; std collections add the element index or
  entry key. A successful decode allocates nothing extra (#92).
- **codec**: shortest round-trip `f32` encoding, through the new defaulted
  `Encoder::encode_f32` (#97).
- **codec**: `codec::DateTime`, an RFC 3339 instant with its UTC offset
  (nanosecond precision, years 0000 to 9999, calendar components), and
  `SystemTime` `Encode` / `Decode` (RFC 3339 UTC out; RFC 3339, a
  `{secs, nanos}` object or epoch seconds in).
- **codec**: `Limits::reject_duplicate_keys` (`with_reject_duplicate_keys`)
  makes a repeated JSON or YAML object key a parse error with its position.
- **codec**: exact `i128` / `u128` decoding of wide JSON literals (new defaulted
  `Decoder::decode_i128` / `decode_u128`); `xml::encode_pretty`, an indented
  writer; `Format::decode_value`.
- **codec**: runnable encode / decode examples in the `toml`, `json`, `yaml` and
  `xml` module docs (#20).
- **turbo**: `Ctx::validate_with` / `Ctx::read_valid_with` validate request
  bodies against a compiled `Schema`; `read_valid*` decode from the validated
  `Value` instead of parsing the body twice.
- **turbo**: API-key authentication: `ApiKeyAuth` reads `X-API-Key` (or a
  configured header, optionally a bearer token), looks up the key's SHA-256
  digest in an `ApiKeyStore`, compares in constant time and checks scopes
  (`401` / `403` / `503`). `ApiKey` (`generate` from the OS CSPRNG),
  `MemoryApiKeyStore`, `Router::api_key` / `Group::api_key`,
  `require_api_key_scopes` and `Ctx::api_key`, behind `hash`.
- **turbo**: `Router::etag` (also on `Group` and `Route`) tags `200` GET / HEAD
  responses with a strong or weak digest of the body, keeps a handler-set
  `ETag`, and answers a matching `If-None-Match` with a bodiless `304`.
- **auth**: JWT algorithms `RS384`, `RS512`, `PS256` / `PS384` / `PS512`, `ES384`,
  `ES512` and `EdDSA` (Ed25519 JWKs, RFC 8037, through `ferroly::sig`);
  `Jwk::algorithms` / `accepts` / `verify_signature_with`,
  `from_ec_coordinates`, `from_ed25519_bytes`, `with_algorithm`,
  `From<Ed25519PublicKey>`; `KeyType::{EcP384, EcP521, Ed25519}`.
- **auth**: OIDC discovery with `RemoteJwks::from_issuer` (exact issuer match,
  no https downgrade, discovered issuer required on tokens), `discover()` /
  `issuer()`, `allow_insecure_http` and `max_stale`.
- **auth**: `jti` replay protection: `Validation::replay_cache` with the
  `ReplayCache` trait and a bounded `MemoryReplayCache` (`fail_closed`);
  `JwtError::Replayed`.
- **auth**: `JwtError::NoCryptoProvider`; `Algorithm::min_secret_len` and
  `recommended_secret_len`.
- **config**: `Layer`, a `#[non_exhaustive]` declarative list of configuration
  sources (`Value`, `File`, `OptionalFile`, `Profile`, `Env`, `EnvWith`, `Args`,
  `ArgsWith`, `SecretDir`, `OptionalSecretDir`, and `Cli` / `CliWith` /
  `CliDefaults` with `cli`);
  `ConfigBuilder::layers` and `Config::from_layers` apply it in order, with the
  same provenance and errors as the matching `merge_*` call (#55).
- **config**: unknown keys: `ConfigBuilder::schema`, `lenient` and
  `build_checked` check each layer against a schema value as it is merged; an
  `UnknownKey` names its source, its own spelling (dotted, `APP_X__Y`,
  `--x.y`, file path) and a suggestion; `Config::ignored_keys` lists them in
  lenient mode (#93).
- **config**: file discovery: `config::search` (`user_config_dir`,
  `default_dirs`, `find_first`, `macos_native`, `Platform`, and injectable
  `_from` / `_for` forms) and `ConfigBuilder::merge_found`; `Source::is_file`
  (#95).
- **config**: secret references: `SecretRef` (feature `hash`) decodes
  `env:NAME` / `file:/path`, refuses literals without showing more than two
  characters, and resolves to a `hash::Secret`; `Config::redact_literals`;
  `${env:NAME}` and `${file:/path}` interpolation, with file values always
  masked (#96).
- **config**: typed argument merging: `ArgsOptions` (`types(&schema)`,
  `value_follows()`), `ConfigBuilder::merge_args_with`, `merge_cli_with` and
  `dynamic_arity`; `merge_cli` also merges the cli's dynamic flags (#94).
- **config**: mounted secret directories (Kubernetes and Docker secret volumes):
  `merge_secret_dir` / `merge_secret_dir_optional` and `Layer::secret_dir` /
  `optional_secret_dir`, values always masked (`Source::SecretDir`);
  `SharedConfig::watch_files` (`ConfigWatcher`) and `SharedConfig::reload_on`
  (with `lifecycle`) for hot reload.
- **cli**: `Matches::occurrences()` (every flag in command-line order),
  `indices_of(name)` and `subcommand_index()` (#98).
- **cli**: dynamic flags: `Command::dynamic_flags(predicate)` and
  `dynamic_flags_with` with `DynamicArity` (`Switch`, `Value`, `Optional`), read
  back with `Matches::dynamic()` / `dynamic_indices()` (#94).
- **cli**: fish and PowerShell completion scripts (`Shell::Fish`,
  `Shell::PowerShell`).
- **log**: `log-bridge` feature with `ferroly::log::bridge::{install,
  install_global, is_installed, BridgeError, MAX_KV_FIELDS}`: records from the
  `log` facade crate flow into a ferroly `Logger` with level, target and
  per-target filtering, `module`, `caller` and typed key-value fields; the
  facade's max level follows ferroly's filters (#56). `bridge::stats()` returns
  `BridgeStats { forwarded, reentrant, truncated }`.
- **obs**: `obs::PlainExporter`, a plain-text span exporter: one line per
  finished span and an indented line per event, with `with_ids`,
  `with_parent`, `min_duration`, `time_precision`, `utc_offset` and, with
  `log`, `redact`; spans from many threads never interleave (#99).
- **obs**: `OtlpBuilder::tls`, `proxy`, `http_client` and `protocol` with
  `OtlpProtocol::{HttpJson, HttpProtobuf, Grpc}` (protobuf with `protobuf`,
  gRPC with `grpc`).
- **obs**: `obs::TraceInterceptor`, a client span per request of an
  `http::Client` with `traceparent` injection; open spans are bounded, and an
  evicted span carries `EVICTED_ATTRIBUTE` (`evicted_spans()`).
- **messaging**: `messaging::Traced<P>` (with `obs`): producer and consumer
  spans around any `Producer`, `Receiver` or `Provider`, with the `Ack` recorded
  as `messaging.ack`.
- **ws**: `ws::MetricsObserver` (with `metrics`): connections, messages, bytes,
  closes, protocol errors, timeouts, rejected handshakes and full queues, with
  bounded labels; wss certificate rotation through `ReloadableTlsConfig`.
- **metrics**: the process collector reports CPU time, resident and virtual
  memory and open descriptors on macOS and Windows, threads and the descriptor
  limit on macOS, and the real start time on all three;
  `Registry::register_collector_while` and `collector_count`.
- **clients**: `CircuitBreaker::export_metrics` and
  `CircuitBreakerRegistry::export_metrics` (with `metrics`) mirror breaker
  state into `circuit_breaker_*` series, holding only weak references.
- **lifecycle**: `ComponentOptions::watch_liveness(LivenessWatch)` polls a
  component's `health()` and reports a failure to the restart policy after
  `failure_threshold` `Down` answers (`initial_delay`);
  `RestartPolicy::reset_after`.
- **genai**: `MessagePart::Document` (`add_document_part`, `document`, `binary`)
  for inline documents; `GenAiError::Connect`; `GenAiError::api`,
  `with_error_type`, `with_retry_after` and `error_type()`.
- **genai**: `DefaultRouter::reload_config` applies a reloaded `RoutingConfig`
  atomically; per-tenant policies with `Task::tenant` and `TenantPolicy`
  (`new`, `deny_all`, `allow_provider`, `allow_model`, strategy and priority
  overrides), from code (`RouterBuilder::tenant_policy`) or `routing.tenants`;
  `require_tenant_policy(true)` refuses unknown tenants; the
  `EmbeddingRouter` applies tenant policies too.
- **vectorstore**: `MemoryStore::offload_threshold`; `load_file` / `load_dir`
  (and `_with` forms taking `LoadOptions`) read plain-text and Markdown into
  `LoadedText`, whose `chunks()` gives `TextChunk`s that convert into
  `RagDocument`s.
- **fsutils**: `FileLock`, an OS file lock (`flock` / `LockFileEx`) released
  by the kernel when its holder dies: `acquire`, `try_acquire`,
  `acquire_timeout`, `acquire_wait`, `owner`, `release`.
- **fsutils**: `fsutils::size`: `format_bytes` (KiB to EiB) and
  `format_bytes_si` (kB to EB) with `Precision`, and `ByteSize` parsing `1024`,
  `64KiB`, `1.5 MiB`, `2G` or `500MB`, with `Encode` / `Decode` and
  `JsonSchema` under `codec` (#101).
- **fsutils**: `Mmap::map` really maps files on 64-bit Windows
  (`CreateFileMappingW` + `MapViewOfFile`); `Mmap::map_supported()` is true
  there (#100).
- **hash**: `hash::random` works on Windows (`BCryptGenRandom`); it was
  `Unsupported`.
- **errutils**: `Classify` on `HttpError`, `VfsError`, `GenAiError`,
  `JwtError`, `WsError` and `MessagingError`; `ferroly::Error` delegates to
  them.
- **crate root**: `ferroly::Error` gains `Sig`, `Store`, `Proto`, `Grpc` and
  `Template` variants, each classified under `errutils`.
- **ci**: crates.io publish automation: `publish.yml` (verify, then publish
  from the protected `crates-io` environment, with a dry run, an index wait,
  optional trusted publishing and a dispatch input to publish an existing tag),
  `tag-on-merge` that tags the merged head for any merge method, the daily
  `crates-io-drift` check with an alerting issue, and
  `.github/scripts/check-release-version.sh` (Refs #90).
- **ci**: an `Interop` workflow with required h2spec, grpc-go (both
  directions) and Go `net/http` jobs; MSRV test runs, i686 checks, an
  `rt`-without-tokio consumer, and SHA-pinned actions.
- **fuzz**: new targets `h2_frames`, `h2_conn`, `grpc_frames`, `regex_match`,
  `ed25519_verify`, `ed25519_structured`, `proto_decode`, `www_authenticate`
  and `template_jinja`, and a seed corpus for `template_render`.

### Changed

- **build**: the optional `log` crate (no transitive dependencies) is the one
  exception to the dependency budget, and only with `log-bridge`.
- **build**: with tokio present, `obs` enables tokio's `rt-multi-thread`, so
  `obs` + `http` builds without default features.
- **genai**: the model-router YAML policy in `docs/model-router.md`, the
  `RoutingConfig` rustdoc and the router tests use compact YAML
  (`- provider: openai`, `capabilities: [text, chat]`); a test pins that it
  decodes the same as the verbose form (#24).
- **hash**: one always-compiled base64 implementation; `hash::base64`
  re-exports it unchanged.
- **docs**: the `errutils` classification list is brought up to date; new
  `docs/{sig,store,proto,grpc,template}.md`; the MSRV policy and the crates.io
  release process in `CONTRIBUTING.md`; supported versions in `SECURITY.md`.

### Deprecated

- **auth**: `encode_hs256` and `decode_hs256`.
- **turbo**: `Router::jwt_auth` and `JwtAuth::legacy_hs256`.

### Fixed

- **http**: applications that use `aws-lc-rs` (for example for FIPS) or another
  rustls provider can use ferroly without `ring` (#58). A provider installed
  after the first `https` request or JWK verification now takes over instead of
  the `ring` fallback found earlier.
- **http2**: review fixes H2-1 to H2-17 and HTTP-1 to HTTP-14: a handler task
  aborted before its first poll freed no stream slot; closed-stream rules and
  lower unused stream ids (h2spec 5.1); `:authority` / `:scheme` / control
  character validation; trimmed header values on send; HEADERS no longer wait
  behind the connection window; buffered bodies are admitted to the budget in
  arrival order, and concurrent uploads beyond it finish; frames for recently
  closed streams are not counted as floods (Go's client no longer gets
  `ENHANCE_YOUR_CALM`); write and stall timers measure the right intervals;
  drain covers the PROXY header and TLS handshake; idempotent requests on a
  dead reused connection are retried; per-host slots are released; an idle
  streamed upload stops when its stream is reset.
- **http2**: a refused HTTP/2 request (or one lost on a reused connection) is
  retried on another connection, dialing a new one while the origin is below
  `max_connections_per_origin`, instead of going back to the connection that
  refused it, so uploads refused for the per-connection body budget finish.
- **grpc**: review fixes GRPC-1 to GRPC-15 and RPC-3 to RPC-12: one
  structured `HttpError` to `Code` mapping (no message matching); a failure
  after the deadline is `DEADLINE_EXCEEDED`; a refused attempt is stopped
  before its replay; a panic in the response-stream pump or a server
  interceptor ends the call with `INTERNAL`; a draining health service keeps
  reporting `NOT_SERVING`; generated code compiles for rpcs named `Clone`,
  `Deref` or `AsRef`, for messages named like generated items, and for clashing
  client method names.
- **proto**: review fixes PB-1 to PB-14 and RPC-1, RPC-2, RPC-9: required
  fields are checked once every occurrence is merged (linear in the input),
  `Message::merge` into a complete message works, a replaced oneof variant no
  longer counts; encoding is linear at any depth; block-comment lexing is
  linear; dotted names resolve as `protoc` does; the derive calls its helpers
  fully qualified; generated code matches `protoc` byte for byte.
- **store**: review fixes ST-1 to ST-17: `purge_temp` never removes a file a
  fetch is using; cancelled waiters no longer leak; a cancelled claim always
  parks its bytes again; the one retry from byte 0 happens only when the
  content still began with the parked prefix; `fetch_http` turns off
  decompression; `with_basic_auth` gets its own token cache; S3 checksums that
  are not `FULL_OBJECT` are not used as digests; SigV4 canonicalises the path.
- **sig**: review fixes SIG-1 to SIG-8: `Ed25519Verifier` keeps the `S`
  validated in `new()`; addition chains for inversion and the square root
  (about 10% faster verification); corrected strictness docs.
- **codec**: review fixes MISC-RX-2 to MISC-RX-5, MISC-TV-2, CORE-1, CORE-9 to
  CORE-15 and CDC-3 to CDC-10: a counted repetition such as
  `^[a-z0-9]{1,255}$` costs about what `+` does; `is_match` reuses its
  scratch; a lone surrogate escape matches nothing instead of failing to
  compile; group names may repeat across alternatives; `#[derive(JsonSchema)]`
  of a recursive type terminates; `Option<enum>` schemas accept `null`;
  `DateTime::with_offset`, `to_utc` and `to_system_time` return `None` instead
  of overflowing; a `SystemTime` far before the epoch keeps its sign; rendered
  paths escape control and bidi characters and cap keys at 64 characters.
- **config**: review fixes CORE-3 to CORE-8, CLI-1 to CLI-10, CDC-1 and CDC-2:
  `watch_files` reloads once a change has settled, notices edits made before
  it started, returns `Result<ConfigWatcher, ConfigError>`, and hashes at most
  1 MiB per file; reloads never overlap; one bounded reader for every secret
  file; a secret directory is read from one revision; typed
  `merge_args_with` never takes `--` as a value, collects every occurrence of
  a list key and reads the cli boolean spellings; settings merge in linear
  time; `SecretRef` literal errors name the field.
- **config** (config review, CFG-1 to CFG-11 and CFG-13):
  - `${file:}` and `${env:}` expand only in trusted layers (files, values,
    secret files and directories); in a value from the environment, the
    arguments or the cli they are an `Interpolation` error and no file is
    opened. Opt in with `ConfigBuilder::interpolate_with(InterpolateOptions::new().trust_env())`
    or `.trust_args()`. A `${key}` reference expands as its own layer allows.
  - With a schema, declared cli arguments outside it (`--config`, `-v`) are
    dropped instead of reported as unknown; keys below a scalar or list leaf
    (`--server.port.x`, `APP_PORT__X`) are unknown.
  - Unknown-key suggestions are bounded in cost (banded distance, candidate
    and work caps), the `UnknownKeys` message lists at most 50 keys, and a
    literal dotted key is told to nest (`UnknownKey::dotted`).
  - Secret taint survives odd key spellings and a second `interpolate()`;
    `${env:}` values are masked; only well-formed `env:`/`file:` references
    are shown in the redacted dump, and `to_redacted_string_with`'s
    predicate is final.
  - `config::search` refuses an `app` name that is not one path component
    and a relative `HOME`/`APPDATA`; an unusable candidate (dangling link,
    unreadable directory) is returned and fails on load instead of falling
    through.
  - Secret files are re-checked after opening (a regular file with the same
    identity) and read from the handle.
- **cli**: dynamic-flag arity (`DynamicArity`); names with an empty `.`
  segment are never dynamic; `-` alone is an optional value.
- **template** (template review, TPL-1 to TPL-14): `[] * n` no longer
  spins; `sort`/`dictsort` use a total order (NaN last) and never panic;
  checked arithmetic in slices, `range`, `split`, `center` and repetition;
  `map('map', …)` counts against `max_depth`; `for` no longer copies the
  list, so `break` cannot dodge the iteration budget; unpacking and string
  slicing build only what they return; maps, scopes and argument lists are
  indexed, so large inputs stay linear; `map(attribute=…, default=…)` and
  macros defined in loops follow Jinja2; integer literals, `round`, `0 ** -n`,
  `int` signs and undefined arguments to `int`/`float`/`indent` follow Jinja2.
  New `Limits::max_steps` (default 10^8) bounds total work;
  `DEFAULT_MAX_SOURCE_BYTES` is now 256 KiB.
- **genai / vectorstore / obs**: review fixes SVC-2, SVC-3, SVC-5, SVC-6 and
  SVC-9 to SVC-15, SVC-17: connect errors are decided by phase; an abandoned offloaded search is cancelled before it
  starts; a configured tenant is laid over a code-registered one instead of
  replacing it; an evicted client span is marked instead of failed; OTLP over
  gRPC retries the specification's codes; a breaker's collector is removed with
  its series; `Traced` keeps the consumer span current while the handler's
  future is created and ends it on a panic; `ws_handshakes_rejected_total`
  folds rare statuses into their class.
- **lifecycle / turbo / errutils**: review fixes CORE-18 to CORE-23: the
  liveness watchdog clamps its settings and charges a failure only to the
  instance it sampled; an explicit HEAD whose body differs from GET gets no
  computed `ETag`; `If-None-Match` is parsed as entity-tags; every `errutils`
  variant is listed, and new ones default to `Unknown`.
- **fsutils**: review fixes CORE-2, CORE-24, CORE-25 and FSO-1 to FSO-10:
  `FileLock` keeps foreign file content, refuses links and reparse points, and
  survives unlink races and `Duration::MAX`; `ByteSize` honours width and
  alignment, requires a whole plain byte count and no longer treats bare `P`
  and `E` as units; `PlainExporter` prints a failed span's status and marks
  standalone events.
- **ci**: release-workflow review fixes (REL-1 to REL-11, SVC-1, SVC-4, SVC-7,
  SVC-8, SVC-16):
  `publish.yml` runs from `main` only, checks out the SHA it verified, passes
  no caller secrets and refuses an unprotected or tag-deployable `crates-io`
  environment; one release-version check for every workflow; tag-on-merge
  handles squash, merge-commit, rebase and fast-forward merges; the interop
  jobs are required checks.

### Security

- **template**: untrusted templates can no longer hang (TPL-1), panic
  (TPL-2, TPL-3), overflow the stack (TPL-4) or get around the iteration,
  memory and time budgets (TPL-5 to TPL-8); `Limits::max_steps` caps total work.
- **config**: an environment variable or argument can no longer read an
  arbitrary file into a configuration value through `${file:}` (CFG-1), and
  unknown-key suggestions can no longer be made to run for seconds (CFG-3).
- **Denial of service**: HTTP/2 stream-slot leak (H2-1), rapid-reset and
  cancel budgets (GRPC-1, GRPC-2, RPC-4), MadeYouReset (H2-3) and a
  per-connection buffered-body budget (H2-2); protobuf memory amplification
  (4 MiB of empty sub-messages expanded to a gigabyte; PB-1, RPC-9) and
  quadratic encoding, block-comment lexing and required-field tracking (PB-5,
  PB-10, RPC-1); a gRPC per-message decoded-memory budget; regex counted
  repetition (MISC-RX-2); a stack overflow on a ~60k-segment config key
  (CLI-9) and on recursive `JsonSchema` types (CORE-1); a panic in the OCI
  challenge parser on a multi-byte quoted pair (ST-1); `DateTime` overflow
  panics (CORE-11, CORE-14); unbounded secret-file reads (CORE-7); unbounded
  vectorstore loader reads and a FIFO that blocked them; an unbounded
  vectorstore search queue.
- **Amplification**: an 8 MiB body of failing items became millions of schema
  violations and a 200 to 500 MB `422` response (MISC-TV-1); a pattern
  violation copied the whole pattern (MISC-RX-1).
- **Credentials**: the OCI adapter answered an auth challenge from blob
  storage and sent credentials to its realm (ST-2); the store adapters
  followed the caller's redirects and decompression (ST-3, ST-4, ST-13); an HF
  token went to `http://` endpoints (ST-15); S3 clock skew could be forged to
  get requests signed for later (SEC-3); weak ETags in `If-Match` (SEC-2);
  `RemoteJwks` over plain http, blocking during a fetch, and serving stale
  sets forever (SEC-4, SEC-5, SEC-6); `jti` replay entries that expired before
  the token (SEC-1, SEC-8); `KeyFetch` answered as `401` (SEC-7); ambiguous
  repeated `Authorization` and API-key headers (SEC-11); cache directories
  left group- or world-readable (ST-17, SEC-12); a crypto provider installed
  late was bypassed (MISC-TLS-1); HMAC secrets below the RFC 7518 minimum.
- **Key material**: hasher, HMAC, HKDF / PBKDF2 and SigV4 intermediate state is
  wiped (checked by an allocator-based test, also in an optimised build;
  SEC-9, SEC-10); `hash::random` and WebSocket mask seeds use the OS CSPRNG on
  Windows too.
- **Injection and spoofing**: completion scripts escaped control characters
  and no longer use bash's `compgen -W`, which ran a `$(...)` choice at
  completion time (CORE-16, CORE-17); the PROXY protocol header is parsed only
  from trusted networks; `FileLock` refuses symbolic links and reparse points,
  and `load_dir` refuses entries swapped for a link (CORE-24, CORE-25); decode
  error paths escape control and bidi characters (CDC-4); the protobuf derive
  calls its helpers fully qualified, so a trait in scope cannot hijack them
  (PB-8); `genai` refuses an image part whose MIME type is not an image.
- **`unsafe`**: the crate has four audited `unsafe` regions, all FFI, each an
  `#[allow(unsafe_code)]` module with `SAFETY` comments: `fsutils/mmap.rs`
  (POSIX and now Windows mappings, behind `unsafe fn Mmap::map`),
  `fsutils/oslock.rs` (`flock` / `LockFileEx` behind `FileLock`),
  `hash/random/windows.rs` (one `BCryptGenRandom` call), and the read-only
  macOS and Windows process-statistics calls in `metrics/process.rs`. CI checks
  the FFI declarations on i686 Linux and i686 Windows. Everything else is
  compiled with `unsafe_code = "deny"`.

## [0.5.0]

Extension-readiness release for `messaging` and `vfs`: typed message
attributes and properties, subscription filters, a delivery context, per-entry
batch errors, receipts and capabilities for providers; write options,
richer metadata, typed errors and async abort for storage backends. Several
messaging types changed shape — see "Changed" and the migration table in
`docs/messaging.md`.

### Added

- **messaging**: typed message attributes — `AttrValue` (`Str` / `Int` / `Float` /
  `Bool` / `Bytes`) and the ordered multi-map `Attributes` (`insert`, `append`,
  `get`, `get_all`, typed getters, `remove`, `iter`, `keys`, `FromIterator` /
  `Extend` / `IntoIterator`).
- **messaging**: `Properties` — system metadata with new fields `content_type`,
  `content_encoding`, `subject`, `event_type`, `source`, `user_id`, `dedup_key`
  and `priority`, with `with_*` / getter pairs on `Message`; new `headers`
  wire-name constants, including `DEAD_LETTER_*` and `PROPERTY_NAMES`.
- **messaging**: wire helpers `Message::to_wire_attributes`, `Message::from_wire`
  and `Message::lookup`, for adapters to brokers with a flat attribute map.
- **messaging**: `MessagingError::Refused` (hands the refused message back) and the
  constructors `backend_transient`, `refused` and `refused_transient`;
  `MessagingError::is_retryable`; `MessagingError::take_message` is now public.
- **messaging**: `BatchEntryFailure` and the per-entry `BatchFailure` API (`new`,
  `first_error`, `into_messages`, `failed`).
- **messaging**: `Receipt` and `Producer::send_with_receipt`; `Capabilities` and
  `Provider::capabilities` (`LocalProvider` reports its features).
- **messaging**: `SendOptions::deliver_at`, `SendOptions::extensions` (builder
  `extension`) and `SendOptions::unsupported_by`.
- **messaging**: `Provider::set_observer` and the observer hooks `on_published` /
  `on_send_failed` (also logged by `LogObserver`).
- **messaging**: `ProviderRegistry::providers`, `capabilities_for`, `setup_all`,
  `close_all` and `set_observer`; the registry forwards `send_with_receipt` and
  `add_delivery_listener`.
- **messaging**: attribute filters on topic subscriptions — `Filter` and
  `ListenerOptions::filter` — plus `DropReason::Filtered`.
- **messaging**: `Ack::RetryAfter`, `Ack::DeadLetter` and `Ack::Pending`.
- **messaging**: the delivery context — `Delivery`, `DeliveryInfo`, `Acker`,
  `DeliveryControl`, `DeliveryHandler`, `delivery_handler` and
  `Receiver::add_delivery_listener`. `LocalProvider` implements it;
  `Delivery::extend` is a no-op there.
- **messaging**: dead-lettered messages carry `dead-letter-reason`
  (`max-delivery-attempts` / `rejected` / `expired`), `dead-letter-source`,
  `dead-letter-attempts` and, when there is one, `dead-letter-description` (a
  handler's text or the panic message).
- **messaging**: message priority (`Message::with_priority`); `LocalProvider`
  delivers higher-priority buffered messages first.
- **messaging**: de-duplication by `Properties::dedup_key`, falling back to the
  message id; scheduled delivery at an absolute time with `SendOptions::deliver_at`.
- **messaging**: `Message::encode_as(content_type, &value)` for any content type
  the codec registry knows.
- **vfs**: `Metadata` gains `etag`, `version`, `content_type` and
  `user_metadata`, with builders `with_etag`, `with_version`, `with_content_type`,
  `with_user_metadata` and the lookup `user(name)`; `MemoryFs` reports a fresh
  etag (also as `version`) every time a file's contents are stored.
- **vfs**: typed backend errors `VfsError::AlreadyExists`, `PreconditionFailed`,
  `Unavailable`, `Timeout` and `Unauthenticated`, plus `VfsError::is_retryable()`;
  with `errutils` the root `Error` classifies them as `Conflict` / `Conflict` /
  `Unavailable` / `Timeout` / `Unauthenticated`.
- **vfs**: `WriteSink::abort` (provided; the default drops the sink) and
  `VfsWriter::abort`, which discard a pending streaming write and wait for the
  backend's cleanup; `LocalFs` removes its temporary file.
- **vfs**: `WriteOptions` (content type, user metadata, precondition) and
  `Precondition::{IfAbsent, IfMatch}`, with provided methods `Vfs::write_with` and
  `Vfs::create_with`; the default refuses any precondition with `Unsupported`.
  `MemoryFs` supports every precondition atomically and stores the content type
  and user metadata; `LocalFs` returns `Unsupported` for any precondition.
- **vfs**: `Vfs::try_exists`, which tells "absent" (`Ok(false)`) from "could not
  check" (`Err`).
- **vfs**: `Vfs::signed_url` with `SignedUrlMethod::{Get, Put}` (default
  `Unsupported`), and `Vfs::as_any` for downcasting an `Arc<dyn Vfs>`; `LocalFs`
  and `MemoryFs` return `Some(self)`.
- **vfs**: `VfsRegistry::resolve_path` (now public) and the forwarders
  `read_range`, `append`, `create`, `write_with`, `create_with`, `list_page`,
  `walk`, `try_exists` and `signed_url`.
- **vfs**: `strip_scheme` is now public, for external backends.

### Changed

- **messaging (breaking)**: `Message::headers` and `with_header` / `set_header` /
  `header` / `header_str` are replaced by `Message::attributes` and
  `with_attribute` / `set_attribute` / `attribute` / `attribute_str`; system
  metadata lives in `Message::properties`. `Message` is `#[non_exhaustive]`.
- **messaging (breaking)**: `MessagingError::Backend` has a `transient` field
  (match it with `{ source, .. }`).
- **messaging (breaking)**: `MessagingError::Batch` is renamed `PartialBatch`;
  `BatchFailure` reports `sent`, per-entry `failures` and the never-tried
  `unsent`, and is `#[non_exhaustive]`.
- **messaging (breaking)**: `Ack` is `#[non_exhaustive]` with three new variants —
  a `match` on it needs a wildcard arm.
- **messaging**: a provider must refuse `delay` / `deliver_at` / `ttl` it cannot
  honour (`InvalidOptions`) instead of ignoring them.
- **messaging**: `Receiver::add_listener` and `Provider::setup` have default
  bodies (`Unsupported` and `Ok(())`), so a publish-only provider writes
  `impl Receiver for P {}`.
- **messaging**: `Message::json` sets the content type to `application/json`, and
  `Message::decode` uses the message's content type when the codec knows it, else
  JSON.
- **messaging**: with the `errutils` feature, `ferroly::Error` classifies every
  `MessagingError` variant, not only `BufferFull`.
- **vfs**: `ListPage::next` is documented as an opaque cursor; `copy_between`
  aborts the destination when the copy fails.
- **build**: `Cargo.lock` moves to syn 3.0.10 (build-time only, for
  `ferroly-derive`) and rustls-pki-types 1.16.0. Both still support Rust 1.75.
- **ci**: CodeQL actions 4.39.3, and `actions/upload-artifact` v7.0.2 in the
  fuzz workflow.
- **ci**: Dependabot no longer proposes tokio 1.54 or later, which requires
  Rust 1.85; tokio stays on 1.53.x while the MSRV is 1.75.

### Fixed

- **messaging**: a provider can now hand a refused message back, and a native
  batch can report each failed entry instead of only the first.
- **vfs**: `LocalFs`'s streaming writer waits for an in-flight write before
  removing its temporary file on abort (Windows cannot remove an open file).
- **docs**: the complete example in the messaging guide did not compile (the
  `Order` type lacked `Encode`).

## [0.4.0]

A hardening release. It fixes the defects found in a full review of the crate —
remotely triggerable parser panics, request smuggling and splitting in the HTTP
stack, a router filter bypass, message loss, and silent YAML mis-parsing — and
fills many capability gaps. Several fixes tighten behaviour that used to be
lenient, so **read the "Changed" section before upgrading**.

### Added

- **codec**: `codec::Limits` — one parser budget (nesting depth, input size, entry
  count) enforced by the JSON, XML, YAML and TOML parsers. Every entry point has a
  `*_with(…, limits)` twin, plus `codec::decode_with`; the plain entry points use
  the defaults (depth 128, 64 MiB, 10,000,000 entries). Exceeding a limit is
  `CodecError::LimitExceeded`.
- **codec**: parse errors in all four formats report the line and column.
- **codec**: `json::encode_pretty` / `json::to_string_pretty`; `try_encode` for
  JSON, XML and TOML and `codec::try_encode`, which fail with
  `CodecError::Unrepresentable` instead of writing a fallback.
- **codec**: `Encode`/`Decode` for tuples (up to 12), `[T; N]`,
  `BTreeMap<String, V>`, `HashSet`, `BTreeSet`, `Rc`, `Arc`, `Cow`, `i128`/`u128`,
  `Duration`, `PathBuf`, and the `std::net` address types; `Encode` for `[T]`,
  `&T`, `Path`.
- **codec**: `Value::pointer` / `pointer_mut` (JSON Pointer), `get_mut`,
  `get_index_mut`, `as_object_mut`, `as_array_mut`, `merge` (deep merge) and
  `take`; `codec::ValueDecoder` and `codec::from_value_strict` to decode a `Value`
  without string-to-scalar coercion.
- **codec**: TOML multi-line basic (`"""`) and literal (`'''`) strings. YAML flow
  collections, block scalars (`|`, `>`), compact sequence items, sequences at the
  indentation of their key, trailing comments, and the full set of quoted-string
  escapes. XML CDATA sections; empty strings, padded strings, nested arrays, empty
  maps and `vec![None]` now round-trip. Schema `type` may be an array of names.
- **derive**: `#[derive(Encode)]` / `#[derive(Decode)]` support generic types —
  type parameters, lifetimes, const parameters and where-clauses. Each type
  parameter used by an encoded field gets an `Encode` / `Decode` bound; override
  with `#[ferroly(bound = "...")]` or `#[ferroly(bound(encode = "...", decode = "..."))]`.
- **derive**: tuple structs (encoded as an array), newtype structs (transparent:
  just the inner value) and unit structs (`null`).
- **derive**: enums with data — newtype, tuple and struct variants. Externally
  tagged by default (`{"Variant": content}`; unit variants stay a plain string);
  `#[ferroly(tag = "...")]` selects the internally tagged form and
  `#[ferroly(untagged)]` the untagged one.
- **derive**: field attributes `#[ferroly(default)]` / `#[ferroly(default = "path")]`,
  `#[ferroly(skip)]`, `#[ferroly(alias = "...")]` (repeatable, decode only) and
  `#[ferroly(flatten)]`; container attribute `#[ferroly(deny_unknown_fields)]`;
  variant-level `alias` and `rename_all`; `rename_all = "SCREAMING-KEBAB-CASE"`.
- **derive**: `#[derive(FerrolyError)]` supports generic error types, `#[from]` on
  a named single-field variant or struct, `Option<E>` sources, raw-identifier
  fields in format strings, fields as width/precision arguments (`{0:>1$}`), and
  an enum-level `#[error("...")]` fallback.
- **errutils**: `.context("…")` / `.with_context(|| …)` on any `Result` or `Option`
  (`Context` trait, `ContextError`), keeping the original error as `source()`;
  `chain()` to walk an error chain.
- **errutils**: `Classify` trait and `ErrorKind` (`is_retryable()`,
  `is_user_error()`, `http_status()`), implemented for `std::io::Error`,
  `ContextError`, `MultiError`, `ConfigError`, `CliError` and `ferroly::Error`.
- **errutils**: `MultiError` implements `IntoIterator`, `FromIterator`,
  `From<Vec<_>>` and a generic `Extend`, and gains `iter()`, `first()` and
  `into_errors()`.
- **config**: optional files (`merge_file_optional`) and profile overlays
  (`merge_profile("config/app", "prod")`).
- **config**: opt-in `${VAR}` / `${VAR:-default}` interpolation from config keys
  and the environment, with cycle detection (`ConfigBuilder::interpolate`).
- **config**: `merge_env_with` + `EnvOptions`: secrets from files
  (`APP_DB__PASSWORD_FILE=/run/secrets/…`), list values, and case-insensitive key
  matching so camelCase keys are reachable from the environment.
- **config**: `Config::require` (reports every missing key),
  `Config::validate_schema`, `Config::contains`.
- **config**: provenance (`Config::source`, `Config::sources`) and a redacted dump
  of the effective configuration (`Config::to_redacted_string`).
- **config**: `SharedConfig`, a clonable handle with stable snapshots and manual
  `reload()`.
- **config**: `merge_cli` / `merge_cli_defaults` to feed a parsed `ferroly::cli`
  command line into the builder (with the `cli` feature).
- **cli**: repeatable and delimited options (`multiple`, `delimiter`, `get_all`),
  counters (`count`, `-vvv`), `choices`, parse-time `validator` / `typed::<T>()`,
  flag and subcommand aliases, mutually exclusive and required-together groups
  (`Group`), named and variadic positionals, global flags (`Arg::global`),
  `subcommand_required`.
- **cli**: `--version` / `-V`, "did you mean" suggestions, help that lists
  positionals, shows the parent command path and wraps long descriptions.
- **cli**: exit-code helpers (`CliError::exit_code`, `Command::parse_or_exit`,
  `Command::run`) and bash/zsh completion scripts (`Command::completions`).
- **hash**: `Sha512` and `Sha384`; a generic `Hmac<H>` with `HmacSha384`,
  `HmacSha512`, `HmacSha1` and a constant-time `verify`; `Md5` and `Crc32`
  checksums (not cryptographic); `kdf` with HKDF and PBKDF2; a public `base64`
  module (standard and URL-safe, padded and unpadded, strict decoding plus an
  explicit `decode_lenient`); a `hex` module and `Digest::from_hex`; `ct_eq` for
  constant-time comparison; `Secret`, which redacts itself in `Debug`/`Display`
  and is wiped on drop (best-effort); and `random` for secure random bytes and
  tokens (Unix only; other platforms return an explicit error).
- **auth**: `Validation` with `decode_hs256_with` / `decode_hmac_with`: expiry
  required by default, leeway, `nbf`/`iat` checks, maximum age, audience, issuer,
  subject, required claims and `typ`. HS384 and HS512. `encode_hmac` /
  `encode_with_header` (with `kid`). `decode_header`. A `Scopes` helper for
  `scope` / `scp` / `roles` claims.
- **auth** (with `http`): verify-only RS256 and ES256 from JWK / JWKS keys (`Jwk`,
  `Jwks`) and `RemoteJwks`, a cached key set that follows key rotation.
- **clients**: an RAII circuit-breaker `Permit` (`try_acquire`, `call`,
  `call_with`), a failure-rate window, state-change callbacks, metrics, and
  `CircuitBreakerRegistry`. `Retry` with jitter, a total time limit, a shared
  `RetryBudget`, and a hook that can honour `Retry-After` (`parse_retry_after`).
  `Bulkhead`, `RateLimiter`, `with_timeout`, `Deadline`, and `Policy`, which
  composes them.
- **clients**: refreshing credentials — `AuthProvider::prepare`, `authorize`,
  `TokenSource`, `RefreshingAuth` and `OAuth2ClientCredentials`
  (client-credentials grant).
- **http**: `Client::builder()` with connect, response-head, read-idle and total
  timeouts, a response-body size cap (64 MiB by default), connection-pool size and
  idle expiry, a default `User-Agent`, optional redirect following, forward
  proxies (absolute-form for `http://`, `CONNECT` tunnels for `https://`, basic
  auth, opt-in `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`), request/response
  interceptors, opt-in `gzip`/`deflate` response decompression, and
  `Client::send_streaming` for uploads from an async reader.
- **http**: client TLS options (`ClientTlsConfig`): custom CA roots in addition to
  or instead of the web PKI, client certificates, minimum TLS version.
- **http**: server TLS options (`TlsConfig::builder()`): mutual TLS (required or
  optional) and minimum TLS version; `ReloadableTlsConfig` +
  `serve_tls_reloadable` to rotate certificates without a restart.
- **http**: `Request::peer_addr()`, `local_addr()`, `is_tls()` and
  `peer_certificates()` for server handlers.
- **http**: `ServerConfig` gains `write_timeout`, `idle_timeout`,
  `handler_timeout`, `keep_alive`, `max_header_bytes`, `max_headers`,
  `max_line_bytes`, `normalize_paths` and `on_error` (`ErrorHook`).
- **http**: `RequestBuilder::form`, `Multipart`, `percent_encode`,
  `percent_decode`, `normalize_path`, `form_urlencode`, `format_http_date`,
  `Uri::resolve`, `Method::try_parse`, `HeaderMap::get_all` / `remove` / `try_set`
  / `try_append`, more `StatusCode` constants, `Response::uri` and
  `Response::set_max_body_bytes`.
- **http**: new `HttpError` variants `ConnectTimeout`, `BodyTooLarge`,
  `TooManyRedirects`, `Proxy`, `HandlerPanic`, and `HttpError::is_timeout()`.
- **turbo**: request-scoped extensions (`ctx.set` / `ctx.get`) and shared state
  (`Router::with_state`, `ctx.state`), so middleware can hand identity to
  handlers; `ctx.route()` returns the matched route template.
- **turbo**: asynchronous filters and authenticators (`filter_async`,
  `auth_async`) on routers, groups and routes.
- **turbo**: `ctx.peer_addr()`, `ctx.is_tls()`, `ctx.peer_certificates()`, and
  `TrustedProxies` / `ctx.client_ip()` for deployments behind reverse proxies.
- **turbo**: `*name` catch-all segments, nested groups, `Router::mount`, and
  `Route` for per-route middleware, filters, body limit and timeout. Groups gain
  `head`, `options`, `add`, `route`, `layer`, `group`.
- **turbo**: `Router::validate`, `problems` and `try_into_handler` report
  malformed patterns, conflicting routes and unusable middleware configuration
  (`RouteError`) without panicking.
- **turbo**: handlers may return any `IntoResponse` type, including
  `Result<HttpResponse, E>`. `Problem` renders RFC 9457
  `application/problem+json`; `Classified<E>` renders any `errutils::Classify`
  error.
- **turbo**: middleware — `cors` (`Cors`), `security_headers`
  (`SecurityHeaders`), `request_id`, `csrf` (`Csrf`, with the `hash` feature) and
  `rate_limit_with` (`RateLimit`, with `RateLimit-*` / `Retry-After` headers and a
  bounded table).
- **turbo**: cookies (`ctx.cookie`, `SetCookie` with secure defaults and
  `__Host-` / `__Secure-` rules), `ctx.form()`, `ctx.multipart()` with
  `MultipartLimits`, and `ctx.validate()` / `ctx.read_valid()` (422 with
  per-field details).
- **turbo**: `StaticFiles` / `Router::static_files` with `ETag`, `Last-Modified`,
  conditional requests, `Range`, and confinement to the root directory.
- **turbo**: `Router::body_limit`, `Router::request_timeout`;
  `serve_with_shutdown`, `serve_with_config`, `serve_listener`, `serve_tls`,
  `serve_tls_reloadable`, `serve_tls_listener`.
- **turbo**: JWT — `jwt_auth_with`, `jwt_auth_hmac_with`, `jwt_auth_jwks`,
  `JwtAuth` (including optional auth and `Group::jwt`), verified claims via
  `ctx.claims()`, and `require_scopes` / `require_any_scope` / `require_roles` /
  `require_any_role`.
- **turbo**: `trace_context()` extracts W3C trace context and runs a server span
  per request (with `obs`); `metrics_in(&registry)`.
- **rest**: client `base_url`, `default_header`, `default_query`; connect / read /
  total timeouts, response-body cap, proxy, TLS, redirects, interceptors, user
  agent, decompression; `http_client` to share a transport; `try_with_options`.
- **rest**: per-host circuit breakers (`circuit_breaker`, `circuit_breakers`) and a
  `Policy` hook.
- **rest**: `Response::headers`, `header`, `status`, `url`, `into_bytes`, `link`,
  `next_link`; `Response::error_for_status()`, an `error_for_status(true)` client
  option, and `ClientError::Status { status, body }`.
- **rest**: `RequestBuilder::accept`, `idempotency_key`, `idempotent`;
  `Client::patch`, `head`, `options`; `Client::paginate` (cursors) and
  `paginate_links` (`Link: rel="next"`). The client injects W3C trace context into
  outgoing requests (with `obs`).
- **rest**: `ServerOptions::config` (`ServerConfig`) and `ServerOptions::tls`
  (`TlsConfig` or `ReloadableTlsConfig`); `Server::last_error`, `Server::is_tls`;
  `ServerBuilder::patch`, `head`, `options`, `route`, `group`, `layer`, `cors`,
  `security_headers`, `request_id`, `try_build`.
- **ws**: per-connection async handlers — `WsHandler`, `WsConn` (id, request
  path/query/headers, peer address, TLS state, `recv`, `send`,
  `close(code, reason)`) and a clonable `WsSender` for pushing from other tasks;
  `server::serve_with_config` with graceful shutdown.
- **ws**: `WsServerConfig` for every limit and timeout (max connections,
  frame/message size, handshake, idle, write, keepalive ping and pong deadline,
  close, drain, queue capacities, header limits).
- **ws**: `UpgradeHook` to authenticate or reject an upgrade with an HTTP response
  before the `101`, and to attach per-connection context; `OriginPolicy`
  (`SameOrigin`, `AllowList`, `Any`).
- **ws**: `Hub` with rooms — broadcast to all, to a room or to one connection id,
  with a `SlowConsumerPolicy`.
- **ws**: `wss` and mutual TLS on the server (`WsServerConfig::tls`); custom
  request headers, subprotocols and `ClientTlsConfig` on the client; subprotocol
  negotiation on both sides.
- **ws**: client keepalive pings, connect and handshake timeouts, and
  `ReconnectingClient` with exponential backoff, jitter and an `on_connect`
  resubscribe hook.
- **ws**: close codes are visible to applications — `Message::Close { code,
  reason }` is the last message of every connection; `close(code, reason)` /
  `WsClient::close_with`; `close_code` constants.
- **ws**: `WsObserver` / `WsEvent` for metrics; `WsServer::with_handler`,
  `with_config`, `connections()` and `health()`.
- **genai**: streaming is a sequence of typed events —
  `CompletionChunk::{Start, TextDelta, ReasoningDelta, ToolCallDelta,
  ProviderData, Usage, Done}` — with `StreamAccumulator` / `collect_stream` to
  fold a stream into a `CompletionResponse`.
- **genai**: specific errors — `GenAiError::{RateLimited, Auth, Timeout,
  ContentFilter, ContextLengthExceeded, Overloaded}` — with `Retry-After` parsed,
  rate-limit headers kept, and helpers `status()`, `retry_after()`,
  `is_transient()`, `is_caller_error()`, `kind_label()`, `error_class()`.
- **genai**: `FinishReason` (with the provider's string in `raw_finish_reason`),
  `Grounding` / `Citation`, and `CompletionResponse::{id, reasoning,
  tool_calls(), is_truncated(), decode_value()}`.
- **genai**: more typed options — `stop`, `seed`, `top_k`, `presence_penalty`,
  `frequency_penalty`, `user`, `parallel_tool_calls`, `reasoning_effort`,
  `prompt_cache` — plus `ResponseFormat::Schema` / `strict_schema` and
  `ToolDefinition::strict`.
- **genai**: `ProviderOptions` gains timeouts (`connect`, `response`,
  `stream_idle`, `total`), a custom `AuthProvider` for every provider, extra
  headers, proxy and TLS passthrough, a ready-made `http::Client`, optional retry,
  an observer, and body-size caps.
- **genai**: `OpenAiConfig` for OpenAI-compatible servers and Azure OpenAI;
  `ClaudeProviderConfig` builder (API version, beta headers,
  `omit_sampling_params`, `default_max_tokens`).
- **genai**: usage reports cached, cache-write and reasoning tokens;
  `ModelInfo::cost_for(&usage)` computes the cost of a call.
- **genai**: `GenAiObserver` hook with adapters `ObsObserver` (`obs`),
  `MetricsObserver` (`metrics`) and `LogObserver` (`log`), plus `redact_payload`.
- **genai**: `MockProvider` — a scripted provider and embedder that records
  requests, for tests.
- **genai**: embeddings accept `dimensions` and `user`; large inputs are batched
  automatically with order preserved. `genai::rag::RagIndex` (feature
  `vectorstore`) — embed-and-upsert and query-by-text.
- **genai**: prompt templates gain `{{#if}}` / `{{#unless}}` / `{{#each}}`,
  `{{> partial}}`, comments, a `\{{` escape, a compiled `Template` with strict
  rendering, versions, and `InMemoryPromptStore::load_dir`.
- **genai** (router): `RouterBuilder::from_config`, cumulative budgets (`Budget`,
  `BudgetStore`, `InMemoryBudgetStore`), sticky routing, `RouteReport` with
  estimated vs actual cost, tag-based rules, `rank_overrides`, `default_model`,
  shared `CircuitBreakerRegistry`, `fallback_on_timeout`,
  `GenAiProvider::model_info_for(model)`.
- **genai** (router): `ModelInfo` carries `quality_rank`, `speed_rank`, cache
  prices and an `as_of` date; stale compiled-in prices are reported once.
- **vectorstore**: metadata `Filter`s and `min_score` on `Query`; namespaces
  (`MemoryStore::namespace`) for tenant isolation; `get`, `count`, `delete_where`,
  `list`; JSON snapshots (`save` / `load`); `*_sync` methods; `Chunker` for
  fixed-size, sentence- and paragraph-aware text chunking;
  `Metric::to_unit_interval`.
- **lifecycle**: `Component::health` and `ComponentManager::health()` /
  `is_ready()` / `is_live()` — readiness follows component state (not ready until
  every component is `Running`).
- **lifecycle**: `HealthRegistry` asynchronous checks (`register_async`, `check`,
  `check_probe`) with a per-check timeout and optional result caching; liveness /
  readiness / startup probe groups; `register_components` to include a manager's
  components in a registry.
- **lifecycle**: opt-in parallel start of independent components
  (`ManagerOptions::parallel_start`), per-component start/stop timeouts and
  restart policies (`ComponentOptions`, `RestartPolicy`, `register_with`,
  `report_failure`).
- **lifecycle**: `ShutdownHandle` for programmatic shutdown, reload notifications
  (`subscribe_reload`, `trigger_reload`, optional SIGHUP on Unix), a global
  state-change stream (`subscribe`) and `LifecycleObserver`.
- **messaging**: publish/subscribe topics (`topic://name`) with `*` and `>`
  wildcards and shared subscriptions, alongside `chan://` queues.
- **messaging**: message TTL, delayed delivery, request/reply (`request`, `reply`,
  `responder`), de-duplication by message id, `send_batch`, and backpressure
  policies (`OnFull::{Reject, Block, BlockFor, DropOldest}`).
- **messaging**: configurable retry backoff (`ListenerOptions::retry_backoff`),
  header conventions for message id, correlation id, timestamp, reply address and
  W3C trace context, and observer hooks for drops, expiry, handler duration,
  handler panics and queue depth.
- **messaging**: `LocalProvider::shutdown` returning the messages it could not
  deliver, `queue_depth`, `pending`; `ProviderRegistry`; `Provider::close`,
  `Receiver::remove_listeners`, `MessagingError::backend` for external provider
  implementations.
- **vfs**: `LocalFs::rooted(dir)` confines every path to one directory (`..`,
  absolute paths and escaping symlinks are rejected). `MemoryFs` in-memory
  backend. `VfsRegistry` routes URLs to backends by scheme and copies across them.
  `Vfs::create` returns a streaming `VfsWriter` published atomically on `finish`.
  `Vfs::append`, `Vfs::list_page`, `copy_between`, `glob` / `glob_match`,
  `checksum_sha256` (with `hash`). `Metadata` reports modified and created times,
  read-only, Unix mode and symlinks.
- **fsutils**: `write_atomic` / `AtomicFile` (temporary sibling, fsync, rename);
  `TempDir` / `TempFile` (unique names, removed on drop); `LockFile`, an advisory
  create-new lock file; `Mmap::map`, `Mmap::map_supported`, `Mmap::is_mapped`.
  Far broader content-type coverage: Office, AVIF/HEIC, JSONL, Parquet, protobuf,
  fonts and archives by extension; mp4/`ftyp` family, RIFF variants, tar, zstd,
  xz, bzip2, 7z, fonts and text/HTML/XML/JSON by content.
- **rt**: the `rt-test-util` feature re-exports the paused test clock
  (`rt::time::{pause, advance, resume}`).
- **rt**: `CancellationToken`, `TaskTracker`, `shutdown_signal()`. Declarative
  `async_test!` / `async_main!` that need no `tokio` dependency.
  `#[ferroly::rt::main]`. Re-exports for `fs`, `process`, `task_local!`,
  `LocalSet` / `spawn_local`, `AbortHandle`, `UdpSocket`, Unix sockets,
  `OnceCell`, `Barrier`, `runtime::Handle`, `MissedTickBehavior`,
  `io::{split, duplex}`.
- **log**: several sinks per logger, each with its own format and minimum level
  (`Sink`, `Logger::add_sink` / `to_sink`); a blocking overflow policy for the
  non-blocking appender (`Overflow::Block`); `RotatingFile` (size and daily
  rotation with a retained-file count).
- **log**: per-target level overrides (`Logger::with_target`, `set_level_for`);
  field redaction (`Redactor`, on by default); sampling (`Logger::sample`); caller
  capture (`Logger::with_caller`); a panic hook (`install_panic_hook`);
  `log::flush()` and `flush_guard()`; timestamp precision and fixed UTC offset;
  `add_trace_context_provider()` to put the current `obs` trace and span id on
  every record; `Level: FromStr`.
- **metrics**: `FloatCounter` and `FloatGauge`; `try_*` constructors returning
  `MetricsError`; pre-bound label handles (`counter_vec`, `histogram_vec`, …);
  `Histogram::start_timer` and `Gauge::track` RAII guards; a per-family
  cardinality cap with an `overflow="true"` series; `unregister`, `remove`,
  `clear`, `reset`; collectors and an opt-in process collector; family metadata
  and units; an OpenMetrics encoder (`encode_openmetrics`).
- **obs**: `instrument`, `with_context` and `with_current_context` for async
  code; `Span::child_of` and `ActiveSpan` for explicit parents; W3C Trace Context
  (`Context::from_traceparent` / `to_traceparent`, `extract`, `inject`).
- **obs**: span kind, status, links and renaming; samplers (`AlwaysOn`,
  `AlwaysOff`, `TraceIdRatio`, `ParentBased`); `Resource` attributes; multiple
  exporters and `obs::shutdown()`; semantic-convention helpers (`obs::http`,
  `obs::messaging`, `obs::genai`); an `OtlpHttpExporter::builder` with headers,
  timeout, batch and queue sizes, retries and statistics.
- **fuzz**: a `toml_parse` fuzz target and a committed seed corpus for every
  target; a weekly CI fuzz run.
- **`ferroly::Error`**: `?` now works for `vfs::VfsError`, `cli::CliError`,
  `genai::RouterError`, `rt::JoinError`, `errutils::MultiError` and
  `errutils::ContextError`.
- **build**: each crate now ships its `LICENSE-APACHE` and `LICENSE-MIT` files.
  Each `ferroly` feature enables only the `tokio` features it needs; `rt` is the
  one feature that enables all of tokio.

### Changed

- **codec (breaking)**: JSON and TOML decode strictly — a string is no longer
  accepted where a number or bool is expected (`"42"` is not an integer). XML,
  YAML and `Decode::decode(&Value)` keep the lenient behaviour that text-only
  sources need.
- **codec (breaking)**: a float decodes into an integer type only when it is a
  whole number that fits; `1.9` and `1e300` are now errors rather than `1` and
  `u64::MAX`.
- **codec**: JSON follows RFC 8259 — `01`, `1.`, `-.5`, numbers that overflow to
  infinity, and raw control characters inside strings are rejected.
- **codec (breaking)**: YAML is a strict subset of YAML 1.2 — anchors, aliases,
  tags, directives, explicit keys, a second `---` document and any line the parser
  cannot place are errors. Plain scalars follow the core schema (`inf`/`nan` are
  strings; `.inf`/`.nan`, `True`, `0x1F`, `0o17` resolve). Output: floats keep a
  `.0`, non-finite floats are `.inf`/`.nan`, empty top-level collections are
  `{}`/`[]`, bytes are a flow sequence.
- **codec**: XML output — `null` is `<a/>`; empty arrays, nested arrays, empty
  objects and empty or padded strings carry a `type` or `xml:space` attribute; map
  keys that are not XML names are escaped as `_xHHHH_`. XML input — content after
  the root element, mixed content and `DOCTYPE` are errors; literal CR/CRLF is
  normalized to LF.
- **codec**: TOML input — bare words, malformed numbers, a table defined twice, a
  lone carriage return and raw control characters are errors; CRLF inside a
  multi-line string decodes to LF.
- **codec**: schema `"integer"` accepts whole floats; `const`/`enum` compare
  numbers by value and objects without regard to member order; a `type` keyword
  that cannot be interpreted is a violation.
- **codec**: `impl Encode for &str` is now the blanket
  `impl<T: Encode + ?Sized> Encode for &T`. `CodecError` gained `LimitExceeded`
  and `Unrepresentable`. Inputs over 64 MiB or 10,000,000 entries are rejected by
  default.
- **derive**: `rename_all` treats an acronym as one word: `HTTPServer` becomes
  `http_server` / `httpServer` / `HttpServer` (was `h_t_t_p_server` / `hTTPServer`
  / `HTTPServer`). Ordinary snake_case fields and PascalCase variants are
  unaffected. Use `rename` to keep an old key.
- **derive**: now compile errors instead of being ignored — an unknown
  `rename_all` rule, a repeated `#[ferroly(...)]` key, a field-only key on a
  variant, two fields or variants that map to the same key, `#[from]` on a struct
  or variant with more than one field, and misplaced `#[from]` / `#[source]` /
  `#[error]` attributes.
- **derive**: derived `encode_to` passes the exact number of entries to
  `Encoder::begin_map` (fields omitted by `skip_none` are no longer counted).
- **errutils**: `MultiError::source()` returns the first collected error.
- **config**: `merge_args` no longer guesses that the token after `--flag` is its
  value. It is taken as the value only when an earlier layer shows the key holds a
  non-boolean value; otherwise the flag is a switch. `--key=value` always works,
  and `--` ends flag processing.
- **config**: a malformed config file is reported as
  `ConfigError::Parse { path, message }` and a failed `get` conversion as
  `ConfigError::InvalidValue { key, source }` (both were `ConfigError::Codec`).
  I/O errors now name the file. `ConfigError` has further new variants.
- **cli**: positional arguments the command does not declare are rejected
  (`CliError::UnexpectedArgument`); declare a `variadic()` positional to accept
  any number.
- **cli**: `CliError::UnknownFlag` has a new `suggestion` field; `CliError` has new
  variants; `.env()` / `.default()` now apply to switches and positionals; help
  layout changed.
- **hash**: `HmacSha256` is now an alias for `Hmac<Sha256>` (same methods).
- **auth**: `decode_hs256` parses strictly. It rejects non-canonical or padded
  base64url, claims or headers that are not JSON objects, duplicate member names,
  a `crit` header, empty secrets, secrets that are public keys, and tokens over
  64 KiB. Tokens from issuers that pad base64url no longer verify. `JwtError` has
  new variants and is `Clone`.
- **clients**: the `clients` feature now enables `hash` and `codec`. `BearerAuth`,
  `ApiKeyAuth` and `BasicAuth` no longer show their credentials in `Debug`. A
  `CircuitBreakerConfig` with `max_half_open: 0` now allows one trial.
- **http** server: malformed requests are answered
  (`400`/`408`/`413`/`414`/`417`/`431`/`501`/`505`) and the connection closed.
  HTTP/1.1 requests need exactly one `Host` header; HTTP/1.0 connections close
  unless the client asks for keep-alive; responses carry a `Date` header; the
  server sets `Content-Length`/`Transfer-Encoding` itself; `100 Continue` is sent
  only after the size check. A client reading slower than the write timeout
  (30 s per 64 KiB by default) is disconnected.
- **http** client: `Uri::parse` rejects control characters, spaces and userinfo,
  drops the `#fragment`, and accepts bracketed IPv6 literals. Response bodies are
  capped at 64 MiB and body reads time out after 300 s idle by default.
  `Response::chunk` returns pieces of at most 16 KiB. A `User-Agent` is sent by
  default.
- **http**: `HeaderMap::set`/`append` strip every character that is not valid in a
  header name and all control characters from values; `content_length()` and
  `is_chunked()` are strict.
- **http (breaking)**: `Request` can no longer be built with a struct literal (use
  `Request::new` / `Request::builder`); build `ServerConfig` with
  `..ServerConfig::default()`.
- **turbo (breaking)**: the request path is normalised before routing. `//x`, a
  path without a leading `/`, malformed percent-escapes and encoded control
  characters are answered `400`; dot segments are resolved; `ctx.path()` returns
  the normalised path. Paths are case-sensitive.
- **turbo**: path parameters are percent-decoded, and query keys as well as
  values. A query that is not valid percent-encoded UTF-8 is a `400`.
- **turbo (breaking)**: the most specific route wins regardless of registration
  order. `HEAD` (for `GET` routes) and `OPTIONS` are answered automatically; opt
  out with `auto_head(false)` / `auto_options(false)`.
- **turbo**: `metrics()` labels `http_requests_total` and
  `http_request_duration_seconds` with the route template (`route`), and reports
  non-standard methods as `OTHER`.
- **turbo**: `trace_context()` accepts an inbound `X-Request-Id` only if it is
  1–128 characters of `A-Z a-z 0-9 . _ -`, adds a `request_id` log field, and with
  `obs` logs the W3C trace id as `trace_id`.
- **turbo**: `jwt_auth(secret)` keeps its lenient validation (no `exp` required);
  use `jwt_auth_with` for new code.
- **rest (breaking)**: `ServerOptions` has new public fields; build it with
  `..Default::default()`.
- **rest**: `Client::new()` no longer disables timeouts — 30 s connect, 60 s
  response head, 300 s read idle, 64 MiB response body.
- **rest**: `retry_policy` accepts `impl Into<Retry>`. Idempotent requests are
  retried on transient transport errors and on `502` / `503` / `504` / `429`,
  honouring `Retry-After`; non-idempotent requests only when opted in.
- **rest**: requests send `Accept: <content type>, */*;q=0.5` unless one is set.
  `/health` runs the registry's liveness checks and `/ready` its readiness checks,
  through the asynchronous registry API.
- **ws (breaking)**: `WsClient::send` is now `async` and waits for space in a
  bounded queue; use `try_send` for the non-waiting form.
- **ws (breaking)**: `Message` has a new `Close { code, reason }` variant; `recv`
  yields it before returning `None`.
- **ws (breaking)**: `WsOptions` has new fields (end struct literals with
  `..WsOptions::default()`) and its defaults are now finite: 16 MiB per frame,
  64 MiB per message, 10 s connect and handshake timeouts.
- **ws (breaking)**: servers apply `OriginPolicy::SameOrigin` by default — a
  browser connecting from another origin gets `403` until it is allow-listed.
  Requests without an `Origin` header are unaffected.
- **ws**: servers now default to a 120 s idle timeout, a 30 s write timeout and a
  30 s keepalive ping with a 10 s pong deadline. A non-101 handshake response is
  `WsError::Rejected { status }`, and sending on a closed connection is
  `WsError::Closed`. `WsServer::start` on a running server returns
  `ComponentAlreadyStarted` instead of replacing it.
- **genai (breaking)**: `CompletionChunk` is an enum (use `chunk.text()` instead
  of `chunk.delta`); `CompletionResponse` is `#[non_exhaustive]` and its
  `finish_reason` is `Option<FinishReason>`; `MessagePart` and `ResponseFormat`
  are `#[non_exhaustive]`; several structs gained fields; `ClaudeProviderConfig`
  is built with `new` / `with_auth`.
- **genai (breaking)**: provider failures map to the new error variants (a `429`
  is `RateLimited`, `401`/`403` is `Auth`, a timeout is `Timeout`) instead of
  `Api` / `Transport`.
- **genai**: default timeouts suit model calls — 10 s connect, 10 min until the
  response starts, 5 min stream idle.
- **genai**: OpenAI requests send `max_completion_tokens`; streamed requests ask
  for usage. Ollama sends and parses tools.
- **genai**: the Claude catalog lists current models; when `max_tokens` is unset
  the provider derives it from the catalog instead of sending 1024.
  `ResponseFormat::Json` on Claude returns `Unsupported` (use a schema).
- **genai**: rendering through `Template`, `PromptTemplate::render_strict`,
  `InMemoryPromptStore::render` and `Message::from_prompt` is strict; the existing
  `render` functions stay lenient. `{{ . }}` no longer renders the whole variables
  object.
- **genai** (router): `Priority::Quality` and `Priority::Speed` rank by explicit
  ranks; routing configuration is parsed strictly; a completion that timed out
  after it was sent is no longer retried or failed over by default; when every
  breaker is open the error is `RouterError::CircuitOpen`.
- **vectorstore (breaking)**: `Query` is `#[non_exhaustive]` (use `Query::new`).
  Vectors with NaN or infinite components, and empty vectors, are rejected with
  `VectorStoreError::InvalidInput`.
- **lifecycle (breaking)**: `ComponentManager::register` and `unregister` return
  `Result`. Registering a duplicate id is an error; unregistering a starting,
  running or stopping component is refused.
- **lifecycle**: a failed `start_all` / `start` / `start_and_wait` stops the
  components it had started (disable with
  `ManagerOptions::rollback_on_start_failure(false)`). `stop_all` and `stop`
  attempt every component and return all failures (`LifecycleError::Multiple`)
  instead of stopping at the first.
- **lifecycle**: component `start`/`stop` run on their own task; on timeout or
  cancellation they are aborted at the next `.await`, so implementations must be
  cancel-safe.
- **messaging (breaking)**: send errors hand the rejected message back —
  `NoListeners`, `BufferFull` and `Closed` now carry a `Rejected` (use
  `MessagingError::into_message`). `SendOptions` and `ListenerOptions` have new
  fields; construct them with `..Default::default()`.
- **messaging**: a retried message is redelivered to the same listener after a
  backoff (default 100 ms, doubling) and keeps its place in the queue, so per-key
  order is preserved across retries.
- **messaging**: `stop()` drains buffered messages (up to
  `LocalOptions::drain_timeout`, default 20 s) and fails if some could not be
  delivered; after a stop, sends return `Closed`. `Message::with_header` replaces
  an existing header with the same key.
- **vfs (breaking)**: `LocalFs` rejects URL schemes other than `file://` instead
  of stripping them. `write` replaces files atomically and refuses read-only
  destinations. `Metadata` is `#[non_exhaustive]` — build it with
  `Metadata::file` / `Metadata::dir`. `LocalFs` is no longer a unit struct (use
  `LocalFs::new()`). Directory/file mismatches are reported as `IsDir` / `NotDir`
  on every platform.
- **fsutils (breaking)**: `Mmap::open` now reads the file into memory and is sound
  against concurrent changes to the file. The zero-copy OS mapping is the new
  `unsafe fn Mmap::map`, and is a real mapping only on 64-bit Linux, Android,
  macOS, iOS and the BSDs. `sniff` and `detect_content_type` now recognize text
  (plain, JSON, HTML, XML) where they returned nothing.
- **log**: sensitive field names (`password`, `token`, `authorization`, …) are
  redacted by default (`Logger::redact(Redactor::none())` turns it off).
  Plain-format keys and values are quoted when they contain whitespace, control
  characters, `=`, `"` or `\`. A field named like one of the record's own keys
  moves under `fields`; a repeated key is printed once.
- **metrics**: a metric name has one kind; an invalid registration through
  `counter` / `gauge` / `histogram` yields a detached instrument and is counted in
  `registration_errors()` instead of being exported. Histogram bucket bounds are
  fixed by the first registration of a name. Infinite observations are ignored.
- **obs**: trace and span ids are random. `FinishedSpan` has new fields (`kind`,
  `status`, `links`, `trace_state`, `parent_remote`) and implements `Default`.
  The default sampler follows an upstream "not sampled" decision. The OTLP
  exporter queue is bounded (2048 spans) and its request timeout is 10 s.
- **build**: the minimum `tokio` version is 1.38.
- **build**: the `tokio` dependency no longer enables `full` for every feature.
  If your crate used tokio APIs only because ferroly happened to enable them,
  depend on `tokio` directly or enable `ferroly`'s `rt` feature.

### Fixed

- **codec**: TOML — a multibyte character after a short `\u`/`\U` escape caused a
  panic; a dotted key or table header with very many segments overflowed the
  stack (the depth limit now covers key paths).
- **codec**: XML — an unterminated `<?` or `<!--` caused a panic; a `>` inside an
  attribute value ended the tag; `[[1,2],[3]]` flattened; map keys could inject
  markup.
- **codec**: YAML — documents silently lost or mis-read data (a sequence at the
  indentation of its key, keys after the first in `- k: v` items, trailing
  comments, flow collections); `encode` output did not always decode (strings
  containing `: `, top-level negative numbers, top-level empty collections).
- **codec**: XML and TOML documents with many distinct keys took quadratic time.
  Hex escapes and character references accepted a leading `+`. Schema ignored an
  array-valued `type`, and `enum: [5]` rejected an unsigned `5`.
- **derive**: raw identifiers no longer leak into keys — a field named `r#type`
  encodes and decodes as `"type"` (was `"r#type"`).
- **derive**: `#[derive(FerrolyError)]` on a struct with `#[source]`, `#[from]` or
  `#[error(transparent)]` now compiles, and `#[from]` on a struct generates the
  `From` impl. `#[derive(Encode)]` on an enum with no variants compiles.
- **config**: `ConfigBuilder`'s `Debug` output no longer prints configuration
  values.
- **config**: loading from the environment no longer panics when any environment
  variable is not valid UTF-8.
- **config**: `.toml` config files are loaded; the unsupported-format error lists
  every supported extension.
- **config**: a parse error no longer repeats the offending line of the config
  file; environment variables with empty key segments (`APP_A____B`) no longer
  create empty-named keys.
- **cli**: `--flag=false` on a boolean switch is honoured; `-n=5` yields `5`; a
  negative number such as `-5` is accepted as an argument; a program's own `-h`
  flag takes precedence over built-in help; flags marked `global()` are accepted
  after a subcommand.
- **auth**: a token could be respelled without invalidating it (non-canonical
  base64url or stray `=` in the signature); non-object claims were returned as
  valid.
- **clients**: a half-open trial whose caller was cancelled left the circuit
  breaker stuck half-open until restart. Permits release the slot when dropped,
  and trials taken through `can_execute` are reclaimed after a timeout.
- **clients**: auth providers printed bearer tokens, API keys and passwords in
  `Debug` output.
- **http** server: request smuggling through a second `Transfer-Encoding` header
  or a coding other than a lone `chunked`; lenient header, `Content-Length`,
  line-ending and chunk parsing; unbounded chunk-size lines and trailers; a
  non-reading client holding a connection slot forever; a handler panic dropping
  the connection without a response (now a `500`, and the server keeps serving).
- **http** client: request splitting through CR/LF or spaces in URLs, methods and
  hand-built request targets; `1xx` interim responses taken as the final response;
  `HEAD` responses with `Content-Length` hanging; truncated bodies reported as
  success; non-idempotent requests re-sent on a dead pooled connection; unbounded
  response bodies.
- **http**: `download_to_file` validates `Content-Range`, restarts when the server
  ignores `Range`, and reports truncated transfers. Server-Sent Events could be
  forged through line breaks in `data`, `event` and `id`.
- **turbo**: group-scoped filters could be bypassed with `//api/x`, `api/x` or dot
  segments, and never ran for a group whose prefix contained a parameter. Filters
  are now scoped with the route matcher on the normalised path, and each route
  re-checks its groups' filters before the handler runs.
- **turbo**: arbitrary HTTP method tokens and request paths could create unbounded
  metric label values; the in-flight gauge and duration histogram are recorded by
  guards, so a cancelled request no longer leaks a count.
- **rest**: `path_param` values were substituted raw, so `/`, `?`, `#`, `..` or
  CR/LF in a value could change the path, query or request line. Values are now
  percent-encoded for their position; values that cannot be a path segment are
  refused.
- **rest**: the retry documentation promised retries on 5xx responses, but only
  transport errors were retried. Refreshing auth providers (OAuth 2.0,
  `RefreshingAuth`) were never prepared, so their requests went out without
  credentials.
- **rest**: `Server` could not be restarted, discarded serve errors, and a
  cancelled `start` left it unusable.
- **ws**: a peer that stops reading (or floods pings) can no longer grow memory —
  outgoing and incoming queues are bounded, pending pongs are coalesced, and a
  slow consumer is closed with `1008`.
- **ws**: control frames must be final and at most 125 bytes; unmasked client
  frames, masked server frames, reserved bits, unknown opcodes and out-of-order
  fragments close the connection with `1002`; invalid UTF-8 closes it with `1007`
  instead of being replaced; oversized frames and messages close it with `1009`;
  close-frame payloads are validated and the status code is echoed.
- **ws**: the server validates the whole upgrade request and answers a bad one
  with an HTTP error instead of `101`; the client verifies `Upgrade`,
  `Connection` and `Sec-WebSocket-Accept`, and refuses a subprotocol or extension
  it did not offer.
- **ws**: stopping a server sends close `1001` to live connections and waits a
  bounded time before dropping them. Masking keys come from a per-connection
  generator seeded from OS randomness, and the handshake nonce no longer exposes
  the key stream.
- **genai**: OpenAI and Ollama dropped assistant tool calls and tool results from
  outgoing messages; both now send them correctly and parse parallel tool calls.
- **genai**: streams no longer lose tool calls, usage, finish reasons or errors; a
  provider error event, a stalled stream, or a connection that closes early is
  reported as an error. Multi-byte UTF-8 characters split across network chunks
  are no longer corrupted.
- **genai**: a slow completion is no longer re-sent (and billed twice) after a
  timeout; an abandoned stream no longer leaks its reader task and connection.
- **genai**: `Options::custom` is now sent to the provider.
- **genai**: Claude no longer drops `Message::system` when `system_instructions`
  is set, no longer ignores a requested response format, and no longer truncates
  silently at 1024 tokens.
- **genai**: `CompletionResponse::decode` accepts a JSON answer wrapped in a
  Markdown code fence and reports anything else clearly. API keys can no longer
  appear in errors, `Debug` output or logs; provider error bodies are size-capped.
- **genai** (router): chat requests are no longer routed to embedding models; a
  cancelled call or abandoned stream can no longer leave a model stuck half-open;
  client errors no longer open the circuit breaker.
- **genai** (router): `default_strategy`, `rules` and `fallback_depth` from the
  routing configuration are now applied; an unknown `disabled_capabilities` entry
  is an error.
- **vectorstore**: a failed first `upsert` of a mixed-dimension batch fixed the
  store's dimension; `search` cloned every document; cosine similarity overflowed
  or underflowed for extreme magnitudes; a poisoned lock panicked every later
  caller.
- **lifecycle**: a start that timed out, was cancelled or panicked left the
  component in `Starting`; a retry then reported success and started dependents on
  a component that never started. Such a start now ends in `Error`, and concurrent
  callers wait for the real outcome.
- **lifecycle**: `wait()` called after `stop_all` had finished never returned;
  `start_and_wait` did not return when `stop_all` was called from another task. A
  panic in a component's `start` or `stop` is contained and reported as
  `LifecycleError::ComponentPanicked`.
- **messaging**: a panicking handler killed its consumer and lost the buffered
  messages; it is now treated as `Ack::Retry` and the consumer keeps running.
- **messaging**: `stop()` returned immediately and dropped buffered and retried
  messages; consumer tasks leaked when a `LocalProvider` was dropped without
  `stop()`; `buffer_size: 0` panicked and is now an `InvalidOptions` error.
- **vfs**: `read_range` with a huge `len` panicked ("capacity overflow") or
  zero-filled that much memory; it is now bounded by the file.
- **fsutils**: the hand-declared `mmap` FFI used the wrong `off_t` width on 32-bit
  targets and assumed one `MAP_PRIVATE` value for all of Unix; large file lengths
  were truncated on 32-bit.
- **rt**: a bare `#[ferroly::rt::test]` did not compile in a crate without a
  direct `tokio` dependency, contrary to the 0.3.2 notes.
- **log**: a newline or control character in a field could forge a log record;
  JSON output could repeat `level` / `msg` / `ts`; `Logger::flush()` did not flush
  a synchronous writer; a context provider that logged recursed without bound.
- **metrics**: exposition output could be invalid (mixed kinds under one
  `# TYPE`, unvalidated names, a duplicated `le` label, duplicate bucket bounds);
  `observe(NaN)` poisoned `_sum`; `_count` could disagree with the `+Inf` bucket.
- **obs**: spans held across `.await` under `join!` / `select!` could swap fields
  and durations; span ids counted up from 1 in every process; the OTLP exporter
  buffered without limit, ignored the HTTP status, and `flush()` returned before
  anything was sent.
- **build**: `clippy -D warnings` passes with any single feature enabled, and
  examples and tests build under any feature selection.
- **docs**: the crate-level documentation shows the current version and lists
  every module.

### Security

This release fixes defects that are exploitable when the affected module handles
untrusted input. If you run any of these, upgrade:

- **HTTP request smuggling** (`http` server): a second `Transfer-Encoding`
  header, or a coding other than a lone `chunked`, was read as a zero-length body
  and the rest of the stream parsed as further requests.
- **Request splitting** (`http` client, `rest` client): CR/LF and spaces in URLs,
  methods and `path_param` values reached the request line.
- **Authorization bypass** (`turbo`): group-scoped filters, such as an auth
  filter on `/api`, were skipped for `//api/x`, `api/x` and dot-segment paths,
  and never ran for groups with a parameter in their prefix.
- **Remotely triggerable panics and aborts** (`codec`, reachable through
  `turbo` and `rest` content-type dispatch): TOML `\u` escapes followed by a
  multibyte character, unterminated XML `<?` / `<!--`, and deeply dotted TOML keys
  (stack overflow).
- **Resource exhaustion**: unbounded chunk-size lines and trailers, write paths
  with no timeout, and quadratic XML / TOML parsing (`http`, `codec`); unbounded
  WebSocket send queues and ping floods (`ws`); unbounded rate-limit tables and
  metric label sets (`turbo`); an unbounded OTLP export queue (`obs`).
- **Response mix-up** (`http` client): `1xx` interim responses were taken as
  final, so a later request on the same pooled connection could receive another
  request's body.
- **Token malleability** (`auth`): non-canonical base64url signatures verified.
- **Log forging** (`log`): newlines in field values could forge records.
- **Credential exposure**: auth providers and `ConfigBuilder` printed secrets in
  `Debug` output.
- **Unsound `unsafe`** (`fsutils`): the safe `Mmap::open` exposed a mapping that
  another process could change or truncate; the mapping is now behind
  `unsafe fn Mmap::map`, and the FFI signature was wrong on 32-bit targets.
- **Dependency advisories**: `Cargo.lock` moves to rustls 0.23.46,
  rustls-webpki 0.103.17, tokio-rustls 0.26.6, webpki-roots 1.0.12 and tokio
  1.53.3, clearing RUSTSEC-2026-0285, -0346, -0348, -0349, -0381, -0382 and
  -0386. These are lockfile changes only, so downstream crates resolve their
  own versions: run `cargo update -p rustls -p rustls-webpki -p webpki-roots -p tokio`.
  tokio stays on 1.53 because 1.54 requires Rust 1.85.

## [0.3.2]

### Added
- **`rt` signal handling** (`ferroly::rt::signal`) — `ctrl_c` on all platforms and,
  on Unix, `signal::unix::{signal, Signal, SignalKind}` for graceful-shutdown
  listeners (SIGINT/SIGTERM/…).
- **`rt` runtime construction** (`ferroly::rt::runtime`) — `Builder` and `Runtime`,
  so a binary that owns its reactor builds it through ferroly without a direct
  `tokio` dependency.
- **`rt` async test attribute** (`#[ferroly::rt::test]`) — re-exported
  `#[tokio::test]`, so consumer test suites need no direct `tokio` dev-dependency.

  Together these close the last reasons a consumer would declare `tokio` directly,
  completing the `rt` module's "single async dependency" promise.

### Changed
- **Dependency updates** (supersedes the open Dependabot PRs #29, #30, #34, #35,
  #36, #37, #38, #39):
  - `syn` `2` → `3` (`Cargo.toml` workspace dependency; build-time only).
  - `tokio` 1.52.3 → 1.53.1, `rustls` 0.23.41 → 0.23.43,
    `rustls-pki-types` 1.15.0 → 1.15.1, `webpki-roots` 1.0.8 → 1.0.9,
    `proc-macro2` 1.0.106 → 1.0.107, `quote` 1.0.46 → 1.0.47 (lockfile).
  - `github/codeql-action` `v4` → `v4.37.6` (CodeQL workflow; superseded
    Dependabot PR #39 and the follow-up bump in #43 / issue #42).
  - `zeroize`, `cc`, and `find-msvc-tools` held at their current versions
    (`1.8.1` / `1.2.65` / `0.1.9`) — newer releases require `edition2024`,
    which would break the MSRV 1.75 guarantee.

### Fixed
- `codec::toml` — `radix_int` rewritten to satisfy `clippy::question_mark`
  (new in clippy 1.97).

## [0.3.1]

### Changed
- `CONTRIBUTING.md` now documents the **versioning & release contract** enforced by
  the `version-guard` job: bump the version in `Cargo.toml`, keep the README badge
  in sync, add a matching `## [<version>]` CHANGELOG entry, and never tag manually
  (the `v<version>` tag is created automatically on merge to `main`).
- `CONTRIBUTING.md` documents two doc-comment rules that clippy enforces at
  `-D warnings`: indent rustdoc examples with **spaces, not tabs**, and rely on the
  implicit doctest `main` rather than an explicit `# fn main() { … }`.

## [0.3.0]

### Added
- **`genai` model router** (`ferroly::genai::router`) — a selection layer above
  `GenAiProvider`: describe a `Task` (required capabilities, budget, priority)
  and the router picks `(provider, model)` from a merged `ModelRegistry`, clamps
  options, dispatches the provider's `complete`/`complete_stream`, and fails over
  across models on transient errors (composing `clients::RetryPolicy` and
  `CircuitBreaker`). Includes `CapabilityStrategy` / `RuleBasedStrategy` /
  `CompositeStrategy`, a YAML policy overlay (`RoutingConfig`), a side-effect-free
  `resolve` + `RouteDecision` trace, and a parallel `EmbeddingRouter`. Providers
  advertise per-model `ModelInfo` via a new defaulted `GenAiProvider::model_catalog()`.
- `Capability` gains `Text`, `Chat`, `Audio`, `Embeddings`, and `Reasoning`
  variants and is now `#[non_exhaustive]`.

### Changed
- CI: a `v<version>` tag is created automatically on merge to `main`, and the
  version-guard now also requires a matching `## [<version>]` entry in
  `CHANGELOG.md` (closes #21).

## [0.2.0] 

### Added
- **`codec::toml`** — a hand-rolled, dependency-free TOML codec (streaming parse
  + encode) covering the common config subset; `Format::Toml` content-type
  dispatch, plus `application/yml` / `text/yml` YAML aliases (in the codec
  registry and the `turbo` router's content negotiation, which now also offers
  TOML).
- **`hash` module** — public **streaming** `Sha256` / `Sha1` / `HmacSha256` with
  a `Digest<N>` type (hex, constant-time `ct_eq`) and one-shot helpers. `auth`
  and `ws` now delegate their SHA to it (single implementation).
- **`rt` module** — a curated async-runtime surface re-exporting the `tokio`
  primitives Ferroly uses (spawn, `select!`, channels, sync, time, net, io), so
  consumers don't add `tokio` themselves.
- **`http::sse`** — structured Server-Sent Events: an `Event`
  (`id`/`event`/`data`/`retry`/`comment`) with spec-correct framing, an
  incremental client-side `SseDecoder`, and `HttpResponse::sse`.
- **`fsutils::Mmap`** — read-only memory-mapped files (`Deref<[u8]>`,
  `Send + Sync`); a true OS mapping on Unix, with an in-memory fallback on
  non-Unix behind the same API.
- **HTTP range & resumable downloads** — `RequestBuilder::range` and
  `http::download_to_file` (resumes a partial file via a `Range` request), plus
  `StatusCode::PARTIAL_CONTENT` / `RANGE_NOT_SATISFIABLE`.
- **`cli` module** — a builder-based command-line parser: subcommands, typed
  flags/options, positional args, environment-variable fallback, and generated
  `--help`.
- **`obs` module** — distributed span/event tracing with a shared `trace_id`,
  typed fields, a `Level` filter, a pluggable `Exporter` trait, a built-in
  `JsonExporter`, and an `OtlpHttpExporter` (OTLP/HTTP+JSON, with `http`).
- The `#[derive(FerrolyError)]` macro is re-exported at the **crate root**
  (usable with only `errutils`, without `codec`).
- **`metrics` module** — a dependency-free metrics registry (`Counter`, `Gauge`,
  `Histogram`) with Prometheus text exposition and a process-global registry.
  The `turbo` router gains `Router::metrics()` (RED middleware) and
  `Router::metrics_route("/metrics")`.
- **Server-side TLS** — `http::serve_tls` / `serve_tls_with_config` with an
  opaque `TlsConfig` built from PEM (`TlsConfig::from_pem`) or DER
  (`TlsConfig::from_der`).
- **HTTP keep-alive & client connection pooling** — the server serves multiple
  requests per connection; the `Client` pools and reuses idle connections per
  host, with transparent retry on a stale pooled connection.
- **Graceful shutdown & limits** — `http::ServerConfig`
  (`max_body_bytes`, `max_connections`, `head_timeout`, `body_timeout`,
  `drain_timeout`, `max_keep_alive_requests`), in-flight connection draining,
  and `HeaderMap::content_length_checked`.
- **`ws::WsServer`** — the WebSocket server as a lifecycle `Component` with
  graceful shutdown and default frame/message size caps; `server::serve` now
  applies safe defaults, with `serve_with_options` to override.
- **Lifecycle** — `ComponentManager::stop_all_with_timeout` and
  `DEFAULT_STOP_TIMEOUT` for bounded, best-effort shutdown.
- **Auth** — `JwtError::NotYetValid`; `nbf` (not-before) enforcement and
  fractional `exp`/`nbf` support.
- API-stability attributes: `#[non_exhaustive]` on public error enums and
  `#[must_use]` on builder types; the top-level `Error` now aggregates every
  enabled module's error.

### Changed
- Workspace lint `unsafe_code` relaxed from `forbid` to `deny` to permit a
  single audited `#[allow(unsafe_code)]` region — the `fsutils::Mmap` POSIX
  `mmap`/`munmap` FFI. The crate is otherwise `unsafe`-free.
- Parsers (JSON/XML/YAML) now enforce a recursion-depth cap (128) and reject
  malformed input without panicking.
- The HTTP server rejects request-smuggling framings (conflicting `Content-Length`
  / `Transfer-Encoding`) and decodes chunked request bodies.
- The rate-limiter bucket map now evicts idle buckets (bounded memory).
- The `#[derive(Encode)]`/`#[derive(Decode)]` macros now reject unknown or
  malformed `#[ferroly(...)]` attributes at compile time.

### Fixed
- JSON parser no longer panics on a malformed UTF-16 surrogate pair.
- XML 0/1-element arrays and YAML empty `{}`/`[]` now round-trip.
- Vector-store search ranks non-finite (NaN) scores last and breaks ties
  deterministically.
- Messaging redelivery/dead-letter no longer silently drops messages under
  backpressure; `Observer::on_dead_letter` reports the outcome.
- Duplicate object keys resolve last-wins consistently (`Value::get` and struct
  decoding agree).
- `HeaderMap::set`/`append` strip CR/LF to prevent header/response splitting.
- `Config`'s `Debug` redacts values (no secret leakage); env variables merge in
  deterministic sorted order; `Config::default()` equals `Config::new()`.
- `HealthRegistry::to_json` runs each check exactly once, escapes names, and runs
  checks without holding the lock.
- `Config`, messaging, and lifecycle robustness: atomic component start
  (no double-init race), dead-consumer pruning, `impl Drop for WsClient`.

### Added (P3)
- `http` server honors `Expect: 100-continue`; caps client chunk sizes and SSE
  line buffers.
- `rest::ServerBuilder::health_endpoints_split` for separate liveness/readiness
  registries; retries are gated to idempotent methods.
- `Logger::flush` to drain the async appender before exit.
- `#[non_exhaustive]` on error enums, `#[must_use]` on builders, and a completed
  top-level `Error` aggregator.

## [0.1.0]

- Initial release: a self-contained, dependency-minimal Rust toolkit (codec,
  config, genai, http, turbo, rest, ws, clients, lifecycle, messaging, vfs,
  vectorstore, auth, log).
