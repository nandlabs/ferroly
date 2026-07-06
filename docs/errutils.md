# ferroly::errutils

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `errutils` (enabled by default) — module `ferroly::errutils`. No dependencies.

## Overview

`errutils` is the smallest module in Ferroly. Its centerpiece is `MultiError`:
an aggregate that collects several errors
into a single value which itself implements `std::error::Error`. It exists for the
common "run a batch, report *all* the failures at once" pattern — validating every
field of a form, fanning out to many endpoints, or shutting down a set of resources —
instead of surfacing only the first error and hiding the rest.

## Enabling

`errutils` is on by default:

```toml
[dependencies]
ferroly = "0.1"                       # errutils + codec are the defaults
# or, explicitly / minimally:
ferroly = { version = "0.1", default-features = false, features = ["errutils"] }
```

## Quick start

```rust
use ferroly::errutils::MultiError;

let mut errs = MultiError::new();
errs.push(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
errs.push_msg("validation failed");

assert!(!errs.is_empty());
assert_eq!(errs.len(), 2);

// Turn the aggregate into a Result at the end of the batch.
errs.into_result().unwrap_err();
```

## API reference

### `MultiError`

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
| `into_result(self) -> Result<(), MultiError>` | `Ok(())` if empty, else `Err(self)`. |

### `BoxError`

```rust
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
```

The element type held by `MultiError`. Because it is `Send + Sync + 'static`, a
`MultiError` can cross thread and async-task boundaries.

### Trait implementations

- **`std::error::Error`** — a `MultiError` is a first-class error; return it from
  functions, box it, or wrap it in another error enum.
- **`Display`** — renders a header plus one indented line per contained error, e.g.:

  ```text
  2 error(s) occurred:
    [1] disk full
    [2] validation failed
  ```

- **`Extend<BoxError>`** — merge an iterator of boxed errors in one call
  (`errs.extend(other_boxed_errors)`), handy for combining sub-batches.

## In depth

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

### Combining aggregates with `Extend`

Because `MultiError: Extend<BoxError>`, you can fold any iterator of boxed errors
into an aggregate in one call:

```rust
use ferroly::errutils::{MultiError, BoxError};

let mut all = MultiError::new();
let more: Vec<BoxError> = vec![
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, "shard 1 failed")),
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, "shard 2 failed")),
];
all.extend(more);
assert_eq!(all.len(), 2);
```

## Error handling

`MultiError` *is* the error type here, so there is no fallible operation to guard —
`push`, `push_msg`, and `extend` cannot fail. The only decision point is
`into_result`, which reports failure exactly when at least one error was collected.

## Limitations

- **Intentionally minimal and dependency-free** — no error codes, severities, or
  categorization; it is a flat, ordered list of errors.
- **Errors are erased to `BoxError`** — downcast via
  `std::error::Error::downcast_ref` on the elements of `errors()` if you need the
  concrete type back.
- **No deduplication** — pushing the same error twice stores it twice.

## See also

- [codec](codec.md) — its `CodecError` is one of the concrete errors you might
  collect into a `MultiError`.
- [derive](derive.md) — the `FerrolyError` derive used by other modules' error
  enums, all of which satisfy `Into<BoxError>` and so can be `push`ed here.

---
**Related:** [codec](codec.md), [derive](derive.md).
