# ferroly::clients

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `clients` (implies `http` + `tokio`).

The resilience toolkit shared by ferroly's HTTP-based code, dependency-minimal and built
entirely in-house. It provides three orthogonal primitives:

- **Auth providers** — [`AuthProvider`](#auth-providers) and the [`BearerAuth`](#bearerauth) /
  [`ApiKeyAuth`](#apikeyauth) / [`BasicAuth`](#basicauth) implementations, applied to
  outbound [`ferroly::http::Request`](http.md)s by setting headers.
- **Retry** — [`RetryPolicy`](#retrypolicy) and the generic [`retry`](#the-retry-driver)
  driver.
- **Circuit breaking** — [`CircuitBreaker`](#circuitbreaker) with the classic
  closed/open/half-open state machine, plus [`CircuitBreakerConfig`](#circuitbreakerconfig),
  [`CircuitState`](#circuitstate), and [`CircuitOpenError`](#error-handling).

These are the building blocks that [`rest`](rest.md) and [`genai`](genai.md) both reuse:
`rest`'s `ClientOptions` takes an `Arc<dyn AuthProvider>` and a `RetryPolicy`, and its
`RequestBuilder::send` runs under the `retry` driver.

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["clients"] }
```

Public exports:

```rust
pub use ferroly::clients::{ApiKeyAuth, AuthProvider, BasicAuth, BearerAuth};
pub use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, CircuitState};
pub use ferroly::clients::{retry, RetryPolicy};
pub use ferroly::clients::CircuitOpenError;
```

---

## Auth providers

### The `AuthProvider` trait

```rust
pub trait AuthProvider: Send + Sync {
    fn apply(&self, req: &mut Request);
}
```

An `AuthProvider` mutates an outbound [`ferroly::http::Request`](http.md) in place — in
practice, by setting one or more headers. The trait is intentionally minimal: a single
`apply(&mut Request)`. Schemes are distinguished by their concrete type, not a runtime tag.

Providers are `Send + Sync` and are shared as `Arc<dyn AuthProvider>`, so a client (or a
GenAI provider) can be configured with any scheme without changing its own type:

```rust
use std::sync::Arc;
use ferroly::clients::{AuthProvider, BearerAuth};

let auth: Arc<dyn AuthProvider> = Arc::new(BearerAuth::new("token"));
// e.g. ClientOptions::builder().auth(auth) — see rest.md
```

You can implement the trait yourself for a custom scheme:

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

### `BearerAuth`

Sets `Authorization: Bearer <token>`.

```rust
use ferroly::clients::BearerAuth;

let auth = BearerAuth::new("my-secret-token");
// => Authorization: Bearer my-secret-token
```

`BearerAuth::new(impl Into<String>) -> Self`. `Debug + Clone`.

### `ApiKeyAuth`

Places a key in a caller-specified header (`header: key`).

```rust
use ferroly::clients::ApiKeyAuth;

let auth = ApiKeyAuth::new("x-api-key", "abc123");
// => x-api-key: abc123
```

`ApiKeyAuth::new(header: impl Into<String>, key: impl Into<String>) -> Self`. `Debug + Clone`.

### `BasicAuth`

HTTP Basic authentication — sets `Authorization: Basic <base64(user:pass)>` using an
in-house base64 encoder (no external crate).

```rust
use ferroly::clients::BasicAuth;

let auth = BasicAuth::new("alice", "s3cr3t");
// => Authorization: Basic YWxpY2U6czNjcjN0
```

`BasicAuth::new(user: impl Into<String>, pass: impl Into<String>) -> Self`. `Debug + Clone`.

---

## Retry

### `RetryPolicy`

Configures a bounded retry loop with optional exponential backoff. All fields are public.

```rust
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,             // retries AFTER the initial attempt
    pub wait: Duration,               // base wait between attempts
    pub exponential: bool,            // double the wait each attempt when true
    pub max_backoff: Option<Duration>,// optional cap on exponential backoff
}
```

`max_retries` counts retries **after** the first attempt, so a value of `3` means up to 4
total attempts.

**Default** (`RetryPolicy::default()`): `max_retries = 3`, `wait = 200ms`,
`exponential = true`, `max_backoff = Some(10s)`.

#### `RetryPolicy::fixed`

```rust
pub fn fixed(max_retries: u32, wait: Duration) -> RetryPolicy
```

A fixed-wait policy — no backoff growth. Equivalent to
`exponential = false`, `max_backoff = None`:

```rust
use std::time::Duration;
use ferroly::clients::RetryPolicy;

let fixed = RetryPolicy::fixed(5, Duration::from_millis(100));  // 5 retries, always 100ms
```

#### Exponential backoff

When `exponential = true`, the delay before retry `attempt` (0-based) is
`wait * 2^attempt`, saturating and then clamped to `max_backoff` if set. For
`wait = 100ms`, `max_backoff = 500ms`:

| retry attempt (0-based) | delay |
| --- | --- |
| 0 | 100ms |
| 1 | 200ms |
| 2 | 400ms |
| 3 | 500ms (capped) |
| 4+ | 500ms (capped) |

Construct a custom exponential policy with a struct literal:

```rust
use std::time::Duration;
use ferroly::clients::RetryPolicy;

let policy = RetryPolicy {
    max_retries: 6,
    wait: Duration::from_millis(250),
    exponential: true,
    max_backoff: Some(Duration::from_secs(5)),
};
```

> There is no random jitter component — backoff is deterministic. If you need jitter to
> avoid thundering-herd retries, layer it into your own `retry_if` timing or add randomness
> around the call.

### The `retry` driver

```rust
pub async fn retry<T, E, P, F, Fut>(
    policy: &RetryPolicy,
    retry_if: P,
    mut op: F,
) -> Result<T, E>
where
    P: Fn(&E) -> bool,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
```

A generic async retry loop over **any** fallible operation, not just HTTP. It runs `op`
until it returns `Ok`, the retries are exhausted, or `retry_if(&err)` returns `false` (fail
fast). On exhaustion, the **last** error is returned. Between attempts it sleeps for
`policy.backoff_for(attempt)` via `tokio::time::sleep`.

- `policy` — the `RetryPolicy` above.
- `retry_if` — a predicate deciding whether a given error is retryable. Return `false` to
  stop immediately (e.g. for a `4xx` or a validation error); return `true` to keep retrying.
- `op` — an `FnMut` closure producing the future for each attempt (called once per attempt).

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

Fail-fast on a non-retryable error:

```rust
# use ferroly::clients::{retry, RetryPolicy};
# use std::time::Duration;
# #[tokio::main] async fn main() {
let policy = RetryPolicy::fixed(5, Duration::from_millis(1));
let result: Result<(), &str> = retry(&policy, |e| *e != "fatal", || async { Err("fatal") }).await;
assert_eq!(result, Err("fatal"));   // stops after the first attempt
# }
```

This is exactly how [`rest`](rest.md)'s `RequestBuilder::send` retries: it passes a
predicate that returns `true` only for `ClientError::Transport`.

---

## Circuit breaking

A circuit breaker stops hammering a failing dependency by "opening" after repeated failures,
rejecting calls for a cooldown, then cautiously probing recovery.

### `CircuitState`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,    // requests flow normally; failures are counted
    Open,      // requests are rejected until the cooldown elapses
    HalfOpen,  // a limited number of trial requests probe recovery
}
```

### `CircuitBreakerConfig`

Tuning for the breaker. All fields public.

```rust
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,  // consecutive Closed failures that trip to Open
    pub success_threshold: u32,  // HalfOpen successes needed to close
    pub max_half_open: u32,      // max concurrent trial requests in HalfOpen
    pub timeout: Duration,       // how long Open lasts before trials are allowed
}
```

**Default:** `failure_threshold = 5`, `success_threshold = 2`, `max_half_open = 1`,
`timeout = 30s`.

### `CircuitBreaker`

A concurrency-safe breaker (an internal `Mutex` guards the state; it is `Debug`). Usage is
the classic check-then-report protocol:

```rust
pub fn new(config: CircuitBreakerConfig) -> CircuitBreaker
pub fn state(&self) -> CircuitState
pub fn can_execute(&self) -> Result<(), CircuitOpenError>
pub fn on_execution(&self, success: bool)
```

| Method | Purpose |
| --- | --- |
| `new(config)` | Build a breaker starting `Closed`. |
| `state()` | Current `CircuitState`. |
| `can_execute()` | Ask permission before an operation. `Ok(())` to proceed, `Err(CircuitOpenError)` to skip. Transitions `Open → HalfOpen` once `timeout` elapses, and admits/counts half-open trial slots. |
| `on_execution(success)` | Report the outcome of an operation that `can_execute` admitted. |

Call `can_execute` before the call and `on_execution` after:

```rust
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig};

let cb = CircuitBreaker::new(CircuitBreakerConfig::default());

if cb.can_execute().is_ok() {
    // let outcome = do_the_call().await;
    let ok = true; // outcome.is_ok()
    cb.on_execution(ok);
} else {
    // breaker is Open — short-circuit (serve a fallback / return an error)
}
```

#### State machine

- **Closed:** calls pass. Each success resets the failure counter; each failure increments
  it. Reaching `failure_threshold` consecutive failures **trips** the breaker to `Open`.
- **Open:** `can_execute` rejects with `CircuitOpenError` until `timeout` has elapsed since
  the breaker opened. The first `can_execute` after the cooldown transitions to `HalfOpen`
  and admits one trial. (A stray failure reported while `Open` refreshes the cooldown.)
- **HalfOpen:** up to `max_half_open` trial requests are admitted concurrently. Each success
  increments the success counter; reaching `success_threshold` **closes** the breaker
  (counters reset). Any failure immediately **re-trips** it to `Open`.

Full check-execute-report loop with the driver-style pattern:

```rust
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig};

async fn call_with_breaker(cb: &CircuitBreaker) -> Result<String, String> {
    cb.can_execute().map_err(|_| "circuit open".to_string())?;
    let result = do_request().await;       // your fallible op
    cb.on_execution(result.is_ok());
    result
}
# async fn do_request() -> Result<String, String> { Ok(String::new()) }
```

---

## Error handling

```rust
#[derive(Debug, Clone, FerrolyError)]
#[error("circuit breaker is open; execution not permitted")]
pub struct CircuitOpenError;
```

`CircuitOpenError` is the only error type this crate defines — returned by
`CircuitBreaker::can_execute` when the breaker is `Open` (or `HalfOpen` at its trial
capacity). It is a zero-field unit struct, `Debug + Clone`, and implements
`std::error::Error`/`Display` via the [`FerrolyError`](derive.md) derive.

The [`retry`](#the-retry-driver) driver is generic over the caller's error type `E` and adds
no error type of its own — it returns the last `E` from your operation on exhaustion.

## Composing the primitives

The three primitives are independent and compose freely. A robust client call typically
layers them: check the breaker, retry transient failures, and attach auth to each request.
[`rest`](rest.md) wires auth and retry into its `Client`; you add a breaker around the whole
`send` if you want one:

```rust
use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, RetryPolicy, retry};

async fn resilient_call(cb: &CircuitBreaker, policy: &RetryPolicy) -> Result<(), String> {
    cb.can_execute().map_err(|_| "circuit open".to_string())?;
    let out = retry(policy, |e: &String| e == "transient", || async {
        // an operation whose auth is applied by the http Client's AuthProvider
        Err("transient".to_string())
    }).await;
    cb.on_execution(out.is_ok());
    out
}
```

## Limitations

- **No jitter** in `RetryPolicy` backoff — delays are deterministic.
- **Manual breaker protocol.** `CircuitBreaker` is check-then-report; it is not
  automatically integrated into `retry` or `rest`'s client — you wire it in yourself.
- **`retry` retries on error only.** It has no notion of an HTTP status code; map a "bad
  status" into your error type first if you want it retried (this is what
  [`rest`](rest.md) does, treating only transport failures as retryable).

## See also

- [rest](rest.md) — consumes `AuthProvider` and `RetryPolicy` in its `Client`/`ClientOptions`
  and drives requests through `retry`.
- [genai](genai.md) — the other consumer of these resilience primitives.
- [http](http.md) — the `Request` that `AuthProvider::apply` mutates.
- [derive](derive.md) — the `FerrolyError` derive behind `CircuitOpenError`.

---
**Related:** [rest](rest.md), [genai](genai.md), [http](http.md), [derive](derive.md).
