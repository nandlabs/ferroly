#![cfg(feature = "errutils")]
//! The crate-root `ferroly::Error`: every module error converts into it with
//! `?`, and it classifies what it wraps.

use std::io;

use ferroly::errutils::{chain, Classify, Context, ErrorKind, MultiError};

fn io_err(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg.to_string())
}

#[test]
fn root_error_wraps_multi_and_context_errors() {
    fn batch() -> Result<(), ferroly::Error> {
        let mut errs = MultiError::new();
        errs.push_msg("one");
        errs.into_result()?;
        Ok(())
    }
    let err = batch().unwrap_err();
    assert!(matches!(err, ferroly::Error::Multi(_)));
    assert!(err.to_string().contains("[1] one"));

    fn step() -> Result<(), ferroly::Error> {
        let r: Result<(), io::Error> = Err(io_err(io::ErrorKind::TimedOut, "slow disk"));
        r.context("flushing")?;
        Ok(())
    }
    let err = step().unwrap_err();
    assert!(matches!(err, ferroly::Error::Context(_)));
    assert_eq!(err.to_string(), "flushing");
    // The chain is intact through the root error…
    let messages: Vec<String> = chain(&err).map(|e| e.to_string()).collect();
    assert!(messages.contains(&"slow disk".to_string()), "{messages:?}");
    // …and so is the classification.
    assert_eq!(err.kind(), ErrorKind::Timeout);
    assert!(err.is_retryable());
}

#[cfg(feature = "codec")]
#[test]
fn root_error_classifies_codec_errors_as_invalid_input() {
    use ferroly::codec::json;

    let err: ferroly::Error = json::from_str("{ nope").unwrap_err().into();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert!(err.is_user_error());
    assert_eq!(err.kind().http_status(), 400);
}

#[cfg(feature = "vfs")]
#[test]
fn root_error_wraps_and_classifies_vfs_errors() {
    use ferroly::vfs::VfsError;

    fn open() -> Result<(), ferroly::Error> {
        Err(VfsError::NotFound("a.txt".into()))?;
        Ok(())
    }
    let err = open().unwrap_err();
    assert!(matches!(err, ferroly::Error::Vfs(_)));
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert!(err.to_string().contains("a.txt"));
}

#[cfg(feature = "genai")]
#[test]
fn root_error_wraps_and_classifies_router_errors() {
    use ferroly::genai::{GenAiError, RouterError};

    fn route() -> Result<(), ferroly::Error> {
        Err(RouterError::DeadlineExceeded)?;
        Ok(())
    }
    let err = route().unwrap_err();
    assert!(matches!(err, ferroly::Error::Router(_)));
    assert_eq!(err.kind(), ErrorKind::Timeout);

    let throttled: ferroly::Error = RouterError::GenAi(GenAiError::Api {
        status: 429,
        message: "slow down".into(),
    })
    .into();
    assert_eq!(throttled.kind(), ErrorKind::RateLimited);
    assert!(throttled.is_retryable());

    let direct: ferroly::Error = GenAiError::Api {
        status: 401,
        message: "no key".into(),
    }
    .into();
    assert_eq!(direct.kind(), ErrorKind::Unauthenticated);
}

#[cfg(feature = "http")]
#[test]
fn root_error_classifies_http_errors() {
    use ferroly::http::HttpError;

    let timeout: ferroly::Error = HttpError::Timeout.into();
    assert_eq!(timeout.kind(), ErrorKind::Timeout);
    let refused: ferroly::Error =
        HttpError::Io(io_err(io::ErrorKind::ConnectionRefused, "refused")).into();
    assert_eq!(refused.kind(), ErrorKind::Unavailable);
    let url: ferroly::Error = HttpError::InvalidUrl("x".into()).into();
    assert!(url.is_user_error());
}

#[cfg(feature = "rt")]
#[test]
fn root_error_wraps_join_errors() {
    let rt = ferroly::rt::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let err: ferroly::Error = rt.block_on(async {
        let handle: ferroly::rt::JoinHandle<()> =
            ferroly::rt::spawn(async { panic!("task blew up") });
        handle.await.unwrap_err().into()
    });
    assert!(matches!(err, ferroly::Error::Join(_)));
    assert_eq!(err.kind(), ErrorKind::Internal);
}

#[cfg(feature = "cli")]
#[test]
fn root_error_wraps_and_classifies_cli_errors() {
    use ferroly::cli::{CliError, Command};

    fn parse(args: &[&str]) -> Result<(), ferroly::Error> {
        Command::new("t").parse(args.iter().map(|s| s.to_string()))?;
        Ok(())
    }
    let err = parse(&["--nope"]).unwrap_err();
    assert!(matches!(
        err,
        ferroly::Error::Cli(CliError::UnknownFlag { .. })
    ));
    assert_eq!(err.to_string(), "unknown flag: --nope");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);

    // A help request travels through the root error unchanged.
    let help = parse(&["--help"]).unwrap_err();
    assert!(matches!(
        help,
        ferroly::Error::Cli(CliError::HelpRequested(_))
    ));
    assert!(!help.is_user_error());
}

#[cfg(feature = "config")]
#[test]
fn root_error_wraps_and_classifies_config_errors() {
    use ferroly::config::{Config, ConfigError};

    fn load() -> Result<Config, ferroly::Error> {
        Ok(Config::from_file("nope.ini")?)
    }
    let err = load().unwrap_err();
    assert!(matches!(
        err,
        ferroly::Error::Config(ConfigError::UnsupportedFormat(_))
    ));
    assert_eq!(err.kind(), ErrorKind::Unsupported);

    let missing: ferroly::Error = ConfigError::MissingKeys(vec!["a".into()]).into();
    assert_eq!(missing.kind(), ErrorKind::InvalidInput);
}
