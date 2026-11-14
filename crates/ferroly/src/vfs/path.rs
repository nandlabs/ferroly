//! URL-scheme handling shared by the backends and the registry.

use super::VfsError;

/// Splits a `scheme://rest` address into its scheme and the remainder.
///
/// Returns `(None, addr)` when `addr` has no scheme prefix. A scheme is two or
/// more characters, starts with an ASCII letter, and continues with letters,
/// digits, `+`, `-` or `.` (RFC 3986); anything else before `://` — a path
/// that merely contains `://`, or a single drive letter — is not a scheme, so
/// such an address is treated as a plain path.
pub(crate) fn split_scheme(addr: &str) -> (Option<&str>, &str) {
    if let Some((scheme, rest)) = addr.split_once("://") {
        let mut chars = scheme.chars();
        let valid = scheme.len() >= 2
            && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if valid {
            return (Some(scheme), rest);
        }
    }
    (None, addr)
}

/// The URL scheme of `addr` (`"s3"` for `s3://bucket/key`), or `None` for a
/// plain path. The scheme is returned as written; compare it ignoring ASCII
/// case.
///
/// ```
/// use ferroly::vfs::scheme_of;
///
/// assert_eq!(scheme_of("s3://bucket/key"), Some("s3"));
/// assert_eq!(scheme_of("file:///tmp/x"), Some("file"));
/// assert_eq!(scheme_of("/tmp/x"), None);
/// assert_eq!(scheme_of("C:\\data\\x"), None);
/// ```
pub fn scheme_of(addr: &str) -> Option<&str> {
    split_scheme(addr).0
}

/// Strips an optional scheme that must be one of `accepted` (compared ignoring
/// ASCII case), returning the rest of the address. Any other scheme is an
/// [`VfsError::InvalidPath`] — a backend must never quietly treat
/// `s3://bucket/key` as one of its own paths.
pub(crate) fn strip_scheme<'a>(
    addr: &'a str,
    accepted: &[&str],
    backend: &str,
) -> Result<&'a str, VfsError> {
    match split_scheme(addr) {
        (None, rest) => Ok(rest),
        (Some(scheme), rest) if accepted.iter().any(|a| a.eq_ignore_ascii_case(scheme)) => Ok(rest),
        (Some(scheme), _) => Err(VfsError::InvalidPath {
            path: addr.to_string(),
            reason: format!("scheme `{scheme}` is not handled by {backend}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_only_real_schemes() {
        assert_eq!(split_scheme("file:///tmp/x"), (Some("file"), "/tmp/x"));
        assert_eq!(split_scheme("s3://bucket/key"), (Some("s3"), "bucket/key"));
        assert_eq!(
            split_scheme("git+ssh://host/repo"),
            (Some("git+ssh"), "host/repo")
        );
        assert_eq!(split_scheme("/tmp/x"), (None, "/tmp/x"));
        assert_eq!(split_scheme("relative/x"), (None, "relative/x"));
        // `://` later in a path is not a scheme separator.
        assert_eq!(split_scheme("/tmp/a://b"), (None, "/tmp/a://b"));
        assert_eq!(split_scheme("dir/a://b"), (None, "dir/a://b"));
        // Not a scheme: empty, starts with a digit, a single (drive) letter.
        assert_eq!(split_scheme("://x"), (None, "://x"));
        assert_eq!(split_scheme("1a://x"), (None, "1a://x"));
        assert_eq!(split_scheme("C://x"), (None, "C://x"));
        assert_eq!(split_scheme("C:\\x"), (None, "C:\\x"));
    }

    #[test]
    fn strip_scheme_rejects_foreign_schemes() {
        assert_eq!(strip_scheme("/a", &["file"], "LocalFs").unwrap(), "/a");
        assert_eq!(
            strip_scheme("FILE:///a", &["file"], "LocalFs").unwrap(),
            "/a"
        );
        let err = strip_scheme("s3://bucket/key", &["file"], "LocalFs").unwrap_err();
        match err {
            VfsError::InvalidPath { path, reason } => {
                assert_eq!(path, "s3://bucket/key");
                assert!(reason.contains("s3"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
