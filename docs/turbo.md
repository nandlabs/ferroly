# ferroly::turbo

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `turbo` (implies [`http`](http.md) + [`codec`](codec.md)). Some helpers are
further gated on `log` and `auth`.

A **first-class, in-house HTTP router and server**, built directly over
[`ferroly::http`](http.md), *not* a thin wrapper around another web framework:
path-parameter routing, query parsing with typed accessors, content-negotiated
responses, before-routing filters, an authenticator hook, route groups, a token-bucket rate
limiter, and an **onion middleware** chain in which each layer runs code before *and* after the
inner handler. There is no `axum`/`actix`/`warp` underneath.

A `Router` is built fluently, then finalized into an [`HttpHandler`](http.md#httphandler-trait)
with [`into_handler`](#into_handler--serve) (to embed in [`rest`](rest.md)'s server) or served
directly with [`serve`](#into_handler--serve).

---

## Overview

| Concern | API |
|---|---|
| Per-request context | [`Ctx`](#ctx-the-request-context) — path/query/header/body accessors, `read`/`respond` |
| Typed extraction | [`param_as`](#path-parameters), [`query_int`/`query_float`/`query_bool`/`query_as`](#query-parameters) |
| Content negotiation | [`Ctx::respond`](#content-negotiated-responses-respond) (JSON / XML / YAML → 406) |
| Route registration | [`get`/`post`/`put`/`delete`/`patch`/`head`/`options`/`add`](#registering-routes) |
| Route groups | [`group`](#route-groups) + group-scoped filters |
| Before-routing hooks | [`filter`](#filters), [`auth`](#authenticator) |
| Onion middleware | [`layer`](#onion-middleware-layer--next) + [`Next`](#onion-middleware-layer--next) |
| Rate limiting | [`rate_limit`](#rate-limiting-rate_limit) (token bucket) |
| Slash policy | [`strict_slash`](#strict-slash) |
| Fallbacks | [`on_not_found`](#custom-fallbacks--the-allow-header) / [`on_method_not_allowed`](#custom-fallbacks--the-allow-header) + RFC-9110 `Allow` |
| Finalize | [`into_handler`](#into_handler--serve) / [`serve`](#into_handler--serve) |
| Feature-gated | [`access_log`](#access_log-log-feature), [`trace_context`](#trace_context-log-feature), [`jwt_auth`](#jwt_auth-auth-feature), [`metrics`/`metrics_route`](#metrics--metrics_route-metrics-feature) |
| Errors | [`ParamError`](#error-handling) |

```rust
use ferroly::turbo::{Router, Group, Ctx, Next, ParamError};
```

---

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["turbo"] }
tokio = { version = "1", features = ["full"] }

# Optional helpers:
# features = ["turbo", "log"]      # enables access_log + trace_context
# features = ["turbo", "auth"]     # enables jwt_auth
# features = ["turbo", "metrics"]  # enables metrics + metrics_route
```

`turbo` is also implied by the `rest` feature.

---

## Quick start

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

#[tokio::main]
async fn main() -> Result<(), ferroly::http::HttpError> {
    let router = Router::new()
        .get("/greet/:name", |ctx| async move {
            let name = ctx.param("name").unwrap_or("world").to_string();
            HttpResponse::text(StatusCode::OK, format!("hi {name}"))
        });
    router.serve("127.0.0.1:8080").await
}
```

Every handler is an `async` closure (or `fn`) taking a [`Ctx`](#ctx-the-request-context) and
returning an [`HttpResponse`](http.md#httpresponse). The closure must be `Send + Sync + 'static`
and its future `Send + 'static` (so no `!Send` state held across `.await`).

---

## `Ctx`: the request context

```rust
pub struct Ctx { /* request + captured path params */ }
```

The handle passed to every handler. It owns the parsed [`Request`](http.md#request--requestbuilder)
plus the path parameters captured by the matched route.

### Request info

| Method | Signature | Description |
|---|---|---|
| `method` | `fn method(&self) -> &Method` | The request method. |
| `path` | `fn path(&self) -> &str` | The request path. |
| `header` | `fn header(&self, name: &str) -> Option<&str>` | A request header (case-insensitive). |
| `body` | `fn body(&self) -> &[u8]` | Raw request body bytes. |
| `into_request` | `fn into_request(self) -> Request` | Consumes the ctx, returns the underlying request. |

```rust
use ferroly::http::{HttpResponse, StatusCode};
# use ferroly::turbo::Ctx;
# async fn h(ctx: Ctx) -> HttpResponse {
let ua = ctx.header("user-agent").unwrap_or("unknown").to_string();
let is_post = *ctx.method() == ferroly::http::Method::Post;
HttpResponse::text(StatusCode::OK, format!("{ua} post={is_post}"))
# }
```

### Path parameters

Patterns use `:name` segments; captured values are strings, optionally parsed.

| Method | Signature | Description |
|---|---|---|
| `param` | `fn param(&self, name: &str) -> Option<&str>` | Raw captured value. |
| `param_as` | `fn param_as<T: FromStr>(&self, name: &str) -> Result<T, ParamError>` | Parsed into `T`, erroring if missing/invalid. |

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new().get("/items/:id", |ctx| async move {
    match ctx.param_as::<u64>("id") {
        Ok(id) => HttpResponse::text(StatusCode::OK, format!("item {id}")),
        Err(e) => HttpResponse::text(StatusCode::BAD_REQUEST, e.to_string()),
    }
});
```

`param_as::<T>` returns [`ParamError::Missing`](#error-handling) when the parameter is absent
and `ParamError::Invalid { name, reason }` when it's present but `T::from_str` fails.

### Query parameters

The query string is parsed on demand (values are **percent-decoded**, and `+` becomes a space).

| Method | Signature | Description |
|---|---|---|
| `query_param` | `fn query_param(&self, name: &str) -> Option<String>` | Percent-decoded raw value. |
| `query_int` | `fn query_int(&self, name: &str) -> Option<i64>` | Parsed integer, `None` if absent/unparseable. |
| `query_float` | `fn query_float(&self, name: &str) -> Option<f64>` | Parsed float, `None` if absent/unparseable. |
| `query_bool` | `fn query_bool(&self, name: &str) -> Option<bool>` | `true`/`1`/`yes`/`on` and `false`/`0`/`no`/`off` (case-insensitive). |
| `query_as` | `fn query_as<T: FromStr>(&self, name: &str) -> Result<T, ParamError>` | Parsed into `T`, erroring like `param_as`. |

The `query_int`/`query_float`/`query_bool` accessors are **lenient** — they return `None`
(rather than an error) when the parameter is missing or doesn't parse — so they suit optional,
defaulted query knobs. Use `query_as` when a bad value should be a hard error.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

// GET /search?q=hi+there&page=2&limit=50&verbose=on
let _router = Router::new().get("/search", |ctx| async move {
    let q = ctx.query_param("q").unwrap_or_default();     // "hi there" (decoded)
    let page = ctx.query_int("page").unwrap_or(1);        // 2
    let ratio = ctx.query_float("ratio").unwrap_or(1.0);  // default (absent)
    let verbose = ctx.query_bool("verbose").unwrap_or(false); // true

    // Strict variant — 400 on a bad value:
    let limit: u32 = match ctx.query_as("limit") {
        Ok(n) => n,
        Err(e) => return HttpResponse::text(StatusCode::BAD_REQUEST, e.to_string()),
    };
    HttpResponse::text(
        StatusCode::OK,
        format!("q={q} page={page} ratio={ratio} verbose={verbose} limit={limit}"),
    )
});
```

### Decoding the body: `read`

```rust
fn read<T: Decode>(&self) -> Result<T, CodecError>;
```

Decodes the request body into `T`, selecting the codec from the `Content-Type` header
(defaulting to JSON when absent). Uses the [`ferroly::codec`](codec.md) `Decode` trait.

```rust
use ferroly::turbo::Router;
use ferroly::codec::Decode;
use ferroly::http::{HttpResponse, StatusCode};

#[derive(Decode)]
struct NewItem { name: String, qty: u32 }

let _router = Router::new().post("/items", |ctx| async move {
    match ctx.read::<NewItem>() {
        Ok(item) => HttpResponse::text(
            StatusCode::CREATED,
            format!("created {} x{}", item.name, item.qty),
        ),
        Err(e) => HttpResponse::text(StatusCode::BAD_REQUEST, format!("bad body: {e}")),
    }
});
```

### Content-negotiated responses: `respond`

```rust
fn respond<T: Encode>(&self, status: StatusCode, value: &T) -> HttpResponse;
```

Encodes `value` as the response body, **picking the codec from the request's `Accept`
header** — JSON, XML, or YAML — and setting `Content-Type` to match. The negotiation:

- `Accept` absent, empty, or `*/*` / `application/*` / `application/json` / `text/json` → **JSON**;
- `application/xml` / `text/xml` → **XML**;
- `application/yaml` / `text/yaml` / `application/x-yaml` → **YAML**;
- anything else supported by none of the above → **`406 Not Acceptable`**.

The first matching `Accept` entry wins (it splits on `,` and ignores `;q=` params). If encoding
itself fails it returns `500`.

```rust
use ferroly::turbo::Router;
use ferroly::codec::Encode;
use ferroly::http::StatusCode;

#[derive(Encode)]
struct Item { id: u64, name: String }

// Client sends `Accept: application/yaml` -> YAML body + `Content-Type: application/yaml`.
// Client sends `Accept: text/csv`         -> 406 Not Acceptable.
let _router = Router::new().get("/items/:id", |ctx| async move {
    let item = Item { id: 1, name: "widget".into() };
    ctx.respond(StatusCode::OK, &item)
});
```

`read` + `respond` together give you a codec-agnostic handler: accept JSON/XML/YAML in, return
whatever the caller asked for.

---

## `Router`

```rust
pub struct Router { /* … */ }
```

Built fluently — every registrar and configurator takes `self` and returns `Self`. Create one
with `Router::new()` (or `Router::default()`).

### Registering routes

Each verb registrar takes a path pattern and an async handler:

| Method | Registers |
|---|---|
| `get(path, handler)` | `GET` |
| `post(path, handler)` | `POST` |
| `put(path, handler)` | `PUT` |
| `delete(path, handler)` | `DELETE` |
| `patch(path, handler)` | `PATCH` |
| `head(path, handler)` | `HEAD` |
| `options(path, handler)` | `OPTIONS` |
| `add(path, handler, methods: Vec<Method>)` | one handler registered for **several** methods at once |

Signature (identical for all verbs):

```rust
fn get<F, Fut>(self, path: &str, handler: F) -> Router
where
    F: Fn(Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HttpResponse> + Send + 'static;
```

Patterns are `/`-separated; a segment beginning with `:` is a named parameter, everything else
is a literal. A route matches only when the number of segments is equal, so `/items/:id` does
not match `/items/42/detail`.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode, Method};

let _router = Router::new()
    .get("/health", |_| async { HttpResponse::text(StatusCode::OK, "ok") })
    .post("/items", |_| async { HttpResponse::text(StatusCode::CREATED, "made") })
    .put("/items/:id", |ctx| async move {
        HttpResponse::text(StatusCode::OK, format!("put {}", ctx.param("id").unwrap()))
    })
    .delete("/items/:id", |_| async { HttpResponse::new(StatusCode::NO_CONTENT) })
    .patch("/items/:id", |_| async { HttpResponse::ok() })
    .head("/items/:id", |_| async { HttpResponse::ok() })
    .options("/items", |_| async {
        HttpResponse::ok().header("allow", "GET, POST, OPTIONS")
    })
    // One handler for GET+POST:
    .add("/mirror", |_| async { HttpResponse::ok() }, vec![Method::Get, Method::Post]);
```

### Route groups

```rust
fn group(self, prefix: &str, build: impl FnOnce(Group) -> Group) -> Router;
```

Registers a batch of routes under a shared path prefix, with optional **group-scoped filters**.
The closure receives a [`Group`](#group) builder; its routes and filters are merged back into
the parent router.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new().group("/api/v1", |g| {
    g.filter(|ctx| {
            // Runs only for requests whose path starts with "/api/v1".
            if ctx.header("x-api-key").is_none() {
                Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "401 no key"))
            } else {
                None
            }
        })
        .get("/health", |_| async { HttpResponse::text(StatusCode::OK, "ok") })
        .post("/items", |_| async { HttpResponse::text(StatusCode::CREATED, "made") })
});
// Effective routes: GET /api/v1/health, POST /api/v1/items
```

#### `Group`

```rust
pub struct Group { /* prefix + routes + filters */ }
```

A `Group` supports the same registrars as the router for the common verbs — `get`, `post`,
`put`, `delete`, `patch` — each prefixing the path with the group's `prefix` (a trailing slash
on the prefix is trimmed). It also has its own `filter`:

| Method | Description |
|---|---|
| `get`/`post`/`put`/`delete`/`patch` | Register a route under the prefix. |
| `filter(f)` | A filter that runs **only** for requests whose path starts with the prefix. |

(Groups do not have `head`/`options`/`add`; register those on the router directly with the full
path.)

### Filters

```rust
fn filter<F>(self, f: F) -> Router
where F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static;
```

A filter runs **before routing** and may short-circuit: return `Some(response)` to stop and
send it, or `None` to continue. Filters run in registration order. They receive `&Ctx` (read
only) — use them for cross-cutting checks like API-key validation, CORS pre-checks, or
maintenance-mode gating.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new()
    .filter(|ctx| {
        if ctx.header("x-maintenance").is_some() {
            Some(HttpResponse::text(StatusCode::SERVICE_UNAVAILABLE, "503 down for maintenance"))
        } else {
            None
        }
    })
    .get("/", |_| async { HttpResponse::ok() });
```

### Authenticator

```rust
fn auth<F>(self, f: F) -> Router
where F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static;
```

Like a filter, but it runs **before all filters** — authentication is always checked first.
There is a single authenticator slot (a second `auth` call replaces the first).
Return `Some(401/403 response)` to reject, `None` to allow.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new()
    .auth(|ctx| match ctx.header("authorization") {
        Some(h) if h == "Bearer secret" => None,
        _ => Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "401 Unauthorized")),
    })
    .get("/private", |_| async { HttpResponse::text(StatusCode::OK, "secret data") });
```

### Onion middleware: `layer` + `Next`

```rust
fn layer<F, Fut>(self, middleware: F) -> Router
where
    F: Fn(Ctx, Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HttpResponse> + Send + 'static;
```

An **onion** middleware wraps the entire rest of the chain: it runs code **before and after**
the inner handler, and may inspect or modify the response.
Middlewares run in registration order — the first registered is the
**outermost** (runs first on the way in, last on the way out) — and they wrap the
authenticator, filters, routing, *and* the matched handler.

Inside a middleware you receive the request `Ctx` and a [`Next`](#next) continuation:

```rust
pub struct Next { /* … */ }
impl Next {
    pub fn run(self, ctx: Ctx) -> BoxFuture<'static, HttpResponse>;
}
```

Call `next.run(ctx).await` to invoke the remainder of the chain, or return an `HttpResponse`
**without** calling it to short-circuit (skipping everything inside, including the handler).

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new()
    // Outermost: timing + a response header.
    .layer(|ctx, next| async move {
        let start = std::time::Instant::now();
        let resp = next.run(ctx).await;                 // run the inner chain
        let ms = start.elapsed().as_millis();
        resp.header("X-Elapsed-Ms", ms.to_string())     // post-process
            .header("X-Powered-By", "ferroly")
    })
    // Inner: short-circuit example (never calls next.run).
    .layer(|ctx, next| async move {
        if ctx.path() == "/blocked" {
            return HttpResponse::text(StatusCode::FORBIDDEN, "403 blocked");
        }
        next.run(ctx).await
    })
    .get("/", |_| async { HttpResponse::text(StatusCode::OK, "ok") });
```

Ordering summary for a fully-configured router, from outside in:
**middlewares (in registration order) → authenticator → filters → routing → handler**, then
the response bubbles back out through each middleware in reverse.

### Rate limiting: `rate_limit`

```rust
fn rate_limit<K>(self, per_second: f64, burst: f64, key_of: K) -> Router
where K: Fn(&Ctx) -> String + Send + Sync + 'static;
```

Adds a **token-bucket** rate-limit filter. Requests are keyed by `key_of` (return a client IP,
an API key, a user id, …). Each key gets a bucket of `burst` tokens that refills at
`per_second` tokens per second; each request spends one token. When a key's bucket is empty the
request short-circuits with **`429 Too Many Requests`**. Buckets are kept in an internal
`Mutex<HashMap<..>>` shared across requests.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

// 5 requests/second sustained, bursts up to 10, keyed by X-Forwarded-For.
let _router = Router::new()
    .rate_limit(5.0, 10.0, |ctx| {
        ctx.header("x-forwarded-for").unwrap_or("anon").to_string()
    })
    .get("/api", |_| async { HttpResponse::text(StatusCode::OK, "ok") });
```

Because it is implemented as a `filter`, it runs before routing and after the authenticator.

**Bounded memory under rotating keys.** So the bucket map cannot grow without limit when keys
have high cardinality (per-IP under a botnet, rotating API keys, …), the filter sweeps every
60 seconds and drops any bucket that has refilled back to full. Evicting a full bucket is
lossless: a returning client is re-inserted with an identical full bucket, so eviction never
grants extra allowance.

### Strict slash

```rust
fn strict_slash(self, strict: bool) -> Router;
```

Controls trailing-slash matching. Default is `false` (**lenient**) — `/x` and `/x/` are treated
as the same route. With `strict_slash(true)`, a route registered as `/x` matches only `/x` and
one registered as `/x/` matches only `/x/`; the request's trailing slash must equal the
pattern's.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new()
    .strict_slash(true)
    .get("/items", |_| async { HttpResponse::text(StatusCode::OK, "no slash") })
    .get("/items/", |_| async { HttpResponse::text(StatusCode::OK, "with slash") });
// /items and /items/ now hit different handlers.
```

### Custom fallbacks & the `Allow` header

```rust
fn on_not_found<F, Fut>(self, handler: F) -> Router;         // custom 404
fn on_method_not_allowed<F, Fut>(self, handler: F) -> Router; // custom 405
```

When no route matches the **path**, the router returns `404 Not Found` (or your `on_not_found`
handler). When the path exists but not for the request **method**, it returns `405 Method Not
Allowed` (or your `on_method_not_allowed` handler).

Either way, on a 405 the router **always attaches an RFC-9110 `Allow` header** listing the
methods registered for that path — added even to your custom handler's response:

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new()
    .get("/items/:id", |_| async { HttpResponse::ok() })
    .delete("/items/:id", |_| async { HttpResponse::new(StatusCode::NO_CONTENT) })
    // POST /items/42  ->  405 with `Allow: GET, DELETE`
    .on_method_not_allowed(|_| async {
        HttpResponse::text(StatusCode::METHOD_NOT_ALLOWED, "nope")
        // the router still appends `Allow: GET, DELETE`
    })
    .on_not_found(|ctx| async move {
        HttpResponse::text(StatusCode::NOT_FOUND, format!("no route for {}", ctx.path()))
    });
```

### `into_handler` / `serve`

```rust
fn into_handler(self) -> Arc<dyn HttpHandler>;
async fn serve(self, addr: impl ToSocketAddrs) -> Result<(), HttpError>;
```

- `into_handler` finalizes the router into a shareable [`HttpHandler`](http.md#httphandler-trait)
  — pass it to [`http::serve`](http.md#server-serve) yourself (e.g. for custom shutdown), or
  embed it in [`ferroly::rest`](rest.md)'s server component.
- `serve` is the batteries-included path: it binds `addr` and serves **until the process ends**
  (its shutdown future is `std::future::pending`).

```rust
use std::sync::Arc;
use ferroly::turbo::Router;
use ferroly::http::{serve, HttpResponse, StatusCode};
use tokio::net::TcpListener;

# async fn ex() -> Result<(), ferroly::http::HttpError> {
let router = Router::new().get("/", |_| async { HttpResponse::text(StatusCode::OK, "hi") });

// One-liner:
// router.serve("127.0.0.1:8080").await

// Or with your own listener + graceful shutdown:
let handler = router.into_handler();
let listener = TcpListener::bind("127.0.0.1:8080").await?;
serve(listener, handler, async { let _ = tokio::signal::ctrl_c().await; }).await
# }
```

---

## Feature-gated helpers

### `access_log` (`log` feature)

```rust
#[cfg(feature = "log")]
fn access_log(self, logger: ferroly::log::Logger) -> Router;
```

Adds an access-log middleware: one [`Level::Info`](log.md) line per request — with `method`,
`path`, `status`, and `ms` (duration) fields — emitted via the given
[`ferroly::log::Logger`](log.md). It is implemented as a `layer`, so it wraps the full chain and
times the whole request.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

# #[cfg(feature = "log")]
# fn ex(logger: ferroly::log::Logger) {
let _router = Router::new()
    .access_log(logger)
    .get("/", |_| async { HttpResponse::text(StatusCode::OK, "ok") });
// logs e.g.  request  method=GET path=/ status=200 ms=1
# }
```

### `trace_context` (`log` feature)

```rust
#[cfg(feature = "log")]
fn trace_context(self) -> Router;
```

Wraps each request in a logging context carrying a **`trace_id`**, so that **every**
[`ferroly::log`](log.md) record emitted while handling that request automatically includes it —
request correlation *without* threading a logger through your call graph.

How it works:

1. The `trace_id` is taken from the incoming `x-request-id` header, or generated
   (`<nanos>-<counter>` in hex) when absent.
2. `trace_context` registers a **log context provider** exactly once (guarded by a
   `std::sync::Once`) via `ferroly::log::add_context_provider`. That provider reads a
   **`tokio` task-local** holding the current request's fields.
3. The middleware sets that task-local (`LOG_FIELDS`) for the scope of `next.run(ctx)` using
   `task_local!`'s `.scope(...)`. Since the whole downstream chain — filters, handler, and any
   nested `.await`s that stay on the task — runs inside that scope, every `ferroly::log` call
   picks up `trace_id` from the provider and stamps it on the record.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

# #[cfg(feature = "log")]
# fn ex(logger: ferroly::log::Logger) {
let _router = Router::new()
    .trace_context()      // establish per-request trace_id
    .access_log(logger)   // its log line, and any log call in a handler, carries trace_id
    .get("/work", |_| async {
        ferroly::log::info("doing work", &[]); // record automatically includes trace_id
        HttpResponse::text(StatusCode::OK, "done")
    });
# }
```

Pair it with [`access_log`](#access_log-log-feature) or the global logger so the correlated
records actually go somewhere.

### `jwt_auth` (`auth` feature)

```rust
#[cfg(feature = "auth")]
fn jwt_auth(self, secret: impl Into<Vec<u8>>) -> Router;
```

Installs an HS256-JWT authenticator (built on [`auth`](#authenticator), so it runs first). It
requires a valid `Authorization: Bearer <token>` (the `Bearer`/`bearer` prefix, case-insensitive
on the scheme) whose signature verifies against `secret` using HS256; otherwise it responds
`401` — distinguishing "invalid token" from "missing bearer token". Verification uses
[`ferroly::auth::decode_hs256`](auth.md).

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

# #[cfg(feature = "auth")]
let _router = Router::new()
    .jwt_auth("my-signing-secret")
    .get("/me", |_| async { HttpResponse::text(StatusCode::OK, "authenticated") });
```

The claims are validated but not injected into `Ctx`; a handler that needs the claims can
re-verify the bearer token with [`ferroly::auth::decode_hs256`](auth.md) itself.

### `metrics` / `metrics_route` (`metrics` feature)

```rust
#[cfg(feature = "metrics")]
fn metrics(self) -> Router;
#[cfg(feature = "metrics")]
fn metrics_route(self, path: &str) -> Router;
```

`metrics()` installs a middleware that records **RED** (Rate, Errors, Duration) metrics for
every request into the process-global [`metrics::Registry`](metrics.md). Because it is a
`layer`, it wraps and times the whole chain. It emits three series:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `http_requests_total` | counter | `method`, `status` | Total requests, split by verb and status code. |
| `http_request_duration_seconds` | histogram | `method` | Request latency, bucketed with the registry's `DEFAULT_BUCKETS`. |
| `http_requests_in_flight` | gauge | — | Requests currently being handled (incremented on entry, decremented on exit). |

`metrics_route(path)` registers a `GET` route at `path` that renders the whole global registry
in the **Prometheus text exposition format** (`Content-Type: text/plain; version=0.0.4`), so a
scraper can pull it. Use the two together — one to record, one to expose:

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

# #[cfg(feature = "metrics")]
let _router = Router::new()
    .metrics()                    // record RED metrics for every route below
    .metrics_route("/metrics")    // GET /metrics -> Prometheus exposition text
    .get("/", |_| async { HttpResponse::text(StatusCode::OK, "ok") });
// GET /metrics now returns e.g.
//   http_requests_total{method="GET",status="200"} 1
//   http_requests_in_flight 0
```

Register `metrics()` **first** (outermost) so its timer spans every other layer, and note that
the registry is process-global — all routers and any direct [`ferroly::metrics`](metrics.md)
calls share it, so `/metrics` reflects the whole process.

---

## Error handling

Handlers return an [`HttpResponse`](http.md#httpresponse) directly — routing never fails, it
just produces a `404`/`405`. The one error type is `ParamError`, from the typed extractors
`param_as` and `query_as`:

```rust
pub enum ParamError {
    Missing(String),                          // "missing parameter: {0}"
    Invalid { name: String, reason: String }, // "invalid parameter '{name}': {reason}"
}
```

- `Missing(name)` — the parameter wasn't present.
- `Invalid { name, reason }` — it was present but `FromStr` rejected it (`reason` is the parse
  error's `Display`).

It derives `ferroly_derive::FerrolyError` (see [`derive`](derive.md)), so it implements
`Display` + `std::error::Error`. The usual pattern is to map it to a `400`:

```rust
use ferroly::turbo::{Ctx, ParamError};
use ferroly::http::{HttpResponse, StatusCode};

# async fn h(ctx: Ctx) -> HttpResponse {
match ctx.param_as::<u64>("id") {
    Ok(id) => HttpResponse::text(StatusCode::OK, format!("id {id}")),
    Err(ParamError::Missing(name)) => {
        HttpResponse::text(StatusCode::BAD_REQUEST, format!("missing {name}"))
    }
    Err(e @ ParamError::Invalid { .. }) => {
        HttpResponse::text(StatusCode::BAD_REQUEST, e.to_string())
    }
}
# }
```

Body decoding via [`Ctx::read`](#decoding-the-body-read) returns
[`codec::CodecError`](codec.md), and [`Ctx::respond`](#content-negotiated-responses-respond)
turns an unmatched `Accept` into `406` (and an encode failure into `500`) for you.

---

## Limitations

- **Linear route matching.** Routes are matched by scanning the registration list in order (no
  radix/trie); fine for typical route counts, not tuned for thousands of routes.
- **Exact-arity patterns.** A pattern matches only a path with the same number of segments;
  there are no wildcard/catch-all (`*`) or regex segments — only literals and `:name`.
- **First match wins** for a given method; if two routes could match the same path+method, the
  earlier-registered one is used.
- **Single authenticator slot** — a second `auth`/`jwt_auth` replaces the first.
- **Groups cover the common verbs** (`get`/`post`/`put`/`delete`/`patch` + `filter`); use the
  router directly for `head`/`options`/`add` on a prefixed path.
- **`respond` negotiates JSON/XML/YAML only** — other `Accept` types yield `406`.
- Inherits the [`http`](http.md#limitations) server characteristics: HTTP/1.1 with keep-alive
  (no pipelining or HTTP/2). Serve over TLS via [`http::serve_tls`](http.md#https-serve_tls--tlsconfig)
  with [`into_handler`](#into_handler--serve).

---

## See also

- [`ferroly::http`](http.md) — the HTTP/1.1 client + server this router is built on.
- [`ferroly::rest`](rest.md) — full client/server framework that embeds a `turbo` router.
- [`ferroly::codec`](codec.md) — the `Encode`/`Decode` traits behind `read`/`respond`.
- [`ferroly::log`](log.md) — logging used by `access_log`/`trace_context`.
- [`ferroly::auth`](auth.md) — HS256 JWT verification behind `jwt_auth`.
- [`ferroly::lifecycle`](lifecycle.md) — run a server as a managed `Component`.
- [`ferroly::derive`](derive.md) — the `FerrolyError` derive behind `ParamError`.
