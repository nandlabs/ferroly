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

#[cfg(feature = "sig")]
#[test]
fn root_error_wraps_and_classifies_sig_errors() {
    use ferroly::sig::{Ed25519PublicKey, Ed25519Signature, SigError};

    // RFC 8032 section 7.1, TEST 1: a valid signature over the empty message,
    // checked here against a different message.
    fn check(message: &[u8]) -> Result<(), ferroly::Error> {
        let key = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        let sig = [
            0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e,
            0x82, 0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65,
            0x22, 0x49, 0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e,
            0x39, 0x70, 0x1c, 0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24,
            0x65, 0x51, 0x41, 0x43, 0x8e, 0x7a, 0x10, 0x0b,
        ];
        let key = Ed25519PublicKey::from_bytes(&key)?;
        key.verify(message, &Ed25519Signature::from_bytes(&sig))?;
        Ok(())
    }
    assert!(check(b"").is_ok());
    let err = check(b"tampered").unwrap_err();
    assert!(matches!(err, ferroly::Error::Sig(_)));
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);

    let malformed: ferroly::Error = SigError::MalformedKey.into();
    assert_eq!(malformed.kind(), ErrorKind::InvalidInput);
    let context: ferroly::Error = SigError::InvalidContext.into();
    assert_eq!(context.kind(), ErrorKind::InvalidInput);

    // A tampered signature is `MalformedSignature` or `VerificationFailed`
    // depending on which bit flipped; both must classify the same way.
    let mut flipped_into_malformed = 0;
    for bit in 0..512 {
        let mut sig = [0u8; 64];
        sig.copy_from_slice(
            &ferroly::hash::hex::decode(
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
                 5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            )
            .unwrap(),
        );
        sig[bit / 8] ^= 1 << (bit % 8);
        let key = Ed25519PublicKey::from_slice(
            &ferroly::hash::hex::decode(
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            )
            .unwrap(),
        )
        .unwrap();
        let err: ferroly::Error = key
            .verify(b"", &Ed25519Signature::from_bytes(&sig))
            .unwrap_err()
            .into();
        if matches!(err, ferroly::Error::Sig(SigError::MalformedSignature)) {
            flipped_into_malformed += 1;
        }
        assert_eq!(err.kind(), ErrorKind::Unauthenticated, "bit {bit}");
    }
    assert!(flipped_into_malformed > 0);
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

    let throttled: ferroly::Error = RouterError::GenAi(GenAiError::api(429, "slow down")).into();
    assert_eq!(throttled.kind(), ErrorKind::RateLimited);
    assert!(throttled.is_retryable());

    let direct: ferroly::Error = GenAiError::api(401, "no key").into();
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

#[cfg(feature = "http")]
#[test]
fn http2_failures_classify_by_their_cause() {
    use ferroly::http::{Http2Cause, Http2Error, Http2ErrorCode, HttpError};

    let h2 = |cause| HttpError::Http2(Http2Error::new(cause, "test"));
    let cases = [
        (h2(Http2Cause::Timeout), ErrorKind::Timeout),
        (h2(Http2Cause::Refused), ErrorKind::Unavailable),
        (h2(Http2Cause::ConnectionClosed), ErrorKind::Unavailable),
        (h2(Http2Cause::LocalLimit), ErrorKind::Unavailable),
        (
            h2(Http2Cause::Reset(Http2ErrorCode::CANCEL)),
            ErrorKind::Unavailable,
        ),
        (
            h2(Http2Cause::Protocol(Http2ErrorCode::PROTOCOL_ERROR)),
            ErrorKind::Internal,
        ),
        (
            h2(Http2Cause::Io(io::ErrorKind::PermissionDenied)),
            ErrorKind::PermissionDenied,
        ),
        (
            h2(Http2Cause::LocalReset(Http2ErrorCode::REFUSED_STREAM)),
            ErrorKind::Unavailable,
        ),
        (
            h2(Http2Cause::LocalReset(Http2ErrorCode::INTERNAL_ERROR)),
            ErrorKind::Internal,
        ),
        (
            h2(Http2Cause::LocalReset(Http2ErrorCode::CANCEL)),
            ErrorKind::Unknown,
        ),
    ];
    for (e, expected) in cases {
        assert_eq!(e.kind(), expected, "direct: {e:?}");
        let root: ferroly::Error = e.into();
        assert_eq!(root.kind(), expected, "root: {root:?}");
    }
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

/// Module error types classify themselves, and the root `Error` answers the
/// same by delegating.
#[test]
fn module_errors_classify_directly_like_the_root_error() {
    #[allow(dead_code)]
    fn same<E>(e: E, expected: ErrorKind)
    where
        E: Classify + Into<ferroly::Error> + std::fmt::Debug,
    {
        assert_eq!(e.kind(), expected, "direct: {e:?}");
        let root: ferroly::Error = e.into();
        assert_eq!(root.kind(), expected, "root: {root:?}");
    }

    #[cfg(feature = "http")]
    {
        use ferroly::http::HttpError;
        same(HttpError::ConnectTimeout, ErrorKind::Timeout);
        same(HttpError::InvalidUrl("x".into()), ErrorKind::InvalidInput);
        same(
            HttpError::Io(io_err(io::ErrorKind::ConnectionReset, "reset")),
            ErrorKind::Unavailable,
        );
        same(HttpError::HandlerPanic("boom".into()), ErrorKind::Internal);
    }
    #[cfg(feature = "vfs")]
    {
        use ferroly::vfs::VfsError;
        same(VfsError::NotFound("/a".into()), ErrorKind::NotFound);
        same(VfsError::Unavailable("down".into()), ErrorKind::Unavailable);
    }
    #[cfg(feature = "genai")]
    {
        use ferroly::genai::GenAiError;
        same(GenAiError::api(503, "busy"), ErrorKind::Unavailable);
    }
    #[cfg(feature = "auth")]
    {
        use ferroly::auth::JwtError;
        same(JwtError::Expired, ErrorKind::Unauthenticated);
        same(JwtError::KeyFetch("down".into()), ErrorKind::Unavailable);
        same(JwtError::NoCryptoProvider, ErrorKind::Unsupported);
    }
    #[cfg(feature = "ws")]
    {
        use ferroly::ws::WsError;
        same(
            WsError::Rejected { status: 401 },
            ErrorKind::Unauthenticated,
        );
        same(WsError::Timeout("connect".into()), ErrorKind::Timeout);
        same(WsError::Closed, ErrorKind::Unavailable);
    }
    #[cfg(feature = "messaging")]
    {
        use ferroly::messaging::MessagingError;
        same(
            MessagingError::InvalidAddress("?".into()),
            ErrorKind::InvalidInput,
        );
    }
}
