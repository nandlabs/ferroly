# ferroly::turbo

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `turbo` (implies [`http`](http.md) + [`codec`](codec.md)). Some helpers are
further gated on `auth`, `log`, `metrics`, `obs`, `errutils`, `fsutils` and `hash`; see
[Enabling](#enabling).

A **first-class, in-house HTTP router and server**, built directly over
[`ferroly::http`](http.md), *not* a thin wrapper around another web framework: routing with
path parameters and catch-alls, groups and mounted sub-routers, typed request extraction,
content-negotiated responses, synchronous and asynchronous filters, an authenticator hook,
an **onion middleware** chain, typed error responses, and a set of ready-made middleware —
CORS, security headers, request ids, CSRF, rate limiting, static files. There is no
`axum`/`actix`/`warp` underneath.

A `Router` is built fluently, then finalized into an [`HttpHandler`](http.md#httphandler-trait)
with [`into_handler`](#finalizing-and-serving) (to embed in [`rest`](rest.md)'s server) or
served directly with one of the [`serve`](#finalizing-and-serving) methods.

---

## Overview

| Concern | API |
|---|---|
| Per-request context | [`Ctx`](#ctx-the-request-context) — path / query / header / cookie / body accessors, connection metadata, extensions, state |
| Handlers and errors | [`IntoResponse`](#handlers-and-typed-errors), [`Problem`](#problem-rfc-9457) (RFC 9457), `Classified` |
| Route registration | [`get`/`post`/`put`/`delete`/`patch`/`head`/`options`/`add`/`route`](#registering-routes) |
| Patterns | [`:name` and `*name`](#path-patterns), most-specific-wins |
| Per-route options | [`Route`](#per-route-options-route) — middleware, filters, body limit, timeout |
| Structure | [`group`](#route-groups) (nestable), [`mount`](#mounting-a-sub-router) |
| Before-handler hooks | [`filter` / `filter_async`](#filters), [`auth` / `auth_async`](#authenticator) |
| Onion middleware | [`layer`](#onion-middleware-layer--next) + `Next` |
| Request state | [`ctx.set` / `ctx.get`](#extensions-and-application-state), [`with_state`](#extensions-and-application-state) |
| Limits | [`body_limit`, `request_timeout`](#body-limits-and-timeouts) |
| Automatic answers | [`HEAD` and `OPTIONS`](#automatic-head-and-options), `405` + `Allow` |
| Validation | [`validate` / `problems` / `try_into_handler`](#validating-the-route-table) |
| Middleware | [`cors`](#cors), [`security_headers`](#security-headers), [`request_id`](#request-ids), [`csrf`](#csrf-hash-feature), [`rate_limit_with`](#rate-limiting) |
| Bodies | [`form`](#forms-and-multipart), [`multipart`](#forms-and-multipart), [`validate`](#schema-validation), [cookies](#cookies) |
| Files | [`static_files`](#static-files) |
| Serving | [`into_handler`, `serve*`](#finalizing-and-serving) — config, shutdown, TLS |
| Feature-gated | [JWT](#jwt-authentication-auth-feature), [`access_log`](#access_log-log-feature), [`trace_context`](#trace_context-log-or-obs-feature), [`metrics`](#metrics--metrics_route-metrics-feature) |
| Errors | [`ParamError`, `RouteError`, `FormError`, `CookieError`](#error-handling) |

```rust
use ferroly::turbo::{
    Router, Group, Route, Ctx, Next, Extensions,
    IntoResponse, Problem, ParamError, RouteError,
    Cors, SecurityHeaders, FrameOptions, RequestId,
    RateLimit, TrustedProxies, ForwardedHeader,
    StaticFiles, SetCookie, SameSite, CookieError,
    Form, FormError, Part, MultipartLimits,
};
```

---

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["turbo"] }
tokio = { version = "1", features = ["full"] }
```

`turbo` is also implied by the `rest` feature. Optional features add methods:

| Feature | Adds |
|---|---|
| `auth` | [`jwt_auth`, `jwt_auth_with`, `jwt_auth_hmac_with`, `jwt_auth_jwks`, `jwt`, `JwtAuth`, `Claims`, `ctx.claims()`, scope / role guards](#jwt-authentication-auth-feature) |
| `log` | [`access_log`](#access_log-log-feature), [`trace_context`](#trace_context-log-or-obs-feature) (log correlation) |
| `obs` | [`trace_context`](#trace_context-log-or-obs-feature) (W3C trace context, server spans) |
| `metrics` | [`metrics`, `metrics_in`, `metrics_route`](#metrics--metrics_route-metrics-feature) |
| `errutils` | [`Classified`](#classified-errutils-feature) — render any `Classify` error |
| `hash` | [`csrf`](#csrf-hash-feature); cryptographically random request ids |
| `fsutils` | the full MIME table for [static files](#static-files) |

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
returning an [`HttpResponse`](http.md#httpresponse) — or anything else that implements
[`IntoResponse`](#handlers-and-typed-errors), such as `Result<HttpResponse, MyError>`. The
closure must be `Send + Sync + 'static` and its future `Send + 'static` (so no `!Send` state
held across `.await`).

---

## Security model

This section states what the router guarantees. The tests in
`crates/ferroly/tests/turbo_security.rs` exercise each rule over raw TCP.

### One canonical path

Before **anything** looks at a request — middleware, the authenticator, filters, routing,
handlers — its path is normalised once with
[`http::normalize_path`](http.md#url-and-date-helpers) and its query string is decoded.
Everything afterwards sees the same value (`ctx.path()`), so an access check and the router
can never disagree about which resource a request names.

| Request target | Result |
|---|---|
| `/api/../api/secret`, `/api/./secret` | dot segments resolved → `/api/secret` |
| `/%61pi/secret`, `/api/%2e%2e/api/secret` | escapes of unreserved characters decoded first, then as above → `/api/secret` |
| `/api%2Fsecret` | **kept** as one segment `api%2Fsecret` — an encoded `/` is never a separator (so this is a different path, usually `404`) |
| `//api/secret`, `/api//secret` | `400` — empty segment |
| `api/secret` | `400` — not an absolute path |
| `/api/sec%ZZret`, `/api/%` | `400` — malformed escape |
| `/api/secret%00` | `400` — encoded control character |
| `/q?a=%FF`, `/q?a=%zz` | `400` — query is not valid percent-encoded UTF-8 |
| `/files/%FF` (matching `/files/:name`) | `400` — parameter is not UTF-8 |

Rejected requests are answered `400 Bad Request` **before** any middleware runs (they are not
seen by `access_log` or `metrics`; use [`ServerConfig::on_error`](http.md#errors-and-panics-errorhook)
if you need to count them at the server).

**Case policy.** Paths are **case-sensitive**, as RFC 9110 specifies: `/API/secret` is not
`/api/secret`. It matches neither the route nor a group prefix `/api`, and is a `404`. If you
want case-insensitive URLs, redirect to the lower-case form in a [`layer`](#onion-middleware-layer--next).

**Trailing slash.** By default `/x` and `/x/` name the same route (and the same group scope);
[`strict_slash(true)`](#strict-slash) makes them distinct.

### Parameters are decoded after routing

Routing splits the normalised path on literal `/` and matches segment by segment. Only then
are `:name` and `*name` values percent-decoded. An encoded slash therefore ends up *inside*
a parameter value and can never change which route matched:

```text
GET /files/a%2Fb    with route /files/:name    →  ctx.param("name") == "a/b"
GET /files/a/b      with route /files/:name    →  404 (two segments)
```

A handler that turns a parameter into a filesystem path, a URL or a query must still treat
it as untrusted data — it may contain `/`, `..` or anything else. [`StaticFiles`](#static-files)
does this for files.

### Filter scoping

A [group](#route-groups) filter runs for every request whose path lies under the group's
prefix. "Under" is decided by **the same segment matcher that matches routes**, on the same
normalised path: literal prefix segments compare whole (`/api` does not cover `/apix`), and a
`:name` prefix segment matches any one segment (so a group `/t/:tenant` protects
`/t/acme/...`). There is no spelling of a path that reaches a group's route without matching
the group's prefix.

Two further properties:

- **Defence in depth.** Each route remembers the filters of every group it was registered in
  (and of a [mounted](#mounting-a-sub-router) router's authenticator and filters).
  Immediately before a handler is called, the router checks that all of them have run for
  this request and runs any that has not.
- **No route enumeration.** A group filter also runs for paths under the prefix that match no
  route, so an authentication filter answers `401` instead of revealing which paths exist.

A group or mount whose prefix is malformed registers **nothing** — its handlers are not
served unprotected somewhere else — and the mistake is reported by
[`validate`](#validating-the-route-table).

### Client addresses and proxies

`ctx.peer_addr()` is the address of the TCP peer. `X-Forwarded-For` and `Forwarded` are
ordinary request headers that any client can set, and **nothing in this module reads them
by default** — not the rate limiter, not the access log.

Behind load balancers or reverse proxies, declare them with
[`TrustedProxies`](#trusted-proxies). The forwarding header is then consulted only when the
direct peer is one of the declared proxies, and it is walked from the right (the entries the
proxies appended), stopping at the first address that is not a trusted proxy; entries further
left are client-controlled and never used.

### Limits

| Limit | Where | Default | On violation |
|---|---|---|---|
| Request head, body, connections, timeouts | [`ServerConfig`](http.md#resource-limits-serverconfig--serve_with_config) via [`serve_with_config`](#finalizing-and-serving) | see `http` | `4xx` / `503` |
| Body size per route / router | [`Route::body_limit`, `Router::body_limit`](#body-limits-and-timeouts) | none (server cap applies) | `413` |
| Handler time per route / router | [`Route::timeout`, `Router::request_timeout`](#body-limits-and-timeouts) | none | `503` |
| Multipart parts, part size, total size, part headers | [`MultipartLimits`](#forms-and-multipart) | 64 / 8 MiB / 16 MiB / 8 KiB | `413` |
| Rate-limit table size | [`RateLimit::max_keys`](#rate-limiting) | 10 000 keys | oldest evicted |
| Inbound request id | [`is_valid_request_id`](#request-ids) | 128 chars of `A-Z a-z 0-9 . _ -` | replaced |
| Metric label values | [`metrics`](#metrics--metrics_route-metrics-feature) | bounded sets | `OTHER` / `unmatched` |

### Other defaults worth knowing

- **Cookies** built with [`SetCookie::new`](#cookies) are `Secure`, `HttpOnly`, `SameSite=Lax`,
  `Path=/` unless you say otherwise, and `__Host-` / `__Secure-` prefix rules are enforced.
- **CORS** never sends `Access-Control-Allow-Origin: *` together with credentials.
- **JWT**: the new authenticators require `exp` and a secret of at least 16 bytes; a `401`
  never says why a token was rejected.
- **Error bodies** produced by [`Classified`](#classified-errutils-feature) carry the error
  message only for caller errors (4xx).
- **Static files** are confined to their root; see [Static files](#static-files).

---

## The request pipeline

```text
request
  │
  ├─ OPTIONS *                      → 204 + Allow (when auto_options)
  ├─ normalise path, decode query   → 400 if not canonicalisable
  ├─ resolve route                  → template, decoded params stored on Ctx
  │
  ├─ layer 1 ─ layer 2 ─ …            (router-wide onion middleware, in order)
  │     ├─ authenticator              (auth / auth_async / jwt)
  │     ├─ filters                    (router filters, group filters by prefix; in order)
  │     ├─ group guards re-checked
  │     ├─ body limit                 → 413
  │     ├─ group layers ─ route layers ─ route filters      ┐ under the
  │     │        └─ handler                                 ┘ route timeout → 503
  │     └─ (no route) 204 Allow / 405 Allow / 404
  └─ queued response headers applied
```

Because the route is resolved **before** the middleware chain starts, `ctx.route()` and
`ctx.param()` work inside layers and filters as well as handlers. A `404` or `405` is still
produced only after the authenticator and filters have had their say.

---

## `Ctx`: the request context

```rust
pub struct Ctx { /* request, route match, extensions, state */ }
```

The handle passed to every handler, filter and middleware.

### Request info

| Method | Signature | Description |
|---|---|---|
| `method` | `fn method(&self) -> &Method` | The request method. |
| `path` | `fn path(&self) -> &str` | The **normalised** request path (see [Security model](#one-canonical-path)). |
| `route` | `fn route(&self) -> Option<&str>` | The matched route **template** (`/users/:id`), or `None`. Use this — not `path` — as a metrics or span label. |
| `header` / `header_all` / `headers` | `fn header(&self, name: &str) -> Option<&str>` | Request headers (case-insensitive). |
| `content_type` | `fn content_type(&self) -> Option<String>` | The media type without parameters, lower-cased. |
| `body` | `fn body(&self) -> &[u8]` | Raw request body bytes. |
| `request` | `fn request(&self) -> &Request` | The underlying request. |
| `into_request` | `fn into_request(self) -> Request` | Consumes the ctx, returns the request (with the normalised path). |
| `from_request` | `fn from_request(request: Request) -> Result<Ctx, ParamError>` | Builds a context outside a router — for unit tests, or a middleware that rewrites the request. Normalises exactly like the router. |

### Connection metadata

| Method | Signature | Description |
|---|---|---|
| `peer_addr` | `fn peer_addr(&self) -> Option<SocketAddr>` | The connected peer (behind a proxy: the proxy). |
| `local_addr` | `fn local_addr(&self) -> Option<SocketAddr>` | The address the connection was accepted on. |
| `is_tls` | `fn is_tls(&self) -> bool` | Whether TLS was terminated by this server. |
| `peer_certificates` | `fn peer_certificates(&self) -> &[Vec<u8>]` | The client's verified chain (DER, leaf first) under mutual TLS. |
| `client_ip` | `fn client_ip(&self, proxies: &TrustedProxies) -> Option<IpAddr>` | The originating client per [`TrustedProxies`](#trusted-proxies). |

### Path parameters

| Method | Signature | Description |
|---|---|---|
| `param` | `fn param(&self, name: &str) -> Option<&str>` | A `:name` or `*name` value, percent-decoded. |
| `param_as` | `fn param_as<T: FromStr>(&self, name: &str) -> Result<T, ParamError>` | Parsed into `T`, erroring if missing/invalid. |
| `tail_segments` | `fn tail_segments(&self) -> &[String]` | The segments captured by a `*name` catch-all, each decoded on its own. |

```rust
use ferroly::turbo::{ParamError, Router};
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new().get("/items/:id", |ctx| async move {
    let id: u64 = ctx.param_as("id")?;          // ParamError -> 400 problem+json
    Ok::<_, ParamError>(HttpResponse::text(StatusCode::OK, format!("item {id}")))
});
```

`param_as::<T>` returns [`ParamError::Missing`](#error-handling) when the parameter is absent
and [`ParamError::Invalid`](#error-handling) when it does not parse.

### Query parameters

Keys and values are percent-decoded (`+` is a space) once, up front; a query that is not
valid percent-encoded UTF-8 is a `400` before any handler runs.

| Method | Signature | Description |
|---|---|---|
| `query_param` | `fn query_param(&self, name: &str) -> Option<String>` | First value for a key. |
| `query_all` | `fn query_all(&self, name: &str) -> impl Iterator<Item = &str>` | Every value for a key. |
| `query_pairs` | `fn query_pairs(&self) -> &[(String, String)]` | All decoded pairs, in order. |
| `raw_query` | `fn raw_query(&self) -> Option<&str>` | The query string as sent. |
| `query_int` / `query_float` / `query_bool` | `-> Option<i64>` / `Option<f64>` / `Option<bool>` | Typed, `None` if absent or unparseable. `query_bool` accepts `true/1/yes/on` and `false/0/no/off`. |
| `query_as` | `fn query_as<T: FromStr>(&self, name: &str) -> Result<T, ParamError>` | Parsed, erroring like `param_as`. |

### Decoding the body: `read`

```rust
use ferroly::codec::Decode;
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{Problem, Router};

#[derive(Decode)]
struct NewItem { name: String, qty: i64 }

let _router = Router::new().post("/items", |ctx| async move {
    let item: NewItem = ctx.read()?;                          // 400 / 415 problem+json
    Ok::<_, Problem>(HttpResponse::text(StatusCode::CREATED, format!("{}x{}", item.qty, item.name)))
});
```

| Method | Description |
|---|---|
| `read::<T>()` | Decodes with the codec named by the request's `Content-Type` (JSON when absent). The **client** picks the codec this way. |
| `read_as::<T>(content_type)` | Decodes with one fixed codec, whatever the request header says — use it to accept a single format. |

### Schema validation

`ctx.validate(&schema)` decodes the body into a [`Value`](codec.md) and checks it against a
schema ([`codec::schema`](codec.md), a JSON-Schema subset). `ctx.read_valid::<T>(&schema)`
validates and then decodes into `T`. Both return a [`Problem`](#problem-rfc-9457) on failure:

| Failure | Status | Body |
|---|---|---|
| Body cannot be decoded | `400` (`415` for an unknown content type) | problem document |
| Body decodes but breaks the schema | `422` | problem document with an `errors` array of `{ "path", "message" }`, one per violation |

```rust
use ferroly::codec::{json, Decode};
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{Ctx, Problem};

#[derive(Decode)]
struct Item { name: String, qty: i64 }

async fn create(ctx: Ctx) -> Result<HttpResponse, Problem> {
    let schema = json::from_str(
        r#"{"type":"object","required":["name","qty"],
            "properties":{"name":{"type":"string","minLength":1},
                          "qty":{"type":"integer","minimum":1}}}"#,
    ).expect("valid schema");
    let item: Item = ctx.read_valid(&schema)?;
    Ok(HttpResponse::text(StatusCode::CREATED, format!("{}x{}", item.qty, item.name)))
}
```

```json
{"type":"about:blank","title":"Unprocessable Entity","status":422,
 "detail":"the request body failed validation",
 "errors":[{"path":"/name","message":"..."},{"path":"/qty","message":"..."}]}
```

Parse the schema once (for example into [application state](#extensions-and-application-state))
rather than per request.

### Content-negotiated responses: `respond`

`ctx.respond(status, &value)` encodes `value` with the codec chosen from the request's
`Accept` header — JSON, XML, YAML or TOML; JSON when `Accept` is absent or `*/*` — and
answers `406 Not Acceptable` when nothing supported matches.

```rust
use ferroly::codec::Encode;
use ferroly::http::StatusCode;
use ferroly::turbo::Router;

#[derive(Encode)]
struct Item { name: String }

let _router = Router::new().get("/item", |ctx| async move {
    ctx.respond(StatusCode::OK, &Item { name: "widget".into() })
});
```

### Response headers from filters

A filter only sees `&Ctx`, yet sometimes needs to contribute a response header.
`ctx.set_response_header(name, value)` queues one; it is added to whatever response finally
leaves the router, unless that response already has a header of that name.
`ctx.append_response_header` always adds (for `Set-Cookie`, `Vary`). The
[rate limiter](#rate-limiting) uses this for its `RateLimit-*` headers.

### Extensions and application state

**Request-scoped** values pass from middleware and filters to handlers through a typed map:

| Method | Signature |
|---|---|
| `set` | `fn set<T: Send + Sync + 'static>(&self, value: T)` |
| `get` | `fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>>` |
| `remove` | `fn remove<T: Send + Sync + 'static>(&self) -> Option<Arc<T>>` |
| `extensions` | `fn extensions(&self) -> Extensions` — a cloneable handle to the same map |

One value per type; wrap a plain value in a newtype to give it its own slot. All of them
take `&self`, so a filter can store what it learned:

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::Router;

struct UserId(u64);

let _router = Router::new()
    .filter(|ctx| match ctx.header("x-user").and_then(|v| v.parse().ok()) {
        Some(id) => {
            ctx.set(UserId(id));
            None
        }
        None => Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "who are you?")),
    })
    .get("/me", |ctx| async move {
        let id = ctx.get::<UserId>().map(|u| u.0).unwrap_or(0);
        HttpResponse::text(StatusCode::OK, format!("user {id}"))
    });
```

**Application state** is registered once and shared by every request:

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::Router;

struct Config { greeting: String }

let _router = Router::new()
    .with_state(Config { greeting: "hello".into() })
    .get("/", |ctx| async move {
        let greeting = ctx.state::<Config>().map(|c| c.greeting.clone());
        HttpResponse::text(StatusCode::OK, greeting.unwrap_or_default())
    });
```

`ctx.state::<T>()` returns `Option<Arc<T>>`. One value per type; registering a type again
replaces it.

---

## Handlers and typed errors

A handler returns any type that implements `IntoResponse`:

```rust
pub trait IntoResponse {
    fn into_response(self) -> HttpResponse;
}
```

| Type | Response |
|---|---|
| `HttpResponse` | itself |
| `Result<T, E>` (both `IntoResponse`) | whichever side it holds |
| [`Problem`](#problem-rfc-9457) | `application/problem+json` |
| `StatusCode` | that status, empty body |
| `(StatusCode, String)` / `(StatusCode, &'static str)` | `text/plain` with that status |
| `String` / `&'static str` | `200 text/plain` |
| `ParamError` | `400` problem |
| `CodecError` | `400` problem (`415` for an unsupported content type) |
| `FormError` | `415` / `400` / `413` problem |
| `Classified<E>` (`errutils`) | problem with the status of `E`'s `ErrorKind` |

So a handler can return `Result<HttpResponse, E>` and use `?`. Implement the trait for your
own error type to control the mapping:

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{Ctx, IntoResponse, Problem, Router};

enum OrderError { NotFound(u64), Closed }

impl IntoResponse for OrderError {
    fn into_response(self) -> HttpResponse {
        match self {
            OrderError::NotFound(id) => Problem::new(StatusCode::NOT_FOUND)
                .detail(format!("order {id} does not exist"))
                .with("order_id", id)
                .into_response(),
            OrderError::Closed => Problem::new(StatusCode(409))
                .detail("the order is closed")
                .into_response(),
        }
    }
}

async fn show(ctx: Ctx) -> Result<HttpResponse, OrderError> {
    let id: u64 = ctx.param_as("id").map_err(|_| OrderError::NotFound(0))?;
    if id == 0 {
        return Err(OrderError::Closed);
    }
    Ok(HttpResponse::text(StatusCode::OK, format!("order {id}")))
}

let _router = Router::new().get("/orders/:id", show);
```

The custom [fallback handlers](#custom-fallbacks--the-allow-header) accept the same return
types.

### `Problem` (RFC 9457)

The default error body of this module is an RFC 9457 "problem details" document.

| Method | Description |
|---|---|
| `Problem::new(status)` | `type` `about:blank`, `title` the status' reason phrase. |
| `.type_uri(uri)` / `.title(text)` / `.detail(text)` / `.instance(uri)` | The standard members. `detail` is sent to the client — keep internals out of it. |
| `.with(name, value)` | An extension member (reserved names are ignored). |
| `.header(name, value)` | A response header (`WWW-Authenticate`, `Retry-After`, …). |
| `.status()` / `.to_json()` / `.into_response()` | Accessors and rendering. |

```json
{"type":"about:blank","title":"Not Found","status":404,"detail":"order 9 does not exist","order_id":9}
```

`Problem` is `From<ParamError>`, `From<CodecError>` and `From<FormError>`, so in a handler
returning `Result<_, Problem>` the `?` operator converts all three.

### `Classified` (`errutils` feature)

`Classified<E>` renders any error that implements
[`errutils::Classify`](errutils.md) + `Display`:

- the status is `ErrorKind::http_status()` (`NotFound` → 404, `Unavailable` → 503, …);
- the document carries a `kind` member (`"not_found"`, `"internal"`, …);
- the error's `Display` text becomes `detail` **only for caller errors** (4xx kinds). For a
  server-side kind the detail is omitted, so messages such as "write to /var/lib/… failed"
  are not disclosed.

`Classified<E>: From<E>`, so `?` converts:

```rust
use ferroly::errutils::{Classify, ErrorKind};
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{Classified, Ctx};

#[derive(Debug)]
struct Missing;
impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no such item")
    }
}
impl Classify for Missing {
    fn kind(&self) -> ErrorKind { ErrorKind::NotFound }
}

fn load(_id: &str) -> Result<String, Missing> { Err(Missing) }

async fn show(ctx: Ctx) -> Result<HttpResponse, Classified<Missing>> {
    let item = load(ctx.param("id").unwrap_or(""))?;   // -> 404 problem+json
    Ok(HttpResponse::text(StatusCode::OK, item))
}
```

---

## `Router`

```rust
#[must_use]
pub struct Router { /* routes, filters, auth, middlewares, fallbacks, state */ }
```

A consuming builder: each method takes and returns `self`.

### Registering routes

| Method | Description |
|---|---|
| `get` / `post` / `put` / `delete` / `patch` / `head` / `options` | `fn(self, path: &str, handler) -> Self` — one method. |
| `add` | `fn(self, path, handler, methods: Vec<Method>) -> Self` — one handler, several methods. |
| `route` | `fn(self, path, route: Route) -> Self` — a handler with [per-route options](#per-route-options-route). |

Registration never panics and never fails; mistakes are collected and reported by
[`validate`](#validating-the-route-table).

### Path patterns

| Segment | Matches | Example |
|---|---|---|
| literal | exactly that segment, case-sensitive | `/users/new` |
| `:name` | any one segment | `/users/:id` |
| `*name` | the rest of the path — **zero or more** segments; last segment only | `/files/*path` |

`/files/*path` matches `/files`, `/files/` (with `path` empty), `/files/a` and
`/files/a/b/c` (`path` = `a/b/c`, `tail_segments()` = `["a", "b", "c"]`).

**Most specific wins.** When several routes match a path, a literal beats `:name`, which
beats `*name`, compared segment by segment from the left — whatever order they were
registered in:

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new()
    .get("/items/:id", |_| async { HttpResponse::text(StatusCode::OK, "by id") })
    .get("/items/new", |_| async { HttpResponse::text(StatusCode::OK, "new") }); // wins for /items/new
```

Literal segments are written as plain text (`/a b` matches a request for `/a%20b`).

### Per-route options: `Route`

```rust
use std::time::Duration;
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{Route, Router};

let _router = Router::new().route(
    "/upload",
    Route::post(|ctx| async move {
        HttpResponse::text(StatusCode::CREATED, format!("{} bytes", ctx.body().len()))
    })
    .body_limit(64 * 1024)                 // larger bodies -> 413
    .timeout(Duration::from_secs(5))       // slower handlers -> 503
    .filter(|ctx| {                        // a check for this route only
        ctx.header("x-upload-token").is_none()
            .then(|| HttpResponse::text(StatusCode::FORBIDDEN, "no token"))
    })
    .layer(|ctx, next| async move {        // middleware for this route only
        next.run(ctx).await.header("X-Route", "upload")
    }),
);
```

| Constructor / method | Description |
|---|---|
| `Route::get(h)` … `Route::options(h)`, `Route::new(methods, h)` | The handler and its methods. |
| `.layer(mw)` | Onion middleware around this route's handler only. |
| `.filter(f)` / `.filter_async(f)` | A filter for this route only; runs after the router's and groups' filters. |
| `.body_limit(bytes)` | See [Body limits and timeouts](#body-limits-and-timeouts). |
| `.timeout(duration)` | See [Body limits and timeouts](#body-limits-and-timeouts). |

### Route groups

`group(prefix, |g| …)` registers routes under a shared prefix, with filters, middleware and
defaults scoped to it.

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new().group("/api", |api| {
    api.filter(|ctx| {
            ctx.header("x-api-key").is_none()
                .then(|| HttpResponse::text(StatusCode::UNAUTHORIZED, "no key"))
        })
        .get("/health", |_| async { HttpResponse::text(StatusCode::OK, "ok") })
        .group("/admin", |admin| {              // nested: /api/admin/…
            admin
                .filter(|ctx| {
                    ctx.header("x-admin").is_none()
                        .then(|| HttpResponse::text(StatusCode::FORBIDDEN, "admins only"))
                })
                .get("/stats", |_| async { HttpResponse::text(StatusCode::OK, "stats") })
        })
});
```

`Group` has the same registration methods as `Router` (`get` … `options`, `add`, `route`)
plus:

| Method | Description |
|---|---|
| `filter` / `filter_async` | Runs for every request under the prefix; see [Filter scoping](#filter-scoping). |
| `layer` | Onion middleware around the group's handlers (nested groups included). It wraps *handlers*, so it runs only when a route of the group matched. |
| `group` | A nested group; the outer group's filters and layers apply to it too. |
| `body_limit` / `request_timeout` | Defaults for the group's routes. |
| `rate_limit_with` | A [rate limit](#rate-limiting) for the group. |
| `jwt` (`auth`) | Requires a [bearer token](#jwt-authentication-auth-feature) for the group. |

The prefix may contain `:name` parameters (`/tenants/:tenant`), not a `*` catch-all.

### Mounting a sub-router

`mount(prefix, router)` serves every route of another `Router` under a prefix:

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

fn v1() -> Router {
    Router::new().get("/users/:id", |_| async { HttpResponse::text(StatusCode::OK, "user") })
}

let _app = Router::new().mount("/v1", v1());   // GET /v1/users/:id
```

What the mounted router carried comes along, scoped to the prefix:

| Of the mounted router | Becomes |
|---|---|
| authenticator, filters | filters for every request under the prefix (its authenticator first), after the filters this router registered before the `mount` call |
| `layer` middleware | middleware around each of its routes (runs only when one matches) |
| default `body_limit` / `request_timeout` | defaults of its routes |
| state | merged in; on a type both registered, the mounting router's value wins |
| custom 404/405 handlers, `strict_slash`, `auto_head`, `auto_options` | **not** carried over — the mounting router's apply |

### Filters

A filter runs before the handler and may short-circuit with a response. Filters run in
registration order, after the authenticator, for every request (including ones no route
matches).

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new().filter(|ctx| {
    if ctx.header("x-maintenance").is_some() {
        Some(HttpResponse::text(StatusCode::SERVICE_UNAVAILABLE, "maintenance"))
    } else {
        None
    }
});
```

**Asynchronous filters** can await — a database or cache lookup, an introspection call.
The closure is called with `&Ctx`, copies out what it needs, and returns a `'static` future:

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::Router;

struct Account(String);

async fn lookup(key: String) -> Option<String> {
    (key == "k1").then(|| "acme".to_string())
}

let _router = Router::new()
    .filter_async(|ctx| {
        let key = ctx.header("x-api-key").unwrap_or_default().to_string();
        let extensions = ctx.extensions();      // a handle the future can keep
        async move {
            match lookup(key).await {
                Some(account) => {
                    extensions.insert(Account(account));
                    None
                }
                None => Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "bad key")),
            }
        }
    })
    .get("/whoami", |ctx| async move {
        let name = ctx.get::<Account>().map(|a| a.0.clone()).unwrap_or_default();
        HttpResponse::text(StatusCode::OK, name)
    });
```

`filter_async` exists on `Router`, `Group` and `Route`.

### Authenticator

`auth(f)` / `auth_async(f)` set the one authenticator, which runs *before* all filters.
Same shapes as `filter` / `filter_async`. Setting it again replaces the previous one. For
bearer tokens see [JWT authentication](#jwt-authentication-auth-feature).

### Onion middleware: `layer` + `Next`

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router: Router = Router::new()
    .layer(|ctx, next| async move {
        let started = std::time::Instant::now();
        let resp = next.run(ctx).await;                      // run the inner chain
        resp.header("X-Elapsed-Ms", started.elapsed().as_millis().to_string())
    })
    .get("/", |_ctx| async move { HttpResponse::text(StatusCode::OK, "ok") });
```

Layers run in registration order — the first registered is outermost — and wrap the
authenticator, the filters and the handlers. Return without calling `next.run` to
short-circuit. The route is already resolved, so `ctx.route()` and `ctx.param()` are
available.

A layer may **rewrite** the request: take it out with `ctx.into_request()`, change it, build
a new context with `Ctx::from_request`, and pass that to `next.run`. The new context is
normalised and routed afresh, and filters are scoped by its path — a rewrite cannot be used
to step around a group's filters. It starts with empty extensions, so copy over anything
earlier layers stored that later ones need.

### Body limits and timeouts

| Method | Scope | Effect |
|---|---|---|
| `Router::body_limit(bytes)` | every route without its own | larger body → `413 Payload Too Large` |
| `Group::body_limit(bytes)` | the group's routes | ″ |
| `Route::body_limit(bytes)` | one route | ″ |
| `Router::request_timeout(d)` | every route without its own | slower handler → dropped, `503 Service Unavailable` |
| `Group::request_timeout(d)` | the group's routes | ″ |
| `Route::timeout(d)` | one route | ″ |

The most specific setting wins (route, then group, then router).

The server has already read the body when the route is known, so these body limits can only
be **lower** than the server-wide
[`ServerConfig::max_body_bytes`](http.md#resource-limits-serverconfig--serve_with_config)
(8 MiB by default) — that is the value that bounds memory. Likewise
`ServerConfig::handler_timeout` bounds the whole pipeline, middleware included, while the
timeouts here cover the handler and its route-level layers.

### Automatic `HEAD` and `OPTIONS`

- **`HEAD`** for a path with a `GET` route and no `HEAD` route is answered by the `GET`
  handler; the server sends the headers — including the `Content-Length` of the body the
  handler produced — and no body. Turn off with `auto_head(false)`.
- **`OPTIONS`** for a path that has routes but no `OPTIONS` route is answered `204 No Content`
  with an `Allow` header. The authenticator and filters still run first. `OPTIONS *` is
  answered with the methods the router knows. Turn off with `auto_options(false)`.

The `Allow` header (also on `405`) lists the registered methods plus `HEAD` and `OPTIONS`
when the router would answer them.

### Strict slash

By default `/users` and `/users/` are the same route. `strict_slash(true)` makes them
distinct. Routes ending in a `*name` catch-all are not affected.

### Custom fallbacks & the `Allow` header

```rust
use ferroly::turbo::{Problem, Router};
use ferroly::http::StatusCode;

let _router = Router::new()
    .on_not_found(|_ctx| async { Problem::new(StatusCode::NOT_FOUND).detail("no such page") })
    .on_method_not_allowed(|_ctx| async { Problem::new(StatusCode::METHOD_NOT_ALLOWED) });
```

`on_method_not_allowed` responses get the `Allow` header added.

### Validating the route table

Because registration is infallible, call one of these at start-up:

| Method | Returns |
|---|---|
| `problems(&self) -> Vec<RouteError>` | every problem found |
| `validate(&self) -> Result<(), RouteError>` | the first problem |
| `try_into_handler(self) -> Result<Arc<dyn HttpHandler>, RouteError>` | validates, then finalizes |

| `RouteError` | Meaning | What `into_handler` does anyway |
|---|---|---|
| `InvalidPattern { pattern, reason }` | `:` without a name, a repeated parameter name, `*name` not last, a `.`/`..` segment, a catch-all in a group/mount prefix | the route (or the whole group/mount) is **not registered** → `404` |
| `Conflict { method, first, second }` | two routes match exactly the same paths for the same method (`/users/:id` and `/users/:name`) | the first registered wins |
| `Config(message)` | a middleware that cannot work as configured: a JWT secret under 16 bytes, an empty key set, `allow_any_origin` + `allow_credentials`, a CSRF cookie that cannot be set | the middleware fails closed (rejects, or omits the unsafe header) |

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{RouteError, Router};

let router = Router::new()
    .get("/users/:id", |_| async { HttpResponse::text(StatusCode::OK, "a") })
    .get("/users/:name", |_| async { HttpResponse::text(StatusCode::OK, "b") });
assert!(matches!(router.validate(), Err(RouteError::Conflict { .. })));
```

A literal beside a parameter (`/a/b` and `/a/:x`), or the same path with different methods,
is not a conflict.

---

## Built-in middleware

### CORS

```rust
use std::time::Duration;
use ferroly::http::{HttpResponse, Method, StatusCode};
use ferroly::turbo::{Cors, Router};

let cors = Cors::new()
    .allow_origin("https://app.example.com")
    .allow_origin_fn(|origin| origin.starts_with("https://") && origin.ends_with(".example.com"))
    .allow_methods([Method::Get, Method::Post, Method::Delete])
    .allow_headers(["content-type", "authorization"])
    .expose_headers(["x-request-id"])
    .allow_credentials(true)
    .max_age(Duration::from_secs(600));

let _router = Router::new()
    .cors(cors)          // first: preflights must be answered before authentication
    .get("/items", |_| async { HttpResponse::text(StatusCode::OK, "[]") });
```

| Builder | Description |
|---|---|
| `Cors::new()` | Allows **no** origin; methods `GET`, `HEAD`, `POST`; no extra headers; no credentials. |
| `Cors::permissive()` | Any origin, common methods, any header, **no** credentials — for public APIs. |
| `allow_origin(o)` / `allow_origins(list)` | Exact origins (`scheme://host[:port]`). |
| `allow_origin_fn(f)` | A predicate, in addition to the list. Match exactly or by a full dotted suffix — never by substring. |
| `allow_any_origin()` | `Access-Control-Allow-Origin: *`. |
| `allow_methods(list)` | Methods for cross-origin requests. |
| `allow_headers(list)` / `allow_any_header()` | Request headers allowed (or echo whatever the preflight asks for). |
| `expose_headers(list)` | Response headers scripts may read. |
| `allow_credentials(bool)` | Cookies / HTTP auth on cross-origin requests. |
| `max_age(duration)` | Preflight cache lifetime. |

Behaviour:

- A **preflight** (`OPTIONS` with `Access-Control-Request-Method`) from an allowed origin is
  answered `204` with the `Access-Control-Allow-*` headers; the authenticator, filters and
  handler do not run. From any other origin: `403`, no CORS headers.
- **Other requests** run normally; `Access-Control-Allow-Origin` (and credentials / expose
  headers) are added only for an allowed origin. CORS is a browser-enforced read protection,
  **not** access control — the request still has to authenticate.
- `Vary: Origin` is added whenever the answer depends on the origin.
- **`*` is never combined with credentials.** With `allow_any_origin()` the header is the
  literal `*` and `Access-Control-Allow-Credentials` is never sent, whatever
  `allow_credentials` says; `validate()` reports the contradiction. To allow credentials,
  list the origins or supply a predicate — the specific origin is then echoed.

### Security headers

```rust
use std::time::Duration;
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{FrameOptions, Router, SecurityHeaders};

let _router = Router::new()
    .security_headers(
        SecurityHeaders::new()
            .hsts(Duration::from_secs(63_072_000), true)
            .frame_options(FrameOptions::SameOrigin)
            .content_security_policy("default-src 'self'")
            .referrer_policy("strict-origin-when-cross-origin"),
    )
    .get("/", |_| async { HttpResponse::text(StatusCode::OK, "ok") });
```

| Header | Default | Change with |
|---|---|---|
| `Strict-Transport-Security` | `max-age=31536000; includeSubDomains`, on TLS requests only | `hsts`, `hsts_preload`, `no_hsts`, `hsts_behind_proxy(true)` (send on every response when TLS ends at a proxy) |
| `X-Content-Type-Options` | `nosniff` | `nosniff(false)` |
| `X-Frame-Options` | `DENY` | `frame_options(Deny / SameOrigin / Unrestricted)` |
| `Content-Security-Policy` | `frame-ancestors 'none'` | `content_security_policy(policy)`; a matching `frame-ancestors` is appended when the policy has none |
| `Referrer-Policy` | `no-referrer` | `referrer_policy`, `no_referrer_policy` |
| any other | — | `header(name, value)` |

The headers are added to every response, including the router's own `404`/`405`. A header
the handler already set is left alone.

### Request ids

`Router::request_id()` gives every request an id, available as `ctx.request_id()` (and as
the `RequestId` extension) and echoed in the `X-Request-Id` response header.

An inbound `X-Request-Id` is kept **only** if it is 1–128 characters from
`A-Z a-z 0-9 . _ -` (`is_valid_request_id`). Anything else — spaces, quotes, separators,
control or non-ASCII characters, over-long values — is discarded and replaced by a generated
id (32 hex characters), because a request id travels into logs, headers and downstream
requests. Register it early so later layers can use the id.

### CSRF (`hash` feature)

For applications that authenticate with **cookies**. `Router::csrf(Csrf::new())` implements
the double-submit pattern:

- A safe request (`GET`, `HEAD`, `OPTIONS`, `TRACE`) without a token cookie is issued a
  random 256-bit token in a cookie; the token is available to the handler as the
  `CsrfToken` extension, to render into the page.
- An unsafe request must send the same token twice — in the cookie and in the
  `X-CSRF-Token` request header. The two are compared in constant time; anything else is
  `403`.
- The token comes from [`hash::random`](hash.md#platform-support-and-limits), which reads
  `/dev/urandom`. Where that is unavailable (Windows, wasm, a sandbox without the device),
  no token can be issued, and a safe request without a token cookie is answered `500`
  rather than given a predictable token.

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{Csrf, CsrfToken, Router};

let _router = Router::new()
    .csrf(Csrf::new())
    .get("/form", |ctx| async move {
        let token = ctx.get::<CsrfToken>().map(|t| t.0.clone()).unwrap_or_default();
        HttpResponse::text(StatusCode::OK, format!("<meta name=\"csrf\" content=\"{token}\">"))
    })
    .post("/transfer", |_| async { HttpResponse::text(StatusCode::OK, "done") });
```

The cookie is `__Host-csrf` (`Secure`, `SameSite=Strict`, `Path=/`, readable by scripts).
The `__Host-` prefix makes browsers refuse a same-named cookie set by a subdomain or over
plain HTTP — the known weakness of double-submit. For local development over plain HTTP use
`Csrf::new().cookie_name("csrf").secure(false)`. Options: `cookie_name`, `header_name`,
`secure`, `same_site`, `max_age`. APIs authenticated by an `Authorization` header do not
need CSRF protection.

### Rate limiting

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{RateLimit, Router, TrustedProxies};

// 5 requests per second, bursts of 20, per client address.
let _router = Router::new()
    .rate_limit_with(RateLimit::new(5.0, 20.0))
    .get("/", |_| async { HttpResponse::text(StatusCode::OK, "ok") });

// Behind load balancers in 10.0.0.0/8:
let limit = RateLimit::new(5.0, 20.0)
    .trusted_proxies(TrustedProxies::from_cidrs(["10.0.0.0/8"]).unwrap());
# let _ = limit;
```

A token bucket per key: `burst` tokens, refilled at `per_second`; a request takes one, and
a request that finds none is answered `429 Too Many Requests`.

| Builder | Description |
|---|---|
| `RateLimit::new(per_second, burst)` | The limit. |
| `key_fn(f)` | Key by something else (an API key, the authenticated user). |
| `trusted_proxies(proxies)` | Derive the client address from forwarding headers written by these proxies. |
| `max_keys(n)` | Bucket table size (default 10 000). |
| `headers(bool)` | Send `RateLimit-*` / `Retry-After` (default on). |
| `into_filter()` | A filter, for `Route::filter` or manual composition. |

- **Key.** By default the client address: the TCP peer, or the address from forwarding
  headers when `trusted_proxies` is set — never otherwise. IPv6 clients are keyed by their
  `/64` network, since one host typically controls a whole `/64`.
- **Memory.** At most `max_keys` buckets. When the table is full, idle (fully refilled)
  buckets are dropped first; if none are idle, the least recently used tenth is evicted.
  Evicting an active bucket hands that client a fresh burst — the price of a hard memory
  bound; size `max_keys` for your traffic.
- **Headers.** `RateLimit-Limit` (the burst), `RateLimit-Remaining` (tokens left),
  `RateLimit-Reset` (seconds until the bucket is full); on `429` also `Retry-After` (seconds
  until one token is available).
- **Scope.** `Router::rate_limit_with` (all requests), `Group::rate_limit_with` (a prefix),
  `Route::filter(limit.into_filter())` (one route). Each call has its own buckets.
- The limiter is per process: several instances of a service each keep their own counts.

`Router::rate_limit(per_second, burst, key_of)` is the original shorthand for
`rate_limit_with(RateLimit::new(per_second, burst).key_fn(key_of))`. Do not derive the key
from `X-Forwarded-For` yourself.

#### Trusted proxies

```rust
use ferroly::turbo::{ForwardedHeader, TrustedProxies};

let proxies = TrustedProxies::from_cidrs(["10.0.0.0/8", "127.0.0.1", "fd00::/8"])
    .unwrap()
    .header(ForwardedHeader::XForwardedFor);   // the default; or ForwardedHeader::Forwarded
# let _ = proxies;
```

| Method | Description |
|---|---|
| `TrustedProxies::none()` | Trust nothing (the default everywhere). |
| `from_cidrs(list)` | Addresses and CIDR ranges of your proxies. |
| `trust_ip(ip)` | One more address. |
| `header(which)` | The header your proxies maintain: `X-Forwarded-For` or RFC 7239 `Forwarded`. Only that one is read. |
| `is_trusted(ip)` / `client_ip(&ctx)` | Queries. Also `ctx.client_ip(&proxies)`. |

Rules, in order:

1. If the TCP peer is **not** a trusted proxy, it is the client. Headers are ignored.
2. Otherwise the configured header is read, across all its lines, and walked **from the
   right**. The first address that is not a trusted proxy is the client.
3. An entry that is not an IP address (`unknown`, an obfuscated identifier) stops the walk.
4. If every entry is a trusted proxy (or the header is absent), the nearest proxy is used.

Make sure the proxy **appends to or overwrites** the header on every request; a header the
proxy passes through untouched is still the client's.

### Cookies

Reading:

```rust
# use ferroly::turbo::Ctx;
# fn h(ctx: &Ctx) {
let session = ctx.cookie("sid");          // Option<String>
let all = ctx.cookies();                  // Vec<(String, String)>
# let _ = (session, all);
# }
```

Values are returned as sent (not percent-decoded); `parse_cookies(header)` is the underlying
parser.

Writing — `SetCookie` starts from **secure defaults** (`Path=/`, `Secure`, `HttpOnly`,
`SameSite=Lax`):

```rust
use std::time::Duration;
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{SameSite, SetCookie};

let cookie = SetCookie::new("__Host-session", "abc123")
    .max_age(Duration::from_secs(3600))
    .same_site(SameSite::Strict);
// __Host-session=abc123; Path=/; Max-Age=3600; Secure; HttpOnly; SameSite=Strict
let resp = cookie.apply(HttpResponse::new(StatusCode::NO_CONTENT)).unwrap();

let logout = SetCookie::removal("__Host-session").apply(resp).unwrap();
# let _ = logout;
```

| Method | Description |
|---|---|
| `SetCookie::new(name, value)` / `SetCookie::removal(name)` | A cookie; a header that deletes one. |
| `path` / `no_path` / `domain` / `max_age` / `expires` | Attributes. |
| `secure(bool)` / `http_only(bool)` / `same_site(SameSite)` / `no_same_site` | Attributes. |
| `to_header_value() -> Result<String, CookieError>` | Validates and renders. |
| `apply(resp) -> Result<HttpResponse, CookieError>` | Appends a `Set-Cookie` header. |

Building validates: the name must be a token; the value may not contain a space, `"`, `,`,
`;`, `\`, a control or non-ASCII character (percent-encode it first) — so a value can never
inject an attribute or a header; and the browser rules are enforced:

- `__Host-…` requires `Secure`, `Path=/` and **no** `Domain`;
- `__Secure-…` requires `Secure`;
- `SameSite=None` requires `Secure`.

### Forms and multipart

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{FormError, MultipartLimits, Router};

let _router = Router::new()
    .post("/login", |ctx| async move {
        let form = ctx.form()?;                               // application/x-www-form-urlencoded
        let user = form.get("user").unwrap_or("").to_string();
        Ok::<_, FormError>(HttpResponse::text(StatusCode::OK, user))
    })
    .post("/upload", |ctx| async move {
        let limits = MultipartLimits { max_parts: 4, max_part_bytes: 1024 * 1024, ..Default::default() };
        let parts = ctx.multipart(&limits)?;                  // multipart/form-data
        let names: Vec<&str> = parts.iter().map(|p| p.name()).collect();
        Ok::<_, FormError>(HttpResponse::text(StatusCode::OK, names.join(",")))
    });
```

`ctx.form()` returns a `Form` (`get`, `get_all`, `pairs`, `len`); keys and values are
percent-decoded (`+` is a space) and must be UTF-8.

`ctx.multipart(&limits)` returns the `Part`s:

| `Part` method | Description |
|---|---|
| `name()` | The field name. |
| `filename()` | The client's file name, **as sent — untrusted**; it may contain `/`, `\` or `..`. |
| `safe_filename()` | The last path component with control characters removed; `None` if nothing usable remains. Prefer a name you generate. |
| `content_type()` / `header(name)` | Part headers. |
| `data()` / `text()` / `into_data()` | The content. |

| `MultipartLimits` field | Default |
|---|---|
| `max_parts` | 64 |
| `max_part_bytes` | 8 MiB |
| `max_total_bytes` | 16 MiB |
| `max_part_header_bytes` | 8 KiB |

`FormError` renders as: `ContentType` → `415`, `Malformed` → `400`, `TooLarge` → `413`.
Bodies are fully buffered (bounded by the server's `max_body_bytes`); there is no streaming
upload API.

### Static files

```rust
use ferroly::turbo::{Router, StaticFiles};

let _router = Router::new().static_files(
    "/assets",
    StaticFiles::new("./public")
        .index_file("index.html")
        .cache_control("public, max-age=3600"),
); // GET /assets/css/site.css -> ./public/css/site.css
```

`static_files(prefix, files)` registers `GET prefix/*path` (which also answers `HEAD`).
`StaticFiles::route()` gives the same thing as a [`Route`](#per-route-options-route) for a
pattern of your choosing, and `serve(&ctx)` / `serve_segments(..)` let a handler call it.

Responses carry:

- `Content-Type` from the extension (the [`fsutils`](fsutils.md) table with that feature, a
  small built-in table otherwise, `application/octet-stream` when unknown) and
  `X-Content-Type-Options: nosniff`;
- `ETag` (a weak validator from size and modification time) and `Last-Modified`;
  `If-None-Match` and `If-Modified-Since` are answered `304 Not Modified`;
- `Accept-Ranges: bytes`; a single-range `Range` is answered `206 Partial Content`, an
  unsatisfiable one `416` with `Content-Range: bytes */<size>`. Several ranges in one
  request, or a `Range` whose `If-Range` does not match, get the whole file;
- `Cache-Control`, if configured.

Files up to 256 KiB are sent with a `Content-Length`; larger ones are streamed in chunks.

**Confinement.** Only regular files inside the root can be reached:

- the path comes from the `*` catch-all, already normalised by the router (no `.`/`..`);
- each segment is percent-decoded on its own and refused if it then contains `/`, `\`, a
  NUL or control character, or is `.` or `..` — an encoded separator (`..%2F..%2Fsecret`) or
  encoded dots cannot build a traversal;
- the final path is resolved through symbolic links and must still lie under the resolved
  root; a link pointing outside is not followed;
- names beginning with `.` (`.env`, `.git`) are refused unless `allow_hidden(true)`;
- a directory is served only through its index file, when one is configured; there are no
  listings.

Anything refused is a plain `404`.

---

## Finalizing and serving

| Method | Signature | Description |
|---|---|---|
| `into_handler` | `fn(self) -> Arc<dyn HttpHandler>` | Finalizes; never fails (see [validation](#validating-the-route-table)). |
| `try_into_handler` | `fn(self) -> Result<Arc<dyn HttpHandler>, RouteError>` | Validates first. |
| `serve` | `async fn(self, addr) -> Result<(), HttpError>` | Binds and serves until the process ends, default `ServerConfig`. |
| `serve_with_shutdown` | `async fn(self, addr, shutdown)` | Serves until `shutdown` resolves, then drains. |
| `serve_with_config` | `async fn(self, addr, config: ServerConfig, shutdown)` | Explicit limits and timeouts. |
| `serve_listener` | `async fn(self, listener: TcpListener, config, shutdown)` | On an already bound listener. |
| `serve_tls` | `async fn(self, addr, tls: TlsConfig, config, shutdown)` | HTTPS. |
| `serve_tls_reloadable` | `async fn(self, addr, tls: ReloadableTlsConfig, config, shutdown)` | HTTPS with certificate rotation. |
| `serve_tls_listener` | `async fn(self, listener, tls: ReloadableTlsConfig, config, shutdown)` | HTTPS on a bound listener (`TlsConfig::reloadable()` for a fixed certificate). |

`shutdown` is any `Future<Output = ()> + Send`; pass `std::future::pending()` to serve
forever.

```rust
use std::time::Duration;
use ferroly::http::{ServerConfig, TlsConfig};
use ferroly::turbo::Router;

# async fn ex(router: Router) -> Result<(), ferroly::http::HttpError> {
let config = ServerConfig {
    max_body_bytes: 1024 * 1024,
    handler_timeout: Some(Duration::from_secs(30)),
    ..ServerConfig::default()
};
let tls = TlsConfig::from_pem(&std::fs::read("server.crt")?, &std::fs::read("server.key")?)?;

router
    .serve_tls("0.0.0.0:8443", tls, config, async {
        let _ = tokio::signal::ctrl_c().await;      // graceful shutdown
    })
    .await
# }
```

See [`http`](http.md#https-serve_tls--tlsconfig) for `TlsConfig` (mutual TLS, minimum
version), [`ReloadableTlsConfig`](http.md#certificate-reload) and every
[`ServerConfig`](http.md#resource-limits-serverconfig--serve_with_config) field. For a server
that is a [lifecycle](lifecycle.md) component with health endpoints, use
[`rest::Server`](rest.md).

The `serve*` methods finalize with `into_handler`; call `validate()` first if you want
routing mistakes to stop start-up.

---

## Feature-gated helpers

### JWT authentication (`auth` feature)

| Method | Verifies | Validation |
|---|---|---|
| `jwt_auth(secret)` | HS256 | **Legacy, lenient**: `exp`/`nbf` checked only when present; any non-empty secret. Unchanged, so existing tokens keep working. |
| `jwt_auth_with(secret, Validation)` | HS256 | [`auth::Validation`](auth.md): `exp` **required** by default, `nbf`, `iat`, issuer, audience, subject, required claims, leeway; secret ≥ 16 bytes. |
| `jwt_auth_hmac_with(alg, secret, Validation)` | HS256 / HS384 / HS512 | as above; the token's `alg` must equal `alg`. |
| `jwt_auth_jwks(Arc<RemoteJwks>, Validation)` | RS256 / ES256 … via a remote JWKS endpoint | as above; asynchronous (key fetch + cache). |
| `jwt(JwtAuth)` | any of the above | the general form; also `Group::jwt` |

```rust
use ferroly::auth::Validation;
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::Router;

let _router = Router::new()
    .jwt_auth_with(
        "a-shared-secret-of-at-least-16-bytes",
        Validation::new().issuer("https://issuer.example").audience("orders-api"),
    )
    .get("/me", |ctx| async move {
        // The verified claims reach the handler.
        let subject = ctx.claims().and_then(|c| c.subject().map(str::to_string));
        HttpResponse::text(StatusCode::OK, subject.unwrap_or_default())
    });
```

**Prefer `jwt_auth_with`** (or `jwt`) for new code. `jwt_auth` keeps its original behaviour
— it does **not** require `exp` — purely for compatibility.

`JwtAuth` is the configurable form:

| Constructor / method | Description |
|---|---|
| `JwtAuth::hs256(secret, validation)` / `JwtAuth::hmac(alg, secret, validation)` | Shared-secret tokens. |
| `JwtAuth::jwks(Jwks, validation)` | A fixed public key set. |
| `JwtAuth::remote_jwks(Arc<RemoteJwks>, validation)` | A remote, cached key set. |
| `JwtAuth::legacy_hs256(secret)` | What `jwt_auth` uses. |
| `.optional(true)` | A request **without** a token passes anonymously (no claims); a presented token is still verified, and rejected if invalid. |
| `.verify(token).await` | Verify a token yourself. |

Behaviour:

- The token is taken from `Authorization: Bearer <token>` (scheme case-insensitive);
  `bearer_token(&ctx)` exposes that.
- Missing token → `401` with `WWW-Authenticate: Bearer`. Invalid token → `401` with
  `WWW-Authenticate: Bearer error="invalid_token"`. The body never says why.
- On success the claims are stored as the `Claims` extension: `ctx.claims()` returns
  `Option<Arc<Claims>>` with `get(name)`, `subject()`, `scopes()`, `roles()`, `value()`.
- A configuration that can accept no token (secret too short, empty key set) rejects every
  request and is reported by `validate()`.

**Scope and role guards** are filters built on [`auth::Scopes`](auth.md):

| Function | Requires |
|---|---|
| `require_scopes(&["a", "b"])` | all listed scopes (`scope` / `scp` claims) |
| `require_any_scope(&["a", "b"])` | at least one |
| `require_roles(&["admin"])` | all listed roles (`roles` claim) |
| `require_any_role(&["admin", "staff"])` | at least one |

They answer `401` when the request has no verified claims and `403` when the requirement is
not met (scope guards add `WWW-Authenticate: Bearer error="insufficient_scope", scope="…"`).

```rust
use std::sync::Arc;
use ferroly::auth::{RemoteJwks, Validation};
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::turbo::{require_any_role, require_scopes, JwtAuth, Route, Router};

let keys = Arc::new(RemoteJwks::new("https://issuer.example/.well-known/jwks.json"));
let auth = JwtAuth::remote_jwks(keys, Validation::new().audience("orders-api"));

let _router = Router::new().group("/orders", |g| {
    g.jwt(auth)
        .filter(require_scopes(&["orders:read"]))
        .get("/", |_| async { HttpResponse::text(StatusCode::OK, "[]") })
        .route(
            "/purge",
            Route::delete(|_| async { HttpResponse::new(StatusCode::NO_CONTENT) })
                .filter(require_any_role(&["admin"])),
        )
});
```

### `access_log` (`log` feature)

`Router::access_log(logger)` logs one `Info` record per request:

| Field | Value |
|---|---|
| `method`, `status`, `ms` | method, response status (a number), duration in ms |
| `path` | the normalised path |
| `route` | the matched template (absent when no route matched) |
| `bytes` | response body size (0 for a streamed body) |
| `peer` | the connected address |
| `request_id` | when `request_id()` or `trace_context()` is registered before it |

If `logger` has no target, records are logged under the target
`ferroly::turbo::access` (`ACCESS_LOG_TARGET`), so
[`log::set_level_for`](log.md) can adjust the access log independently:

```rust
# #[cfg(feature = "log")]
# fn ex() {
use ferroly::log::{logger, set_level_for, Level};
use ferroly::turbo::Router;

let _router = Router::new().request_id().access_log(logger());
set_level_for("ferroly::turbo::access", Level::Warn);   // silence it
# }
```

Requests rejected with `400` during path normalisation are not logged here.

### `trace_context` (`log` or `obs` feature)

`Router::trace_context()` correlates everything that happens while a request is handled.

- **Request id** (always): as [`request_id()`](#request-ids) — a valid inbound
  `X-Request-Id` or a generated one, in `ctx.request_id()` and the response header.
- **Tracing** (`obs`): the W3C trace context (`traceparent` / `tracestate`) is extracted and
  a [server span](obs.md) is started as its child — or as a new trace when there is none or
  it is malformed. The span is named `"{METHOD} {route template}"` (the method alone when no
  route matched; an unknown method is `_OTHER`), is the **current span** for the whole
  request — so spans started in handlers, and outbound calls made with
  [`rest::Client`](rest.md), join the trace — records `http.route`,
  `http.response.status_code`, `url.path` and the request id, is marked as an error on a
  `5xx`, and ends when the response is produced or the request is dropped.
- **Logging** (`log`): every `ferroly::log` record emitted during the request carries
  `request_id`, plus `trace_id` and `span_id` of the request's span when `obs` is on.
  Without `obs`, `trace_id` is the request id (the previous behaviour).

Register it before `access_log` so access records are correlated too.

### `metrics` / `metrics_route` (`metrics` feature)

`Router::metrics()` records RED metrics into the process-global
[`metrics::Registry`](metrics.md); `metrics_in(&registry)` uses another one;
`metrics_route(path)` serves the global registry in the Prometheus text format
(`PROMETHEUS_CONTENT_TYPE`).

| Metric | Type | Labels |
|---|---|---|
| `http_requests_total` | counter | `method`, `route`, `status` |
| `http_request_duration_seconds` | histogram | `method`, `route` |
| `http_requests_in_flight` | gauge | — |

**Label values come from bounded sets**, so a client cannot grow the number of series:

- `method` is one of the nine standard method names, or `OTHER` for any other token;
- `route` is the matched route **template** (`/users/:id`) — never the request path — or
  `unmatched` (`UNMATCHED_ROUTE`) when no route matched;
- `status` is the response status code, or `cancelled` when the request was dropped before
  a response was produced (the client disconnected, a server timeout fired).

The instruments are bound once when the middleware is installed. The in-flight gauge and
the duration are recorded by RAII guards, so they stay correct when a request is cancelled
or a handler panics.

```rust
# #[cfg(feature = "metrics")]
# fn ex() {
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let _router = Router::new()
    .metrics()
    .metrics_route("/metrics")
    .get("/users/:id", |_| async { HttpResponse::text(StatusCode::OK, "user") });
// http_requests_total{method="GET",route="/users/:id",status="200"} 3
# }
```

`metrics_route` is served to whoever can reach it; put it behind your authenticator or a
group filter, or on an internal listener, if the metrics are not public.

---

## Error handling

| Type | Raised by | Variants |
|---|---|---|
| `ParamError` | `param_as`, `query_as`, `Ctx::from_request`, `TrustedProxies::from_cidrs` | `Missing(name)`, `Invalid { name, reason }` |
| `RouteError` | `validate`, `problems`, `try_into_handler` | `InvalidPattern { pattern, reason }`, `Conflict { method, first, second }`, `Config(message)` |
| `FormError` | `ctx.form`, `ctx.multipart`, `Form::parse`, `parse_multipart` | `ContentType(found)`, `Malformed(reason)`, `TooLarge(limit)` |
| `CookieError` | `SetCookie::to_header_value` / `apply` | `InvalidName`, `InvalidValue`, `InvalidAttribute(name)`, `Rule(reason)` |

All are `#[non_exhaustive]`, implement `Display` + `std::error::Error`, and (except
`RouteError` and `CookieError`) implement [`IntoResponse`](#handlers-and-typed-errors).

Statuses the router itself produces:

| Status | When |
|---|---|
| `400` | the target cannot be normalised, the query is not valid, a path parameter is not UTF-8 |
| `404` | no route (or `on_not_found`) |
| `405` + `Allow` | the path has routes, none for the method (or `on_method_not_allowed`) |
| `204` + `Allow` | automatic `OPTIONS` |
| `413` | a route / router body limit |
| `503` | a route / router timeout |

---

## Upgrading to 0.4

Behaviour that changed in 0.4.0:

- **Paths are normalised before routing.** `//x`, a path without a leading `/`, malformed
  escapes and encoded control characters are now `400` (they used to be matched leniently);
  dot segments are resolved; `ctx.path()` returns the normalised path.
- **Path parameters are percent-decoded**, and query **keys** are decoded as well as values.
  A query with a malformed escape or invalid UTF-8 is `400` (it used to be passed through
  lossily).
- **The most specific route wins**, regardless of registration order (it used to be the
  first registered).
- **`HEAD` and `OPTIONS` are answered automatically** (they used to be `405`); opt out with
  `auto_head(false)` / `auto_options(false)`. The `Allow` header now includes them.
- **Group filters are scoped by segments**, so a group `/api` no longer covers `/apix`, and
  a group with a parameter in its prefix now runs its filters (it never did).
- **`metrics()`** adds a `route` label and reports unknown methods as `OTHER`.
- **`trace_context()`** validates the inbound `X-Request-Id`, adds a `request_id` log field,
  and with the `obs` feature the `trace_id` field is the W3C trace id.
- **`rate_limit`** responses carry `RateLimit-*` headers.
- Handlers may return any `IntoResponse` type. A closure whose return type was inferred
  *only* from the old `HttpResponse` bound (for example one ending in `.into()`) needs a type
  annotation.

---

## Limitations

- **No response compression.** The crate has an INFLATE decoder but no DEFLATE encoder yet,
  so there is no `gzip` middleware; compress at the reverse proxy.
- **No OpenAPI generation.** Routes are not introspectable into a schema document.
- **No sessions.** Use a signed or opaque session id in a [cookie](#cookies) and your own
  store.
- **No WebSocket upgrade from a route.** Serve WebSockets with [`ferroly::ws`](ws.md) on its
  own listener for now.
- **Request bodies are buffered** by the server before the router runs; there is no
  streaming request body, and multipart uploads are parsed in memory within the limits.
- **Rate limiting is per process.**
- **Static-file `ETag`s are weak**, so `If-Range` with an entity tag falls back to the whole
  file (a date works).
- **HTTP/1.1 only** — see [`http`](http.md#limitations).

---

## See also

- [`ferroly::http`](http.md) — the server, `ServerConfig`, TLS, `HttpResponse`, streaming.
- [`ferroly::rest`](rest.md) — a lifecycle-managed server built on this router, and a client.
- [`ferroly::auth`](auth.md) — JWT verification, `Validation`, JWKS, `Scopes`.
- [`ferroly::codec`](codec.md) — the `Encode`/`Decode` traits and schema validation.
- [`ferroly::metrics`](metrics.md), [`ferroly::obs`](obs.md), [`ferroly::log`](log.md) —
  what the observability middleware reports to.
- [`ferroly::errutils`](errutils.md) — `Classify` / `ErrorKind`.
