# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
crate is pre-1.0 (`0.x`), minor releases may contain breaking changes.

## [Unreleased]

### Added
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
