#![cfg(feature = "errutils")]
//! Behaviour tests for `errutils`: `MultiError` collection traits, context
//! wrapping and error classification.

use std::error::Error;
use std::io;

use ferroly::errutils::{chain, BoxError, Classify, Context, ContextError, ErrorKind, MultiError};

fn io_err(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg.to_string())
}

// ---- MultiError ------------------------------------------------------------

#[test]
fn multi_error_collects_from_iterators_and_vecs() {
    // FromIterator over concrete errors…
    let errs: MultiError = (1..=3)
        .map(|i| io_err(io::ErrorKind::Other, &format!("e{i}")))
        .collect();
    assert_eq!(errs.len(), 3);

    // …and over already boxed ones.
    let boxed: Vec<BoxError> = vec!["a".into(), "b".into()];
    let from_boxed: MultiError = boxed.into_iter().collect();
    assert_eq!(from_boxed.len(), 2);

    // From<Vec<_>> for both shapes.
    let from_vec = MultiError::from(vec![
        io_err(io::ErrorKind::Other, "x"),
        io_err(io::ErrorKind::Other, "y"),
    ]);
    assert_eq!(from_vec.len(), 2);
    let boxed: Vec<BoxError> = vec!["only".into()];
    assert_eq!(
        MultiError::from(boxed).to_string(),
        "1 error(s) occurred:\n  [1] only"
    );

    // The idiomatic "collect the failures of a batch".
    let failures: MultiError = ["1", "x", "3", "y"]
        .iter()
        .filter_map(|s| s.parse::<u8>().err())
        .collect();
    assert_eq!(failures.len(), 2);
    assert!(failures.into_result().is_err());
}

#[test]
fn multi_error_extends_and_iterates() {
    let mut errs = MultiError::new();
    errs.push_msg("first");
    // Extend with concrete errors (no manual boxing)…
    errs.extend(vec![io_err(io::ErrorKind::Other, "second")]);
    // …and with boxed errors, as before.
    let more: Vec<BoxError> = vec!["third".into()];
    errs.extend(more);
    assert_eq!(errs.len(), 3);

    let by_iter: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
    assert_eq!(by_iter, ["first", "second", "third"]);
    let by_ref: Vec<String> = (&errs).into_iter().map(|e| e.to_string()).collect();
    assert_eq!(by_ref, by_iter);
    let mut n = 0;
    for e in &errs {
        assert!(!e.to_string().is_empty());
        n += 1;
    }
    assert_eq!(n, 3);

    assert_eq!(errs.first().unwrap().to_string(), "first");
    // By value: the errors come back out, still downcastable.
    let owned: Vec<BoxError> = errs.into_iter().collect();
    assert!(owned[1].downcast_ref::<io::Error>().is_some());

    let again = MultiError::from(owned);
    assert_eq!(again.into_errors().len(), 3);
}

#[test]
fn multi_error_source_is_the_first_error() {
    let mut errs = MultiError::new();
    assert!(errs.source().is_none());
    assert!(errs.first().is_none());

    errs.push(io_err(io::ErrorKind::NotFound, "first cause"));
    errs.push_msg("second cause");
    let source = errs.source().expect("first error is the source");
    assert_eq!(source.to_string(), "first cause");
    assert!(source.downcast_ref::<io::Error>().is_some());

    // A generic chain walker therefore reaches a real cause.
    let messages: Vec<String> = chain(&errs).map(|e| e.to_string()).collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1], "first cause");
}

// ---- context ---------------------------------------------------------------

#[test]
fn context_wraps_and_preserves_the_source() {
    let r: Result<(), io::Error> = Err(io_err(io::ErrorKind::NotFound, "no such file"));
    let err = r.context("loading config").unwrap_err();

    assert_eq!(err.to_string(), "loading config");
    assert_eq!(err.context(), "loading config");
    let source = err.source().expect("source preserved");
    assert_eq!(source.to_string(), "no such file");
    assert_eq!(
        source.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(format!("{err:#}"), "loading config: no such file");

    // The Ok path is untouched.
    let ok: Result<u8, io::Error> = Ok(7);
    assert_eq!(ok.context("unused").unwrap(), 7);
}

#[test]
fn with_context_is_lazy_and_layers_stack() {
    let mut calls = 0;
    let ok: Result<u8, io::Error> = Ok(1);
    let v = ok
        .with_context(|| {
            calls += 1;
            "never built"
        })
        .unwrap();
    assert_eq!((v, calls), (1, 0), "closure must not run on Ok");

    let r: Result<u16, _> = "eighty".parse::<u16>();
    let err = r
        .with_context(|| format!("parsing {:?}", "eighty"))
        .context("reading the port")
        .context("starting the server")
        .unwrap_err();

    let messages: Vec<String> = chain(&err).map(|e| e.to_string()).collect();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        &messages[..3],
        [
            "starting the server",
            "reading the port",
            "parsing \"eighty\""
        ]
    );
    assert_eq!(err.root_cause().to_string(), messages[3]);
    assert!(err
        .root_cause()
        .downcast_ref::<std::num::ParseIntError>()
        .is_some());
    assert_eq!(
        format!("{err:#}"),
        format!(
            "starting the server: reading the port: parsing \"eighty\": {}",
            messages[3]
        )
    );

    // The wrapped error can be taken back out.
    let inner = err.into_source().unwrap();
    assert_eq!(inner.to_string(), "reading the port");
}

