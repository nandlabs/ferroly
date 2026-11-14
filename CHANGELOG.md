# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
crate is pre-1.0 (`0.x`), minor releases may contain breaking changes.

## [Unreleased]

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
