# ferroly::errutils

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `errutils` (enabled by default) — module `ferroly::errutils`. No dependencies.

## Overview

`errutils` holds three small, independent error tools:

| Tool | Use it when |
|---|---|
| [`MultiError`](#multierror) | a batch should report *all* its failures, not just the first |
| [`Context`](#context-context--with_context) (`.context("…")`) | an error needs a "what was I doing" message without losing the original |
| [`Classify` / `ErrorKind`](#classification-classify--errorkind) | callers must decide "retry?" or "whose fault?" without matching every error type |

For the complementary *single typed error* case, the
[`#[derive(FerrolyError)]`](derive.md) macro is re-exported at the crate root as
`ferroly::FerrolyError` — available with only `errutils` (no `codec` needed). It
generates `Display` from `#[error("…")]` format strings, `source()` chaining, and
`From` conversions for `#[from]` fields, so the derived type works with `?`
directly:

```rust
#[derive(Debug, ferroly::FerrolyError)]
pub enum StoreError {
    #[error("object {key} not found")]
    NotFound { key: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

## Enabling

`errutils` is on by default:

```toml
[dependencies]
ferroly = "0.4"                       # errutils + codec are the defaults
# or, explicitly / minimally:
ferroly = { version = "0.4", default-features = false, features = ["errutils"] }
```

## Quick start

```rust
use ferroly::errutils::{Context, MultiError};

// Collect every failure of a batch.
let mut errs = MultiError::new();
errs.push(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
errs.push_msg("validation failed");
assert_eq!(errs.len(), 2);
errs.into_result().unwrap_err();

// Say what you were doing when something failed.
let err = std::fs::read_to_string("/no/such/file")
    .context("loading the license")
    .unwrap_err();
assert_eq!(err.to_string(), "loading the license");
assert!(std::error::Error::source(&err).is_some()); // the io::Error is still there
```

## `MultiError`

An aggregate error holding a `Vec` of boxed errors. Derives `Debug` and `Default`
(the default is an empty aggregate), and implements `std::error::Error`.

| Method | Description |
|---|---|
| `MultiError::new() -> MultiError` | Create an empty aggregate. |
| `push<E: Into<BoxError>>(&mut self, err: E)` | Append any error that converts into `BoxError`. |
| `push_msg<S: Into<String>>(&mut self, msg: S)` | Append a plain string message as an error. |
| `is_empty(&self) -> bool` | Whether no errors have been collected. |
| `len(&self) -> usize` | Number of collected errors. |
| `errors(&self) -> &[BoxError]` | Borrow the collected errors as a slice. |
| `iter(&self) -> slice::Iter<'_, BoxError>` | Iterate over the collected errors in insertion order. |
| `first(&self) -> Option<&BoxError>` | The first collected error. |
| `into_errors(self) -> Vec<BoxError>` | Take the collected errors out. |
| `into_result(self) -> Result<(), MultiError>` | `Ok(())` if empty, else `Err(self)`. |

### `BoxError`

```rust
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
```

The element type held by `MultiError`. Because it is `Send + Sync + 'static`, a
`MultiError` can cross thread and async-task boundaries.

### Trait implementations

- **`std::error::Error`** — a `MultiError` is a first-class error; return it from
  functions, box it, or wrap it in another error enum. `source()` returns the
  **first** collected error, so a generic reporter that walks the `source()` chain
  reaches a real cause (use `iter()` to see all of them).
- **`Display`** — renders a header plus one indented line per contained error, e.g.:

  ```text
  2 error(s) occurred:
    [1] disk full
    [2] validation failed
  ```

- **`FromIterator<E>`** for any `E: Into<BoxError>` — `collect()` an iterator of
  errors straight into an aggregate.
- **`Extend<E>`** for any `E: Into<BoxError>` — merge more errors in one call
  (boxed or not).
- **`From<Vec<E>>`** for any `E: Into<BoxError>`.
- **`IntoIterator`** for `MultiError` (yields `BoxError`) and for `&MultiError`
  (yields `&BoxError`), so `for e in &errs { … }` works.

### `push` vs. `push_msg`

- `push` accepts **any** error convertible into `BoxError` — `std::io::Error`,
  another module's error enum, a `MultiError`, and so on. Use it when you already
  hold an error value.
- `push_msg` accepts a **string** and wraps it in an internal minimal error type.
  Use it for ad-hoc, context-only messages where no underlying error object exists.

### `into_result` — the batch idiom

`into_result` consumes the aggregate and collapses it into a `Result`, which lets a
fallible batch end with a single `?`:

```rust
use ferroly::errutils::MultiError;

fn validate_all(items: &[Item]) -> Result<(), MultiError> {
    let mut errs = MultiError::new();
    for item in items {
        if let Err(e) = validate(item) {
            errs.push(e);          // collect, don't bail out
        }
    }
    errs.into_result()            // Ok(()) if every item passed
}
```

Every item is checked even when earlier ones fail, so the caller sees the complete
set of problems rather than just the first.

### Collecting and combining

```rust
use ferroly::errutils::{BoxError, MultiError};

// collect() the failures of a batch
let failures: MultiError = ["1", "x", "3", "y"]
    .iter()
    .filter_map(|s| s.parse::<u8>().err())
    .collect();
assert_eq!(failures.len(), 2);

// extend with more errors — concrete or already boxed
let mut all = MultiError::new();
all.extend(failures);                       // a MultiError is IntoIterator
let more: Vec<BoxError> = vec!["shard 1 failed".into(), "shard 2 failed".into()];
all.extend(more);
assert_eq!(all.len(), 4);

for e in &all {
    eprintln!("- {e}");
}
```

## Context: `.context()` / `.with_context()`

The `Context` extension trait is implemented for every `Result<T, E>` whose error
converts into `BoxError` (any `std::error::Error + Send + Sync + 'static`, a
`String`, a `&str`, a `BoxError`) and for `Option<T>`.

| Method | Description |
|---|---|
| `context<C: Display>(self, context: C) -> Result<T, ContextError>` | Wrap the error with a message. |
| `with_context<C: Display, F: FnOnce() -> C>(self, f: F) -> Result<T, ContextError>` | Same, but the message is built only on the error path. |

On an `Option`, `None` becomes a `ContextError` with no source.

```rust
use ferroly::errutils::{chain, Context, ContextError};

fn read_port(path: &str) -> Result<u16, ContextError> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {path}"))?;   // lazy: only on failure
    text.trim().parse::<u16>().context("parsing the port")
}

let err = read_port("/no/such/file").context("starting the server").unwrap_err();

// `{}` shows the outermost message; `{:#}` the whole chain.
assert_eq!(err.to_string(), "starting the server");
println!("{err:#}"); // starting the server: reading /no/such/file: No such file or directory (os error 2)

// Every layer, outermost first.
for cause in chain(&err) {
    println!("- {cause}");
}
```

### `ContextError`

| Method | Description |
|---|---|
| `ContextError::new(context, source) -> ContextError` | Wrap an error by hand. |
| `ContextError::msg(context) -> ContextError` | A context error with no source. |
| `context(&self) -> &str` | The message attached by this layer. |
| `root_cause(&self) -> &(dyn Error + 'static)` | The innermost error of the chain. |
| `into_source(self) -> Option<BoxError>` | Take the wrapped error back out. |

`ContextError` implements `std::error::Error` (`source()` is the wrapped error), is
`Send + Sync + 'static`, and is itself an ordinary error — so `.context()` calls
stack, each adding one message and one link to the chain. Downcast the links with
`downcast_ref` to get a concrete type back.

`chain(&err)` returns an iterator (`Chain`) over an error and everything reachable
through `source()`; it works on any error, not only `ContextError`.

## Classification: `Classify` / `ErrorKind`

`ErrorKind` is a deliberately small, transport-neutral set of categories:

| `ErrorKind` | Meaning | `is_retryable()` | `is_user_error()` | `http_status()` |
|---|---|---|---|---|
| `InvalidInput` | malformed or invalid input | no | yes | 400 |
| `Unauthenticated` | missing, expired or invalid credentials | no | yes | 401 |
| `PermissionDenied` | authenticated but not allowed | no | yes | 403 |
| `NotFound` | the thing asked for does not exist | no | yes | 404 |
| `Conflict` | conflicts with the current state | no | yes | 409 |
| `Unsupported` | operation, format or capability not supported | no | yes | 501 |
| `RateLimited` | throttled; try again later | yes | no | 429 |
| `Timeout` | did not finish in time | yes | no | 504 |
| `Unavailable` | a dependency is unreachable or overloaded | yes | no | 503 |
| `Internal` | unexpected failure inside the system | no | no | 500 |
| `Unknown` | not classified | no | no | 500 |

`ErrorKind` is `Copy`, `Eq`, `Hash` and `#[non_exhaustive]`; `as_str()` / `Display`
give a stable lowercase name (`"rate_limited"`) for logs and metric labels.

`Classify` is the trait an error implements to report its kind:

```rust
pub trait Classify {
    fn kind(&self) -> ErrorKind;
    fn is_retryable(&self) -> bool { self.kind().is_retryable() }
    fn is_user_error(&self) -> bool { self.kind().is_user_error() }
}
```

```rust
use ferroly::errutils::{Classify, ErrorKind};

#[derive(Debug)]
enum StoreError { Missing, Busy }

impl Classify for StoreError {
    fn kind(&self) -> ErrorKind {
        match self {
            StoreError::Missing => ErrorKind::NotFound,
            StoreError::Busy => ErrorKind::Unavailable,
        }
    }
}

assert!(StoreError::Busy.is_retryable());
assert_eq!(StoreError::Missing.kind().http_status(), 404);
```

Implementations provided:

| Type | Kind |
|---|---|
| `std::io::Error` | mapped from its `io::ErrorKind` (`NotFound` → `NotFound`, `ConnectionRefused` → `Unavailable`, `TimedOut` → `Timeout`, …). Call it as `Classify::kind(&err)` — the inherent `err.kind()` still returns the `io::ErrorKind`. |
| `ContextError` | the kind of the first `std::io::Error` in its `source()` chain, else `Unknown` |
| `MultiError` | the common kind of its members when it can be determined and they all agree, else `Unknown` |
| `ferroly::Error` | delegates to the wrapped module error — see below |
| `ConfigError`, `CliError` | `InvalidInput` (see [config](config.md), [cli](cli.md)) |

A boxed `dyn Error` cannot be asked for its kind, which is why wrappers answer
`Unknown` unless they find an error type they recognise.

## The root `ferroly::Error`

`ferroly::Error` (at the crate root, not in this module) has one `#[from]` variant
per enabled module error, so a function touching several modules can return
`Result<_, ferroly::Error>` and use `?` throughout:

| Variant | Wraps | Feature |
|---|---|---|
| `Codec` | `codec::CodecError` | `codec` |
| `Config` | `config::ConfigError` | `config` |
| `Fs` | `fsutils::FsError` | `fsutils` |
| `Lifecycle` | `lifecycle::LifecycleError` | `lifecycle` |
| `GenAi` | `genai::GenAiError` | `genai` |
| `Router` | `genai::RouterError` | `genai` |
| `Rest` | `rest::ClientError` | `rest` |
| `Ws` | `ws::WsError` | `ws` |
| `Http` | `http::HttpError` | `http` |
| `Breaker` | `clients::CircuitOpenError` | `clients` |
| `Auth` | `auth::JwtError` | `auth` |
| `Param` | `turbo::ParamError` | `turbo` |
| `Messaging` | `messaging::MessagingError` | `messaging` |
| `VectorStore` | `vectorstore::VectorStoreError` | `vectorstore` |
| `Vfs` | `vfs::VfsError` | `vfs` |
| `Cli` | `cli::CliError` | `cli` |
| `Join` | `rt::JoinError` | `rt` |
| `Multi` | `errutils::MultiError` | `errutils` |
| `Context` | `errutils::ContextError` | `errutils` |

With `errutils` enabled, `ferroly::Error` implements `Classify`. It answers where
the category is unambiguous and `Unknown` otherwise:

- `Codec` → `InvalidInput` (`Unsupported` for an unknown content type); `Config`
  and `Cli` → their own classification; `Param` → `InvalidInput`.
- `Auth` → `Unauthenticated`; `Breaker` and `Ws` → `Unavailable`.
- `Http` → `InvalidInput` for a bad URL, `Timeout` for a timeout, the I/O kind for
  an I/O failure; `Rest` → `InvalidInput` for an invalid request, otherwise the
  kind of the HTTP error underneath.
- `GenAi` → by variant, and by HTTP status for provider responses (`401` →
  `Unauthenticated`, `429` → `RateLimited`, `5xx` → `Unavailable`, other `4xx` →
  `InvalidInput`); `Router` → `Timeout` for an exceeded deadline, `InvalidInput`
  for configuration problems, otherwise the kind of the provider error underneath.
- `Lifecycle` → `NotFound` / `Conflict` / `InvalidInput` / `Timeout` by variant;
  `Vfs` → `NotFound` / `PermissionDenied` / `InvalidInput` / `Unsupported` by
  variant, the I/O kind for an I/O failure; `Fs` → the I/O kind for an I/O failure.
- `Messaging` → `Unavailable` for a full buffer; `VectorStore` → `InvalidInput`
  for a dimension mismatch; `Join` → `Internal` for a panicked task.
- `Multi`, `Context` → as described above.

```rust
use ferroly::errutils::Classify;

fn handle(err: &ferroly::Error) {
    if err.is_retryable() {
        // back off and try again
    } else if err.is_user_error() {
        // report to the caller; status = err.kind().http_status()
    } else {
        // log and alert
    }
}
```

## Error handling

`push`, `push_msg`, `extend` and the context methods cannot fail. The only decision
point of `MultiError` is `into_result`, which reports failure exactly when at least
one error was collected.

## Limitations

- **Errors are erased to `BoxError`** — downcast via
  `std::error::Error::downcast_ref` on the elements of `errors()` / the links of
  `chain()` if you need the concrete type back.
- **No backtraces** — `ContextError` records messages, not a stack trace.
- **Classification is coarse and opt-in** — only the types listed above implement
  `Classify`; the individual module error enums do not yet, so classify through
  `ferroly::Error` or implement the trait for your own types.
- **No deduplication** — pushing the same error twice stores it twice.

## See also

- [codec](codec.md) — its `CodecError` is one of the concrete errors you might
  collect into a `MultiError`.
- [derive](derive.md) — the `FerrolyError` derive used by other modules' error
  enums, all of which satisfy `Into<BoxError>` and so can be `push`ed here.

---
**Related:** [codec](codec.md), [derive](derive.md).