#[test]
fn context_on_option_and_boxed_errors() {
    let none: Option<u8> = None;
    let err = none.context("value was absent").unwrap_err();
    assert_eq!(err.to_string(), "value was absent");
    assert!(err.source().is_none());
    assert_eq!(err.root_cause().to_string(), "value was absent");
    assert_eq!(Some(3).with_context(|| "unused").unwrap(), 3);

    // Works on Box<dyn Error + Send + Sync> results and on string errors.
    let boxed: Result<(), BoxError> = Err("low level".into());
    assert_eq!(
        format!("{:#}", boxed.context("high level").unwrap_err()),
        "high level: low level"
    );
    let stringly: Result<(), String> = Err("bad".to_string());
    assert_eq!(
        format!("{:#}", stringly.context("ctx").unwrap_err()),
        "ctx: bad"
    );

    // ContextError is Send + Sync + 'static, so it crosses threads and boxes.
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<ContextError>();
    assert_send_sync::<MultiError>();
    let as_box: BoxError = ContextError::msg("boxed").into();
    assert_eq!(as_box.to_string(), "boxed");
}

// ---- classification --------------------------------------------------------

#[test]
fn error_kind_answers_retry_blame_and_status() {
    use ErrorKind::*;
    let table = [
        (InvalidInput, false, true, 400),
        (Unauthenticated, false, true, 401),
        (PermissionDenied, false, true, 403),
        (NotFound, false, true, 404),
        (Conflict, false, true, 409),
        (Unsupported, false, true, 501),
        (RateLimited, true, false, 429),
        (Timeout, true, false, 504),
        (Unavailable, true, false, 503),
        (Internal, false, false, 500),
        (Unknown, false, false, 500),
    ];
    for (kind, retryable, user, status) in table {
        assert_eq!(kind.is_retryable(), retryable, "{kind}");
        assert_eq!(kind.is_user_error(), user, "{kind}");
        assert_eq!(kind.http_status(), status, "{kind}");
        assert_eq!(kind.to_string(), kind.as_str());
    }
    assert_eq!(Unavailable.as_str(), "unavailable");
}

#[test]
fn io_and_wrapper_errors_classify() {
    let refused = io_err(io::ErrorKind::ConnectionRefused, "refused");
    assert_eq!(Classify::kind(&refused), ErrorKind::Unavailable);
    assert!(refused.is_retryable());
    assert!(!refused.is_user_error());

    let missing = io_err(io::ErrorKind::NotFound, "missing");
    assert_eq!(Classify::kind(&missing), ErrorKind::NotFound);
    assert!(missing.is_user_error());
    assert_eq!(
        Classify::kind(&io_err(io::ErrorKind::TimedOut, "t")),
        ErrorKind::Timeout
    );

    // Context layers do not hide the classification of an io error beneath.
    let r: Result<(), io::Error> = Err(io_err(io::ErrorKind::PermissionDenied, "denied"));
    let wrapped = r.context("opening").context("starting").unwrap_err();
    assert_eq!(wrapped.kind(), ErrorKind::PermissionDenied);
    assert_eq!(ContextError::msg("opaque").kind(), ErrorKind::Unknown);

    // A MultiError has a kind only when its members agree.
    let same: MultiError = vec![
        io_err(io::ErrorKind::TimedOut, "a"),
        io_err(io::ErrorKind::TimedOut, "b"),
    ]
    .into();
    assert_eq!(same.kind(), ErrorKind::Timeout);
    assert!(same.is_retryable());
    let mixed: MultiError = vec![
        io_err(io::ErrorKind::TimedOut, "a"),
        io_err(io::ErrorKind::NotFound, "b"),
    ]
    .into();
    assert_eq!(mixed.kind(), ErrorKind::Unknown);
    assert_eq!(MultiError::new().kind(), ErrorKind::Unknown);
}
