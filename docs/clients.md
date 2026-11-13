# ferroly::clients

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `clients` (implies `http` + `tokio`).

The resilience toolkit shared by ferroly's HTTP-based code, dependency-minimal
and built entirely in-house:

| Area | Items |
|---|---|
| [Auth providers](#auth-providers) | `AuthProvider`, `authorize`, `BearerAuth`, `ApiKeyAuth`, `BasicAuth` |
| [Refreshing credentials](#refreshing-credentials) | `TokenSource`, `AccessToken`, `RefreshingAuth`, `OAuth2ClientCredentials` |
| [Retry](#retry) | `RetryPolicy`, `Retry`, `Jitter`, `RetryBudget`, `RetryDecision`, `RetryAttempt`, `retry`, `retry_with`, `parse_retry_after` |
| [Circuit breaking](#circuit-breaking) | `CircuitBreaker`, `Permit`, `CircuitBreakerConfig`, `CircuitBreakerBuilder`, `FailureRate`, `Outcome`, `CircuitState`, `BreakerMetrics`, `CircuitBreakerRegistry` |
| [Bulkhead](#bulkhead) | `Bulkhead`, `BulkheadPermit` |
| [Rate limiting](#rate-limiter) | `RateLimiter` |
| [Timeouts](#timeouts-and-deadlines) | `with_timeout`, `Deadline` |
| [Composition](#policy-the-composed-pipeline) | `Policy`, `ErrorClass` |
| [Errors](#error-handling) | `CircuitOpenError`, `BulkheadFullError`, `TimeoutError`, `RateLimitedError`, `PolicyError<E>`, `AuthError` |
| Secrets | `Secret` — see [hash](hash.md#secret) |

Everything except the auth providers is generic over the operation and its
error type — usable around any async call, not only HTTP.

These are the building blocks that [`rest`](rest.md) and [`genai`](genai.md)
reuse: `rest`'s `ClientOptions` takes an `Arc<dyn AuthProvider>` and a
`RetryPolicy`, and its `RequestBuilder::send` runs under the `retry` driver.

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["clients"] }
```

`OAuth2ClientCredentials` and `ClientAuthStyle` additionally need the `codec`
feature (a default feature), to parse the token endpoint's JSON response.

---

## Auth providers

### The `AuthProvider` trait

```rust
pub trait AuthProvider: Send + Sync {
    /// Copies the credentials into the request's headers.
    fn apply(&self, req: &mut Request);

    /// Makes sure those credentials are present and current (default: no-op).
    fn prepare(&self) -> BoxFuture<'_, Result<(), AuthError>> { /* Ok(()) */ }
}

pub async fn authorize(provider: &dyn AuthProvider, req: &mut Request) -> Result<(), AuthError>
```

`apply` is synchronous: it copies credentials the provider already holds into
headers. That is all a static scheme needs. A provider whose credentials
**expire** has to do I/O first; that happens in `prepare`, an async hook whose
default does nothing.

**Callers should `prepare().await` before every `apply`** — or simply call
`authorize`, which does both — so that any provider works, static or
refreshing:

```rust
use std::sync::Arc;
use ferroly::clients::{authorize, AuthError, AuthProvider, BearerAuth};
use ferroly::http::{Method, Request};

async fn build(auth: &Arc<dyn AuthProvider>) -> Result<Request, AuthError> {
    let mut req = Request::builder(Method::Get, "https://api.example/v1/items")
        .expect("valid url")
        .build();
    authorize(auth.as_ref(), &mut req).await?;   // prepare(), then apply()
    Ok(req)
}

let auth: Arc<dyn AuthProvider> = Arc::new(BearerAuth::new("token"));
```

`prepare` has a default, so existing `AuthProvider` implementations and callers
that only use `apply` keep compiling. Code that calls `apply` without
`prepare` works for static providers but sends **no credentials** for a
refreshing one that has not fetched a token yet.

Implement the trait yourself for a custom scheme:

```rust
use ferroly::clients::AuthProvider;
use ferroly::http::Request;

struct HmacAuth { /* ... */ }

impl AuthProvider for HmacAuth {
    fn apply(&self, req: &mut Request) {
        // compute a signature over req and set a header
        req.headers.set("x-signature", "…");
    }
}
```

### `BearerAuth`, `ApiKeyAuth`, `BasicAuth`

| Type | Constructor | Sets |
|---|---|---|
| `BearerAuth` | `BearerAuth::new(token)`, `BearerAuth::from_secret(Secret)` | `Authorization: Bearer <token>` |
| `ApiKeyAuth` | `ApiKeyAuth::new(header, key)` | `<header>: <key>` |
| `BasicAuth` | `BasicAuth::new(user, pass)` | `Authorization: Basic base64(user:pass)` |

```rust
use ferroly::clients::{ApiKeyAuth, BasicAuth, BearerAuth};

let bearer = BearerAuth::new("my-secret-token");     // Authorization: Bearer my-secret-token
let key = ApiKeyAuth::new("x-api-key", "abc123");    // x-api-key: abc123
let basic = BasicAuth::new("alice", "s3cr3t");       // Authorization: Basic YWxpY2U6czNjcjN0

// Credentials never appear in Debug output:
assert_eq!(format!("{bearer:?}"), "BearerAuth { token: Secret([REDACTED]) }");
assert!(format!("{basic:?}").contains("alice"));
assert!(!format!("{basic:?}").contains("s3cr3t"));
```

All three are `Clone`. Their secrets are held in a
[`Secret`](hash.md#secret): `Debug` redacts them (the header name and user name
stay visible for diagnostics) and the buffers are wiped on drop, best-effort.
CR/LF in a credential is stripped when the header is set, so a credential
cannot inject headers.

---

## Refreshing credentials

### `TokenSource` and `RefreshingAuth`

```rust
pub trait TokenSource: Send + Sync {
    fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>>;
}
```

A `TokenSource` obtains a fresh access token — an OAuth 2.0 grant, a cloud
metadata endpoint, a secrets manager. It should not cache.
`AccessToken::new(token)` optionally `.expires_in(Duration)`; accessors
`token() -> &Secret`, `lifetime() -> Option<Duration>`; `Debug` redacts.

`RefreshingAuth` wraps a source into an `AuthProvider` that sends
`Authorization: Bearer <token>`:

```rust
use std::time::Duration;
use ferroly::clients::{AccessToken, AuthError, RefreshingAuth, TokenSource};
use ferroly::http::BoxFuture;

struct Vault;

impl TokenSource for Vault {
    fn fetch_token(&self) -> BoxFuture<'_, Result<AccessToken, AuthError>> {
        Box::pin(async {
            // ... call the secrets manager ...
            Ok(AccessToken::new("token-from-vault").expires_in(Duration::from_secs(900)))
        })
    }
}

let auth = RefreshingAuth::new(Vault).skew(Duration::from_secs(60));
```

| Method | Notes |
|---|---|
| `RefreshingAuth::new(source)` / `from_arc(Arc<dyn TokenSource>)` | |
| `skew(Duration)` | refresh this long before expiry, so a request never leaves with a token that expires in flight. Default 30 s. |
| `token().await -> Result<Secret, AuthError>` | a currently valid token, refreshing if needed |
| `invalidate()` | drop the cached token — call it when the server answers `401` before the stated expiry (revocation) |
| `prepare()` (trait) | fetches or refreshes |
| `apply()` (trait) | copies the cached token; sets nothing if none was fetched yet |

Behaviour:

- **Caching** — the token is reused until `skew` before its expiry. A token
  with no stated lifetime is reused until `invalidate()`.
- **Single flight** — when a refresh is due, concurrent callers share one
  fetch.
- **Graceful refresh** — if a refresh fails while the current token has not
  actually expired, the current token keeps being used and the error is
  swallowed; the next call tries again. Once it has expired, the error is
  returned.
- There is no backoff between failed fetches: each `prepare()` after a failure
  tries once. Put a [`Retry`](#retry) or [`CircuitBreaker`](#circuit-breaking)
  inside your `TokenSource` if the endpoint needs protecting.

### OAuth 2.0 client credentials

`OAuth2ClientCredentials` is a `TokenSource` for the **client-credentials
grant** (RFC 6749 §4.4): the application authenticates as itself, with no user
involved. Needs the `codec` feature.

```rust
use std::sync::Arc;
use ferroly::clients::{authorize, AuthProvider, OAuth2ClientCredentials};
use ferroly::http::{Client, Method, Request};

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let auth: Arc<dyn AuthProvider> = Arc::new(
        OAuth2ClientCredentials::new(
            "https://login.example/oauth2/token",
            "my-client-id",
            std::env::var("CLIENT_SECRET")?,
        )
        .scope("orders:read")
        .into_provider(),          // -> RefreshingAuth
    );

    let mut req = Request::builder(Method::Get, "https://api.example/orders")?.build();
    authorize(auth.as_ref(), &mut req).await?;   // fetches or reuses the token
    let resp = Client::new().send(req).await?;
    println!("{}", resp.status().as_u16());
    Ok(())
}
```

| Method | Notes |
|---|---|
| `OAuth2ClientCredentials::new(token_url, client_id, client_secret)` | the secret is `impl Into<Secret>`; default HTTP client with a 30 s timeout |
| `scope(s)` / `scopes([..])` | sent space-delimited |
| `audience(a)` | the `audience` parameter (Auth0 and others) |
| `param(name, value)` | any extra form parameter (e.g. `resource`) |
| `auth_style(ClientAuthStyle)` | `BasicHeader` (default) or `RequestBody` |
| `with_client(Client)` | custom timeout / shared pool |
| `into_provider() -> RefreshingAuth` | add caching and refresh |
| `fetch_token()` (trait) | one token request, no caching |

**The request** is a `POST` with `content-type:
application/x-www-form-urlencoded`, `accept: application/json` and the body
`grant_type=client_credentials[&scope=…][&audience=…][&…]`. The client
authenticates with `Authorization: Basic base64(form-encode(id) ":"
form-encode(secret))` (RFC 6749 §2.3.1), or — with
`ClientAuthStyle::RequestBody` — with `client_id` and `client_secret` form
fields.

**The response** must be a JSON object with a non-empty string `access_token`.
`token_type`, if present, must be `Bearer` (any case). `expires_in` — a number,
or a numeric string as some servers send — sets the cache lifetime; without it
the token is reused until `invalidate()`.

| Failure | `AuthError` |
|---|---|
| cannot connect, TLS, timeout, malformed HTTP | `Transport(reason)` |
| non-2xx status | `Rejected { status, code }` — `code` is the OAuth `error` value (e.g. `invalid_client`) when the body has one |
| 2xx but not a usable token response | `InvalidResponse(reason)` |
| unparsable token URL, non-UTF-8 secret | `Config(reason)` |

**No credential ever reaches an error or `Debug`**: not the client secret, not
the token, not the response body, not the endpoint URL; the server's
`error_description` is dropped and its `error` code is kept only if it looks
like one. Always use an `https://` token URL.

Other grants (authorization code, PKCE, refresh tokens, device flow) and OIDC
discovery are not implemented.

---

## Retry

### `RetryPolicy` — the backoff schedule

```rust
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,              // retries AFTER the initial attempt
    pub wait: Duration,                // base wait between attempts
    pub exponential: bool,             // double the wait each attempt when true
    pub max_backoff: Option<Duration>, // optional cap on exponential backoff
}
```

`max_retries` counts retries **after** the first attempt, so `3` means up to 4
attempts.

| Constructor | Schedule |
|---|---|
| `RetryPolicy::default()` | 3 retries, 200 ms doubling, capped at 10 s |
| `RetryPolicy::fixed(max_retries, wait)` | always `wait` |
| `RetryPolicy::exponential(max_retries, wait, max_backoff)` | `wait`, `2·wait`, `4·wait`, … capped |
| struct literal | any combination |

`backoff_for(attempt)` returns the un-jittered delay before retry `attempt`
(0-based): `wait * 2^attempt`, saturating, clamped to `max_backoff`. For
`wait = 100ms`, `max_backoff = 500ms`: 100, 200, 400, 500, 500, … ms.

### `Retry` — jitter, time limit, budget, server delays

`Retry` wraps a schedule with runtime behaviour. Build it with
`Retry::new(policy)` or the shortcuts on the policy:

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::clients::{Jitter, Retry, RetryBudget, RetryPolicy};

let budget = Arc::new(RetryBudget::new(10, 1.0));

let retry: Retry = RetryPolicy::default()
    .with_jitter(Jitter::Full)                 // -> Retry
    .max_elapsed(Duration::from_secs(30))
    .budget(budget.clone());
```

| Method | Default | Effect |
|---|---|---|
| `Retry::new(policy)`, `Retry::from(policy)`, `Retry::default()` | | no jitter, no limits |
| `RetryPolicy::with_jitter` / `with_max_elapsed` / `with_budget` | | shortcuts returning a `Retry` |
| `jitter(Jitter)` | `Jitter::None` | randomise each delay |
| `max_elapsed(Duration)` | none | no retry is scheduled whose delay would end later than this after the first attempt started. An attempt in flight is not interrupted — use a timeout for that. |
| `max_retry_after(Option<Duration>)` | 5 minutes | the longest server-requested delay that is honoured; a longer one **stops** the loop. `None` lets a server stall the caller indefinitely. |
| `budget(Arc<RetryBudget>)` | none | every retry draws a token; when empty, stop |
| `policy()`, `delay_for(attempt)` | | the schedule; a (random) jittered delay |
| `run(retry_if, op).await` | | retry while `retry_if(&err)` is true |
| `run_with(decide, op).await` | | retry under a [decision hook](#the-decision-hook-and-retry-after) |

#### Jitter

Without jitter, clients that failed together retry together, and the
synchronised waves can keep a recovering service down.

| `Jitter` | Delay | Use |
|---|---|---|
| `None` | exactly the schedule | deterministic tests |
| `Full` | uniform in `[0, delay)` | the usual choice — spreads load best |
| `Equal` | `delay/2` plus uniform in `[0, delay/2)` | when a minimum wait matters |

The randomness comes from a small internal pseudo-random generator
(SplitMix64) seeded from the clock, address-space layout and the standard
library's per-process hash keys. It is adequate for spreading load and **not
cryptographically secure — never a source of tokens, keys or nonces** (use
[`hash::random`](hash.md#secure-randomness) for those).

#### The decision hook and `Retry-After`

`run_with` calls a hook after every failed attempt that still has a retry
available. It receives the error and a `RetryAttempt` and returns a
`RetryDecision`:

```rust
pub enum RetryDecision { Stop, Retry, RetryAfter(Duration) }

#[non_exhaustive]
pub struct RetryAttempt {
    pub attempt: u32,        // the attempt that just failed, from 1
    pub retries_left: u32,
    pub elapsed: Duration,   // since the first attempt started
    pub delay: Duration,     // what the policy would wait (after jitter)
}
```

`RetryAfter(d)` waits exactly `d` instead of the policy's delay — the way to
honour a server's `Retry-After`. It is not jittered, and it is subject to
`max_retry_after` and `max_elapsed`. `parse_retry_after` reads the header:

```rust
use std::time::Duration;
use ferroly::clients::{parse_retry_after, Retry, RetryDecision, RetryPolicy};

struct ApiError { status: u16, retry_after: Option<String> }

async fn call_api() -> Result<String, ApiError> { Ok(String::new()) }

async fn fetch() -> Result<String, ApiError> {
    let retry = Retry::new(RetryPolicy::default()).max_elapsed(Duration::from_secs(60));
    retry
        .run_with(
            |err: &ApiError, attempt| {
                eprintln!("attempt {} failed with {}", attempt.attempt, err.status);
                match err.status {
                    429 | 503 => match err.retry_after.as_deref().and_then(parse_retry_after) {
                        Some(delay) => RetryDecision::RetryAfter(delay),
                        None => RetryDecision::Retry,
                    },
                    500..=599 => RetryDecision::Retry,
                    _ => RetryDecision::Stop,
                }
            },
            call_api,
        )
        .await
}
```

`parse_retry_after(&str) -> Option<Duration>` accepts both RFC 9110 forms:
delta-seconds (`"120"`) and an IMF-fixdate (`"Wed, 21 Oct 2015 07:28:00 GMT"`,
converted with the system clock; a past date is a zero delay). The obsolete
RFC 850 and asctime date formats return `None`. `RetryDecision` also converts
from `bool` (`true` → `Retry`).

The hook is the place to log or count attempts. It is not called for the final
failure (no retry left) or after a success.

#### `RetryBudget` — preventing retry storms

When a dependency is down, every caller retrying multiplies the load on it. A
`RetryBudget` is a token bucket **shared by all callers of one dependency**:
each retry withdraws a token; when the bucket is empty, callers stop retrying
and return their error at once.

```rust
use std::sync::Arc;
use ferroly::clients::{RetryBudget, RetryPolicy};

// A burst of 10 retries, then 1 retry per second, plus 0.1 token per call
// ("retries may add about 10 % to the request volume").
let budget = Arc::new(RetryBudget::new(10, 1.0).with_deposit_per_call(0.1));
let retry = RetryPolicy::default().with_budget(budget.clone());
```

| Method | Notes |
|---|---|
| `RetryBudget::new(capacity, refill_per_second)` | starts full; `refill_per_second` may be `0.0` |
| `with_deposit_per_call(tokens)` | credit per call that goes through a `Retry` using the budget |
| `try_withdraw() -> bool` | take one token (done by `Retry` per retry) |
| `record_call()` | credit the per-call deposit (done by `Retry` once per `run`) |
| `available() -> f64` | tokens right now |

### The drivers

```rust
pub async fn retry<T, E, P, F, Fut>(policy: &RetryPolicy, retry_if: P, op: F) -> Result<T, E>
where P: Fn(&E) -> bool, F: FnMut() -> Fut, Fut: Future<Output = Result<T, E>>;

pub async fn retry_with<T, E, D, F, Fut>(retry: &Retry, decide: D, op: F) -> Result<T, E>
where D: FnMut(&E, &RetryAttempt) -> RetryDecision, F: FnMut() -> Fut, Fut: Future<Output = Result<T, E>>;
```

`retry` is the original driver: it runs `op` until it returns `Ok`, the retries
are exhausted, or `retry_if(&err)` is `false`, sleeping the policy's exact
backoff between attempts (no jitter). `retry_with` is the free-function form of
`Retry::run_with`. On exhaustion the **last** error is returned.

```rust
use std::time::Duration;
use ferroly::clients::{retry, RetryPolicy};

#[tokio::main]
async fn main() {
    let policy = RetryPolicy::fixed(2, Duration::from_millis(50));
    let mut attempts = 0;

    let result: Result<u32, &str> = retry(&policy, |_e| true, || {
        attempts += 1;
        async move {
            if attempts < 2 { Err("transient") } else { Ok(42) }
        }
    }).await;

    assert_eq!(result, Ok(42));
}
```

This is how [`rest`](rest.md)'s `RequestBuilder::send` retries: it passes a
predicate that returns `true` only for `ClientError::Transport`.

**Only retry what is safe to repeat.** These drivers do not know whether an
operation is idempotent.

---

## Circuit breaking

A circuit breaker stops hammering a failing dependency: it "opens" after
repeated failures, rejects calls for a cooldown, then cautiously probes
recovery.

### States and configuration

```rust
pub enum CircuitState { Closed, Open, HalfOpen }

#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,  // consecutive Closed failures that trip to Open
    pub success_threshold: u32,  // HalfOpen successes needed to close
    pub max_half_open: u32,      // max concurrent trial requests in HalfOpen
    pub timeout: Duration,       // how long Open lasts before trials are allowed
}
```

**Default:** `failure_threshold = 5`, `success_threshold = 2`,
`max_half_open = 1`, `timeout = 30s`.

- **Closed** — calls pass. A success resets the failure run; a failure extends
  it. Reaching `failure_threshold` consecutive failures (or the
  [failure rate](#failure-rate-window)) **trips** the breaker.
- **Open** — calls are rejected with `CircuitOpenError` until `timeout` has
  elapsed. The first call after that moves to `HalfOpen` and is admitted as a
  trial.
- **HalfOpen** — up to `max_half_open` trials run concurrently (at least one).
  `success_threshold` successes **close** the breaker; any failure **re-trips**
  it and restarts the cooldown.

`CircuitBreaker::new(config)` builds a breaker; `CircuitBreaker::builder(config)`
adds a failure-rate window, a trial timeout and state-change callbacks (see
below).

### The permit API (preferred)

`try_acquire()` returns a `Permit` that you resolve with the outcome:

```rust
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig};

async fn call_with_breaker(cb: &CircuitBreaker) -> Result<String, String> {
    let permit = cb.try_acquire().map_err(|_| "circuit open".to_string())?;
    match do_request().await {
        Ok(body) => {
            permit.success();
            Ok(body)
        }
        Err(e) => {
            permit.failure();
            Err(e)
        }
    }
}

async fn do_request() -> Result<String, String> { Ok(String::new()) }
```

| `Permit` method | Effect |
|---|---|
| `success()` | the dependency worked |
| `failure()` | the dependency failed |
| `ignore()` | release without counting either way |
| `record(Outcome)` | `Outcome::{Success, Failure, Ignored}` |
| `record_result(&Result<T, E>)` | `Ok` → success, `Err` → failure |
| `is_trial()` | whether this call is a half-open trial |
| **drop, unresolved** | **recorded as `Outcome::Ignored`** |

**Dropping an unresolved permit releases its half-open trial slot immediately
and counts nothing.** So a caller that is cancelled, times out from outside or
panics can neither leak a slot (leaving the breaker stuck half-open) nor trip
the breaker. If you *want* a cancellation or an external timeout to count
against the dependency, call `failure()` before dropping — or use an
[attempt timeout](#policy-the-composed-pipeline), which does.

A permit owns a handle to the breaker: it is `Send + 'static`, so it can move
into a spawned task or be held for the duration of a streamed response. If the
breaker changes state before a permit is resolved, that permit's outcome is
discarded.

### `call` and `call_with`

```rust
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, Outcome, PolicyError};

async fn get(cb: &CircuitBreaker) -> Result<String, PolicyError<u16>> {
    cb.call_with(
        // 4xx is the caller's problem: it must not open the breaker for everyone.
        |status: &u16| if *status >= 500 { Outcome::Failure } else { Outcome::Ignored },
        || async { Err::<String, u16>(404) },
    )
    .await
}
```

- `call(op)` — rejects with `PolicyError::CircuitOpen` when not admitted;
  otherwise `Ok` is a success and every `Err` a failure (returned as
  `PolicyError::Inner`).
- `call_with(classify, op)` — `classify(&err)` decides how each error counts.
  **Classify client errors (`400`, `401`, `403`, `404`, `422`) as
  `Outcome::Ignored`** so a few bad requests cannot open the breaker for every
  other caller.
- If the returned future is dropped mid-call, the call is recorded as ignored.

### Failure-rate window

The consecutive count misses a dependency that fails *often* but not *every
time*. A `FailureRate` adds a second trip condition over a sliding time window:

```rust
use std::time::Duration;
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, FailureRate};

let cb = CircuitBreaker::builder(CircuitBreakerConfig::default())
    .failure_rate(FailureRate {
        window: Duration::from_secs(60),
        min_calls: 20,     // do not judge on fewer outcomes than this
        threshold: 0.5,    // trip when >= 50 % of them failed
    })
    .half_open_timeout(Duration::from_secs(10))
    .on_state_change(|from, to| eprintln!("breaker: {from:?} -> {to:?}"))
    .build();
```

The breaker trips when, within the last `window`, at least `min_calls` outcomes
were recorded and the failed fraction is at least `threshold`. Both conditions
are active together; to trip on the rate only, set `failure_threshold` to
`u32::MAX`. The window is tracked in ten buckets (outcomes age out with a
granularity of a tenth of the window) and is cleared on every state change.
Ignored outcomes are not in the window.

### `CircuitBreakerBuilder`

| Method | Notes |
|---|---|
| `failure_rate(FailureRate)` | see above |
| `half_open_timeout(Duration)` | how long an admitted trial may stay unreported before its slot is given to another trial. Default: the cooldown (`timeout`). Set it above the longest time a healthy call can take. |
| `on_state_change(Fn(CircuitState, CircuitState))` | called with `(from, to)` on every transition, on the thread that caused it, **after** the breaker's lock is released (so it may use the breaker). Keep it short. May be registered several times. |
| `build()` | |

### Metrics and control

| Method | Notes |
|---|---|
| `state()` | current `CircuitState`. An `Open` breaker whose cooldown has elapsed still reports `Open` until the next admission request. |
| `metrics() -> BreakerMetrics` | `state`, `successes`, `failures`, `rejected`, `trips`, `consecutive_failures`, `half_open_in_flight` |
| `reset()` | force `Closed`, clearing counters and window (operator override) |

### Legacy protocol: `can_execute` / `on_execution`

```rust
pub fn can_execute(&self) -> Result<(), CircuitOpenError>
pub fn on_execution(&self, success: bool)
```

The original check-then-report pair still works and is what existing callers
use. Because nothing ties the report to the check, a caller that is admitted as
a half-open trial and never reports would hold the slot. Such a slot is now
**reclaimed after `half_open_timeout`**, so the breaker recovers instead of
staying half-open forever — but until then that trial slot is unavailable.
Prefer the permit API, which releases the slot the moment the caller goes away.

```rust
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig};

let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
if cb.can_execute().is_ok() {
    // let outcome = do_the_call().await;
    cb.on_execution(true);
}
```

### `CircuitBreakerRegistry` — one breaker per key

```rust
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerRegistry};

let breakers = CircuitBreakerRegistry::new(CircuitBreakerConfig::default());
let permit = breakers.get("api.example.com").try_acquire().unwrap();
permit.success();

// Per-key configuration and callbacks that know the key:
let breakers = CircuitBreakerRegistry::with_factory(|key| {
    let key = key.to_string();
    CircuitBreaker::builder(CircuitBreakerConfig::default())
        .on_state_change(move |from, to| eprintln!("{key}: {from:?} -> {to:?}"))
        .build()
});
```

| Method | Notes |
|---|---|
| `new(config)` / `with_factory(Fn(&str) -> CircuitBreaker)` | |
| `get(key) -> Arc<CircuitBreaker>` | created on first use; the same key returns the same breaker |
| `get_existing(key)` | `None` if never created |
| `remove(key)`, `clear()` | |
| `keys()`, `len()`, `is_empty()` | |
| `states() -> Vec<(String, CircuitState)>` | sorted by key — for a health endpoint |

Keys are never evicted automatically. If they are unbounded (derived from
request data), remove them yourself.

---

## Bulkhead

Limits how many operations run at once, so one slow dependency cannot absorb
every task and connection of the caller.

```rust
use std::time::Duration;
use ferroly::clients::Bulkhead;

async fn example() {
    let bulkhead = Bulkhead::new(16).with_queue_timeout(Duration::from_millis(200));

    match bulkhead.run(async { /* the call */ 42 }).await {
        Ok(value) => println!("{value}"),
        Err(_full) => println!("shed"),   // BulkheadFullError
    }
}
```

| Method | Notes |
|---|---|
| `Bulkhead::new(max_concurrent)` | at least 1. Callers beyond the limit **wait without limit**. |
| `with_queue_timeout(Duration)` | wait at most this long for a slot, then `BulkheadFullError` |
| `fail_fast()` | a zero queue timeout: reject immediately when full (load shedding) |
| `run(future).await` | run in a slot; the future is not polled if no slot is obtained |
| `acquire().await` / `try_acquire()` | hold a `BulkheadPermit` yourself; the slot frees when it is dropped |
| `available()`, `in_flight()`, `max_concurrent()` | |

Waiters are served in FIFO order. Cancelling a waiting or running call releases
its place. Built on `tokio::sync::Semaphore`.

## Rate limiter

A client-side token bucket: stay under a quota the *server* enforces, instead
of collecting `429`s.

```rust
use ferroly::clients::RateLimiter;

async fn example() {
    let limiter = RateLimiter::new(50.0, 10);   // 50 per second, bursts of 10
    limiter.acquire().await;                    // waits its turn
    if limiter.try_acquire() { /* got one without waiting */ }
}
```

| Method | Notes |
|---|---|
| `RateLimiter::new(rate_per_second, burst)` | the bucket holds `burst` tokens (at least 1) and starts full |
| `RateLimiter::per(count, period)` | `count` per `period`, burst `count` |
| `acquire().await` / `acquire_many(n).await` | wait until the tokens are earned |
| `try_acquire()` / `try_acquire_many(n)` | take them only if available now |
| `acquire_timeout(max_wait).await` | take one if it can be had within `max_wait`; otherwise `RateLimitedError` **immediately**, taking nothing |
| `available() -> f64` | negative while callers are queued |

Waiters are served in arrival order: each reserves its token up front and
sleeps until it has been earned. A caller cancelled while waiting does **not**
return its reservation, so the pace stays at or below the configured rate. A
rate of zero (or a non-finite one) never refills: after the initial burst,
`try_acquire` always refuses and `acquire` waits forever.

## Timeouts and deadlines

```rust
use std::time::Duration;
use ferroly::clients::{with_timeout, Deadline, TimeoutError};

async fn step() -> u32 { 1 }

async fn example() -> Result<u32, TimeoutError> {
    // One operation, one limit:
    let a = with_timeout(Duration::from_secs(2), step()).await?;

    // One limit shared by every step of a request:
    let deadline = Deadline::after(Duration::from_secs(5));
    let b = deadline.run(step()).await?;
    let c = deadline.run(step()).await?;   // gets what is left of the 5 s
    Ok(a + b + c)
}
```

- `with_timeout(limit, future)` — `Err(TimeoutError)` if the future has not
  finished after `limit`. The future is dropped, which cancels it at its next
  `.await`; work that cannot be cancelled that way (a blocking call, a request
  the server already started) keeps running — only the wait is abandoned.
- `Deadline` — a fixed instant, `Copy`, passed down through the steps of one
  request so the total stays bounded: `Deadline::after(d)`, `Deadline::at(i)`,
  `remaining()`, `is_expired()`, `check()`, `min_with(d)` (the earlier of this
  deadline and `d` from now), `instant()`, `run(future)`.

Both use tokio's clock, so they follow a paused test clock.

## `Policy`: the composed pipeline

`Policy` composes the primitives around an async operation in a fixed order,
outermost first:

| # | Stage | Builder | What it does |
|---|---|---|---|
| 1 | overall timeout | `timeout(d)` | bounds everything below — all attempts, backoff, queueing |
| 2 | retry | `retry(impl Into<Retry>)` | a `Retry` (or plain `RetryPolicy`) around each attempt |
| 3 | circuit breaker | `circuit_breaker(Arc<CircuitBreaker>)` | each attempt takes a permit and reports its outcome |
| 4 | bulkhead | `bulkhead(Arc<Bulkhead>)` | each attempt takes a slot |
| 5 | rate limiter | `rate_limiter(Arc<RateLimiter>)` | each attempt waits for a token |
| 6 | attempt timeout | `attempt_timeout(d)` | bounds one run of the operation |

Every stage is optional; an empty `Policy` just runs the operation.

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::clients::{
    parse_retry_after, Bulkhead, CircuitBreaker, CircuitBreakerConfig, ErrorClass, Jitter,
    Policy, PolicyError, RetryPolicy,
};

struct ApiError { status: u16, retry_after: Option<String> }

async fn call_api() -> Result<String, ApiError> { Ok(String::new()) }

fn classify(err: &ApiError) -> ErrorClass {
    match err.status {
        429 | 503 => match err.retry_after.as_deref().and_then(parse_retry_after) {
            Some(delay) => ErrorClass::retry_after(delay),
            None => ErrorClass::transient(),
        },
        500..=599 => ErrorClass::transient(),
        _ => ErrorClass::caller_error(),   // 4xx: not retried, not counted
    }
}

async fn example() -> Result<String, PolicyError<ApiError>> {
    // Build once per dependency and share.
    let policy = Policy::new()
        .timeout(Duration::from_secs(30))
        .retry(RetryPolicy::default().with_jitter(Jitter::Full))
        .circuit_breaker(Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default())))
        .bulkhead(Arc::new(Bulkhead::new(32)))
        .attempt_timeout(Duration::from_secs(5));

    policy.execute_with(classify, call_api).await
}
```

- `execute(op)` treats every error of the operation as
  `ErrorClass::transient()`.
- `execute_with(classify, op)` lets `classify(&err)` decide **two independent
  things** per error — whether to retry it and how the breaker counts it:

| `ErrorClass` | Retry | Breaker | Typical errors |
|---|---|---|---|
| `transient()` | yes | failure | timeouts, connection resets, `502`/`503`/`504` |
| `retry_after(delay)` | yes, after exactly `delay` | failure | `429`/`503` with `Retry-After` |
| `fatal()` | no | failure | `500` on a non-idempotent call |
| `caller_error()` | no | **ignored** | `400`, `401`, `403`, `404`, `422` |

  (or build one from its public fields `retry: RetryDecision` and
  `outcome: Outcome`).

How the stages interact:

- A **rejection** — open circuit (`PolicyError::CircuitOpen`) or full bulkhead
  (`PolicyError::BulkheadFull`) — fails the call at once and is **never
  retried**: both are load-shedding signals. A bulkhead rejection is not
  counted by the breaker.
- An **attempt timeout** is a breaker failure and is retried.
- The **overall timeout** cancels whatever is in flight; that attempt's permit
  is dropped unresolved, so it is *not* a breaker failure. Use an attempt
  timeout if slow calls should count.
- Each attempt is reported to the breaker individually, so the breaker can open
  in the middle of a retry loop and stop it.
- Cancelling the `execute` future releases the breaker permit and bulkhead
  slot.
- `op` is called once per attempt to create that attempt's future; the future
  is created before the breaker and bulkhead are consulted and dropped unpolled
  if they reject. `op` itself should therefore only *build* the future.

`Policy` is `Clone`; clones share the breaker, bulkhead and limiter. Share one
of each (in an `Arc`) between every policy that talks to the same dependency.

---

## Error handling

| Type | Returned by |
|---|---|
| `CircuitOpenError` | `CircuitBreaker::can_execute`, `try_acquire` — breaker open, or half-open with every trial slot in use |
| `BulkheadFullError` | `Bulkhead::acquire`, `try_acquire`, `run` |
| `TimeoutError` | `with_timeout`, `Deadline::run`, `Deadline::check` |
| `RateLimitedError` | `RateLimiter::acquire_timeout` |
| `PolicyError<E>` | `Policy::execute*`, `CircuitBreaker::call*` |
| `AuthError` | `AuthProvider::prepare`, `authorize`, `TokenSource::fetch_token` |

```rust
#[non_exhaustive]
pub enum PolicyError<E> {
    Timeout,        // overall deadline or attempt timeout
    CircuitOpen,    // not attempted
    BulkheadFull,   // not attempted
    Inner(E),       // the operation's own error
}
```

`PolicyError::into_inner() -> Option<E>`, `is_rejection()` (`CircuitOpen` or
`BulkheadFull`); `From<CircuitOpenError>`, `From<BulkheadFullError>`,
`From<TimeoutError>`. It implements `Display` when `E: Display` and
`std::error::Error` when `E` does.

```rust
#[non_exhaustive]
pub enum AuthError {
    Transport(String),                              // could not reach the token endpoint
    Rejected { status: u16, code: Option<String> }, // non-2xx; code = OAuth "error"
    InvalidResponse(String),                        // 2xx but unusable
    Config(String),                                 // provider misconfigured
    Other(String),                                  // custom TokenSource failures
}
```

`AuthError` messages never contain credential values.

The `retry` / `Retry` drivers add no error type: they return the last `E` of
your operation.

## Limitations

- **No hedging and no fallback** stage. Hedged requests and fallbacks are left
  to the caller.
- **Retries are not idempotency-aware.** The drivers retry whatever the
  predicate, hook or `ErrorClass` says to; do not let them repeat a
  non-idempotent operation.
- **Process-local state.** Breakers, budgets, bulkheads and rate limiters are
  per process; nothing is coordinated across instances.
- **Jitter is not cryptographic** randomness.
- **`AuthProvider::apply` alone does not refresh.** Callers must `prepare()`
  first (or use `authorize`); code that calls only `apply` works with static
  providers but sends no credentials for a refreshing one that has not fetched
  a token yet.
- **OAuth 2.0**: client-credentials grant only; no authorization code, PKCE,
  refresh tokens, device flow, mTLS or `private_key_jwt` client authentication,
  and no OIDC discovery.
- `Secret`'s wipe-on-drop is best-effort (see [hash](hash.md#secret)); a
  header value copied into a `Request` is an ordinary `String`.

## See also

- [rest](rest.md) — consumes `AuthProvider` and `RetryPolicy` in its `Client`/`ClientOptions`
  and drives requests through `retry`.
- [genai](genai.md) — the other consumer of these resilience primitives.
- [http](http.md) — the `Request` that `AuthProvider::apply` mutates.
- [auth](auth.md) — *verifying* tokens (JWT, JWKS); this page is about *sending* credentials.
- [hash](hash.md) — `Secret`, base64 and secure randomness.
- [derive](derive.md) — the `FerrolyError` derive behind the error types.

---
**Related:** [rest](rest.md), [genai](genai.md), [http](http.md), [auth](auth.md), [hash](hash.md), [derive](derive.md).
