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

<!-- ADDED -->
- **fsutils**: `write_atomic` / `AtomicFile` (temporary sibling, fsync, rename);
  `TempDir` / `TempFile` (unique names, removed on drop); `LockFile`, an advisory
  create-new lock file; `Mmap::map`, `Mmap::map_supported`, `Mmap::is_mapped`.
  Far broader content-type coverage: Office, AVIF/HEIC, JSONL, Parquet, protobuf,
  fonts and archives by extension; mp4/`ftyp` family, RIFF variants, tar, zstd,
  xz, bzip2, 7z, fonts and text/HTML/XML/JSON by content.
- **vfs**: `LocalFs::rooted(dir)` confines every path to one directory (`..`,
  absolute paths and escaping symlinks are rejected). `MemoryFs` in-memory
  backend. `VfsRegistry` routes URLs to backends by scheme and copies across them.
  `Vfs::create` returns a streaming `VfsWriter` published atomically on `finish`.
  `Vfs::append`, `Vfs::list_page`, `copy_between`, `glob` / `glob_match`,
  `checksum_sha256` (with `hash`). `Metadata` reports modified and created times,
  read-only, Unix mode and symlinks.
- **rt**: `CancellationToken`, `TaskTracker`, `shutdown_signal()`. Declarative
  `async_test!` / `async_main!` that need no `tokio` dependency.
  `#[ferroly::rt::main]`. Re-exports for `fs`, `process`, `task_local!`,
  `LocalSet` / `spawn_local`, `AbortHandle`, `UdpSocket`, Unix sockets,
  `OnceCell`, `Barrier`, `runtime::Handle`, `MissedTickBehavior`,
  `io::{split, duplex}`.
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
- **errutils**: `.context("…")` / `.with_context(|| …)` on any `Result` or `Option`
  (`Context` trait, `ContextError`), keeping the original error as `source()`;
  `chain()` to walk an error chain.
- **errutils**: `Classify` trait and `ErrorKind` (`is_retryable()`,
  `is_user_error()`, `http_status()`), implemented for `std::io::Error`,
  `ContextError`, `MultiError`, `ConfigError`, `CliError` and `ferroly::Error`.
- **errutils**: `MultiError` implements `IntoIterator`, `FromIterator`,
  `From<Vec<_>>` and a generic `Extend`, and gains `iter()`, `first()` and
  `into_errors()`.
- **`ferroly::Error`**: `?` now works for `vfs::VfsError`, `cli::CliError`,
  `genai::RouterError`, `rt::JoinError`, `errutils::MultiError` and
  `errutils::ContextError`.
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

### Changed

<!-- CHANGED -->
- **fsutils (breaking)**: `Mmap::open` now reads the file into memory and is sound
  against concurrent changes to the file. The zero-copy OS mapping is the new
  `unsafe fn Mmap::map`, and is a real mapping only on 64-bit Linux, Android,
  macOS, iOS and the BSDs. `sniff` and `detect_content_type` now recognize text
  (plain, JSON, HTML, XML) where they returned nothing.
- **vfs (breaking)**: `LocalFs` rejects URL schemes other than `file://` instead
  of stripping them. `write` replaces files atomically and refuses read-only
  destinations. `Metadata` is `#[non_exhaustive]` — build it with
  `Metadata::file` / `Metadata::dir`. `LocalFs` is no longer a unit struct (use
  `LocalFs::new()`). Directory/file mismatches are reported as `IsDir` / `NotDir`
  on every platform.
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
- **errutils**: `MultiError::source()` returns the first collected error.
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

### Fixed

<!-- FIXED -->
- **fsutils**: the hand-declared `mmap` FFI used the wrong `off_t` width on 32-bit
  targets and assumed one `MAP_PRIVATE` value for all of Unix; large file lengths
  were truncated on 32-bit.
- **vfs**: `read_range` with a huge `len` panicked ("capacity overflow") or
  zero-filled that much memory; it is now bounded by the file.
- **vectorstore**: a failed first `upsert` of a mixed-dimension batch fixed the
  store's dimension; `search` cloned every document; cosine similarity overflowed
  or underflowed for extreme magnitudes; a poisoned lock panicked every later
  caller.
- **rt**: a bare `#[ferroly::rt::test]` did not compile in a crate without a
  direct `tokio` dependency, contrary to the 0.3.2 notes.
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
- **derive**: raw identifiers no longer leak into keys — a field named `r#type`
  encodes and decodes as `"type"` (was `"r#type"`).
- **derive**: `#[derive(FerrolyError)]` on a struct with `#[source]`, `#[from]` or
  `#[error(transparent)]` now compiles, and `#[from]` on a struct generates the
  `From` impl. `#[derive(Encode)]` on an enum with no variants compiles.
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
- **docs**: the crate-level documentation shows the current version and lists
  every module.

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
