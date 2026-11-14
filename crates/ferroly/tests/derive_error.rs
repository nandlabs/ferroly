//! `#[derive(FerrolyError)]`: struct errors with `#[source]` / `#[from]` /
//! `#[error(transparent)]`, `#[from]` on named fields, generic error types, and
//! the `#[error("...")]` format-string handling.

use std::error::Error;
use std::fmt::Debug;
use std::io;
use std::num::ParseIntError;

use ferroly::FerrolyError;

fn io_error(msg: &str) -> io::Error {
    io::Error::other(msg.to_string())
}

fn parse_error() -> ParseIntError {
    "x".parse::<i32>().unwrap_err()
}

/// The message of an error's direct source, if it has one.
fn source_text(e: &dyn Error) -> Option<String> {
    e.source().map(ToString::to_string)
}

// ---- structs -------------------------------------------------------------

#[derive(Debug, FerrolyError)]
#[error("request {id} failed")]
struct RequestError {
    id: u64,
    #[source]
    cause: io::Error,
}

#[derive(Debug, FerrolyError)]
#[error("step {0} failed")]
struct StepError(u8, #[source] io::Error);

#[test]
fn struct_with_source() {
    let e = RequestError {
        id: 7,
        cause: io_error("disk"),
    };
    assert_eq!(e.to_string(), "request 7 failed");
    assert_eq!(source_text(&e).as_deref(), Some("disk"));

    let e = StepError(2, io_error("net"));
    assert_eq!(e.to_string(), "step 2 failed");
    assert_eq!(source_text(&e).as_deref(), Some("net"));
}

#[derive(Debug, FerrolyError)]
#[error("config unreadable")]
struct ConfigError(#[from] io::Error);

#[derive(Debug, FerrolyError)]
#[error("bad number")]
struct NumberError {
    #[from]
    cause: ParseIntError,
}

fn read_config() -> Result<(), ConfigError> {
    Err(io_error("missing"))?;
    Ok(())
}

fn read_number(text: &str) -> Result<i32, NumberError> {
    Ok(text.parse::<i32>()?)
}

#[test]
fn struct_with_from_converts_through_question_mark() {
    let e = read_config().unwrap_err();
    assert_eq!(e.to_string(), "config unreadable");
    assert_eq!(source_text(&e).as_deref(), Some("missing"));

    assert_eq!(read_number("12").unwrap(), 12);
    let e = read_number("twelve").unwrap_err();
    assert_eq!(e.to_string(), "bad number");
    assert_eq!(e.cause, parse_error());
    assert!(e.source().is_some());
}

#[derive(Debug, FerrolyError)]
#[error(transparent)]
struct Opaque(#[from] io::Error);

#[derive(Debug, FerrolyError)]
#[error(transparent)]
struct OpaqueNamed {
    inner: io::Error,
}

#[derive(Debug, FerrolyError)]
#[error(transparent)]
struct AnyError(Box<dyn Error + Send + Sync + 'static>);

#[test]
fn transparent_struct_forwards_display() {
    let e: Opaque = io_error("boom").into();
    assert_eq!(e.to_string(), "boom");
    assert!(e.source().is_some());

    let e = OpaqueNamed {
        inner: io_error("named"),
    };
    assert_eq!(e.to_string(), "named");
    assert!(e.source().is_some());

    let e = AnyError(Box::new(io_error("boxed")));
    assert_eq!(e.to_string(), "boxed");
    assert_eq!(source_text(&e).as_deref(), Some("boxed"));
}

#[derive(Debug, FerrolyError)]
#[error("nothing to report")]
struct Plain;

#[derive(Debug, FerrolyError)]
#[error("cleanup failed")]
struct Cleanup {
    #[source]
    cause: Option<io::Error>,
}

#[derive(Debug, FerrolyError)]
#[error("handler failed")]
struct Handler {
    #[source]
    cause: Box<dyn Error + Send + Sync>,
}

#[test]
fn unit_optional_and_boxed_sources() {
    assert_eq!(Plain.to_string(), "nothing to report");
    assert!(Plain.source().is_none());

    assert!(Cleanup { cause: None }.source().is_none());
    let e = Cleanup {
        cause: Some(io_error("late")),
    };
    assert_eq!(source_text(&e).as_deref(), Some("late"));

    let e = Handler {
        cause: Box::new(io_error("inner")),
    };
    assert_eq!(source_text(&e).as_deref(), Some("inner"));
}

// ---- enums ---------------------------------------------------------------

#[derive(Debug, FerrolyError)]
enum AppError {
    #[error("io: {cause}")]
    Io {
        #[from]
        cause: io::Error,
    },
    #[error(transparent)]
    Parse {
        #[from]
        inner: ParseIntError,
    },
    #[error("format failed")]
    Format(#[from] std::fmt::Error),
    #[error("lookup of {key:?} failed")]
    Lookup {
        key: String,
        #[source]
        cause: io::Error,
    },
    #[error("unit")]
    Unit,
}

fn parse(text: &str) -> Result<i32, AppError> {
    Ok(text.parse::<i32>()?)
}

#[test]
fn from_on_named_variant_fields() {
    let e: AppError = io_error("disk").into();
    assert!(matches!(e, AppError::Io { .. }));
    assert_eq!(e.to_string(), "io: disk");
    assert_eq!(source_text(&e).as_deref(), Some("disk"));

    let e = parse("nope").unwrap_err();
    assert!(matches!(e, AppError::Parse { .. }));
    assert_eq!(e.to_string(), parse_error().to_string());
    assert!(e.source().is_some());

    let e: AppError = std::fmt::Error.into();
    assert!(matches!(e, AppError::Format(_)));

    let e = AppError::Lookup {
        key: "k".into(),
        cause: io_error("gone"),
    };
    assert_eq!(e.to_string(), "lookup of \"k\" failed");
    assert_eq!(source_text(&e).as_deref(), Some("gone"));
    assert!(AppError::Unit.source().is_none());
}

#[derive(Debug, FerrolyError)]
#[error("operation failed")]
enum WithFallback {
    Generic,
    AlsoGeneric(u8),
    #[error("specific: {0}")]
    Specific(u8),
}

#[test]
fn enum_level_error_is_the_fallback_message() {
    assert_eq!(WithFallback::Generic.to_string(), "operation failed");
    assert_eq!(WithFallback::AlsoGeneric(1).to_string(), "operation failed");
    assert_eq!(WithFallback::Specific(2).to_string(), "specific: 2");
}

// ---- format strings ------------------------------------------------------

#[derive(Debug, FerrolyError)]
enum Formats {
    #[error("code {0}: {1}")]
    Positional(u32, String),
    #[error("{1} before {0}, then {0} again")]
    Reordered(u8, u8),
    #[error("debug {0:?} and pretty-ish {1:?}")]
    DebugSpec(String, Option<u8>),
    #[error("named {name} / {name:?} / {count:>4}|{count:<4}|{count:04}")]
    Named { name: String, count: u32 },
    #[error("literal {{0}} {{name}} and value {0}")]
    EscapedTuple(u8),
    #[error("{{{id}}}")]
    EscapedNamed { id: u8 },
    #[error("hex {0:#x} bin {0:b} exp {1:e}")]
    Radix(u8, f64),
    #[error("{value:>width$}|{value:.precision$}")]
    Dynamic {
        value: f64,
        width: usize,
        precision: usize,
    },
    #[error("{0:>1$}")]
    DynamicTuple(u8, usize),
    #[error("keyword field {type} in {fn:?}")]
    Raw { r#type: u8, r#fn: String },
    #[error("no placeholders, 100%")]
    Bare(u8),
}

#[test]
fn positional_and_named_placeholders() {
    assert_eq!(
        Formats::Positional(404, "missing".into()).to_string(),
        "code 404: missing"
    );
    assert_eq!(
        Formats::Reordered(1, 2).to_string(),
        "2 before 1, then 1 again"
    );
    assert_eq!(
        Formats::DebugSpec("s".into(), Some(1)).to_string(),
        "debug \"s\" and pretty-ish Some(1)"
    );
    assert_eq!(
        Formats::Named {
            name: "n".into(),
            count: 7
        }
        .to_string(),
        "named n / \"n\" /    7|7   |0007"
    );
}

#[test]
fn escaped_braces_stay_literal() {
    assert_eq!(
        Formats::EscapedTuple(5).to_string(),
        "literal {0} {name} and value 5"
    );
    assert_eq!(Formats::EscapedNamed { id: 3 }.to_string(), "{3}");
}

#[test]
fn format_specs_and_dynamic_width() {
    assert_eq!(
        Formats::Radix(255, 1500.0).to_string(),
        "hex 0xff bin 11111111 exp 1.5e3"
    );
    assert_eq!(
        Formats::Dynamic {
            value: 1.2345,
            width: 8,
            precision: 2
        }
        .to_string(),
        "  1.2345|1.23"
    );
    assert_eq!(Formats::DynamicTuple(7, 3).to_string(), "  7");
}

#[test]
fn raw_identifier_fields_and_plain_text() {
    assert_eq!(
        Formats::Raw {
            r#type: 1,
            r#fn: "main".into()
        }
        .to_string(),
        "keyword field 1 in \"main\""
    );
    assert_eq!(Formats::Bare(0).to_string(), "no placeholders, 100%");
}

/// Field names chosen to collide with the locals the generated code uses.
#[derive(Debug, FerrolyError)]
#[error("{f} {__f} {__0} {self_}")]
struct Collide {
    f: u8,
    __f: u8,
    __0: u8,
    self_: u8,
}

#[test]
fn generated_locals_do_not_collide_with_field_names() {
    let e = Collide {
        f: 1,
        __f: 2,
        __0: 3,
        self_: 4,
    };
    assert_eq!(e.to_string(), "1 2 3 4");
}

// ---- generics ------------------------------------------------------------

#[derive(Debug, FerrolyError)]
#[error("invalid {what}: {value:?}")]
struct Invalid<T> {
    what: &'static str,
    value: T,
}

#[derive(Debug, FerrolyError)]
#[error("{0} (limit {1})")]
struct OverLimit<T, L>(T, L)
where
    T: Copy;

#[test]
fn generic_struct_errors() {
    let e = Invalid {
        what: "port",
        value: 70000u32,
    };
    assert_eq!(e.to_string(), "invalid port: 70000");
    // `{value:?}` only needs `Debug`, so a type without `Display` is fine.
    let e = Invalid {
        what: "list",
        value: vec![1, 2],
    };
    assert_eq!(e.to_string(), "invalid list: [1, 2]");
    assert_eq!(OverLimit(9u8, "8").to_string(), "9 (limit 8)");

    let boxed: Box<dyn Error + Send + Sync> = Box::new(Invalid {
        what: "x",
        value: 1u8,
    });
    assert_eq!(boxed.to_string(), "invalid x: 1");
}

#[derive(Debug, FerrolyError)]
enum Layered<E> {
    #[error("inner layer failed")]
    Inner(#[from] E),
    #[error("gave up after {attempts} attempts")]
    Exhausted {
        attempts: u32,
        #[source]
        last: Option<E>,
    },
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, FerrolyError)]
#[error(transparent)]
struct Passthrough<E>(E);

fn layered() -> Result<(), Layered<io::Error>> {
    Err(io_error("deep"))?;
    Ok(())
}

#[test]
fn generic_enum_errors_with_from_and_source() {
    let e = layered().unwrap_err();
    assert_eq!(e.to_string(), "inner layer failed");
    assert_eq!(source_text(&e).as_deref(), Some("deep"));

    let e = Layered::Exhausted {
        attempts: 3,
        last: Some(io_error("timeout")),
    };
    assert_eq!(e.to_string(), "gave up after 3 attempts");
    assert_eq!(source_text(&e).as_deref(), Some("timeout"));
    assert!(Layered::<io::Error>::Cancelled.source().is_none());

    let e = Passthrough(io_error("through"));
    assert_eq!(e.to_string(), "through");
    assert!(e.source().is_some());
}

#[derive(Debug, FerrolyError)]
#[error("unexpected token {token:?} at {at}")]
struct TokenError<'a> {
    token: &'a str,
    at: usize,
}

#[derive(Debug, FerrolyError)]
enum Borrowed<'a, T: Debug> {
    #[error("bad key {0}")]
    Key(&'a str),
    #[error("bad value {0:?}")]
    Value(T),
}

#[test]
fn error_types_with_lifetimes() {
    let text = String::from("}");
    let e = TokenError {
        token: &text,
        at: 4,
    };
    assert_eq!(e.to_string(), "unexpected token \"}\" at 4");
    assert!(e.source().is_none());

    assert_eq!(Borrowed::<u8>::Key("k").to_string(), "bad key k");
    assert_eq!(Borrowed::Value(Some(1u8)).to_string(), "bad value Some(1)");
}

/// A generic error can sit behind `?` into `Box<dyn Error>` like any other.
fn boxed(fail: bool) -> Result<u8, Box<dyn Error>> {
    if fail {
        Err(Invalid {
            what: "flag",
            value: fail,
        })?;
    }
    Ok(1)
}

#[test]
fn generic_errors_compose_with_boxed_dyn_error() {
    assert_eq!(boxed(false).unwrap(), 1);
    assert_eq!(boxed(true).unwrap_err().to_string(), "invalid flag: true");
}
