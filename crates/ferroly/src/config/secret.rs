//! Secret references (`env:NAME` / `file:/path`) as configuration values.

use std::fmt;
use std::io::Read;
use std::path::Path;
#[cfg(feature = "hash")]
use std::path::PathBuf;

#[cfg(feature = "hash")]
use ferroly::codec::{CodecError, Decode, Encode, Value};

use super::Config;
#[cfg(feature = "hash")]
use super::ConfigError;

/// Largest secret or referenced file configuration reads: `file:` references,
/// `${file:…}` placeholders, `*_FILE` variables and secret directories.
const MAX_SECRET_BYTES: u64 = 1024 * 1024;

/// Whether the redacted dump may show `text` as a secret reference under a
/// sensitive key: `env:NAME` with `NAME` an identifier (ASCII letters,
/// digits and `_`, not starting with a digit), or `file:PATH` with a
/// non-empty `PATH` free of whitespace and control characters (both
/// prefixes case-sensitive).
///
/// Stricter than what [`SecretRef`] accepts, on purpose: a secret someone
/// wrote as `env:hunter2 and more` must not be shown just because of its
/// first four characters.
pub(super) fn is_reference(text: &str) -> bool {
    if let Some(name) = text.strip_prefix("env:") {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else if let Some(path) = text.strip_prefix("file:") {
        !path.is_empty() && !path.chars().any(|c| c.is_whitespace() || c.is_control())
    } else {
        false
    }
}

/// An environment variable name `SecretRef` accepts: non-empty, without `=`
/// or NUL.
#[cfg(feature = "hash")]
fn valid_var(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0'])
}

/// A file path `SecretRef` accepts: non-empty, without NUL.
#[cfg(feature = "hash")]
fn valid_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0')
}

/// Why a secret file could not be read. The message never contains any of
/// the file's content.
#[derive(Debug)]
pub(super) struct ReadError {
    kind: Option<std::io::ErrorKind>,
    reason: String,
}

impl ReadError {
    /// The I/O error kind, when the failure was an I/O error.
    pub(super) fn kind(&self) -> Option<std::io::ErrorKind> {
        self.kind
    }

    fn io(e: &std::io::Error) -> Self {
        let reason = match e.kind() {
            std::io::ErrorKind::NotFound => "file not found".to_string(),
            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
            _ => e.to_string(),
        };
        ReadError {
            kind: Some(e.kind()),
            reason,
        }
    }

    fn other(reason: &str) -> Self {
        ReadError {
            kind: None,
            reason: reason.to_string(),
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Reads a whole secret file: a regular file of at most 1 MiB of UTF-8, with
/// **one** trailing `\n` or `\r\n` removed (so a deliberate trailing blank
/// line survives; `*_FILE` variables, older than this, strip every trailing
/// `\n` and `\r`).
///
/// Used by `file:` references, `${file:…}` placeholders and mounted secret
/// directories; see [`read_bounded`] for the size cap.
///
/// The path is checked before it is opened, so a FIFO or device that is
/// already there is refused without blocking on `open`; the opened handle is
/// checked again (and on Unix must be the same file), so a path swapped for
/// something else between the two is refused rather than read. The handle
/// is what is read. (Without platform `open` flags, an `open` that lands on
/// a FIFO swapped in during that window can still wait for a writer; only
/// someone who can already replace the secret can arrange that.)
pub(super) fn read_secret_file(path: &Path) -> Result<String, ReadError> {
    let meta = std::fs::metadata(path).map_err(|e| ReadError::io(&e))?;
    if !meta.is_file() {
        return Err(ReadError::other("not a regular file"));
    }
    let file = std::fs::File::open(path).map_err(|e| ReadError::io(&e))?;
    let opened = file.metadata().map_err(|e| ReadError::io(&e))?;
    if !opened.is_file() || !same_file(&meta, &opened) {
        return Err(ReadError::other("file changed while it was opened"));
    }
    let mut text = read_bounded_from(file)?;
    if text.ends_with("\r\n") {
        text.truncate(text.len() - 2);
    } else if text.ends_with('\n') {
        text.truncate(text.len() - 1);
    }
    Ok(text)
}

/// Reads at most 1 MiB of UTF-8 from `path`, the one way configuration reads
/// a secret or referenced file (`file:` references, `${file:…}`
/// placeholders, `*_FILE` variables and secret directories).
///
/// The cap is enforced on the bytes actually read, not on the length the
/// file reports: a file that grows after it was checked, or one that reports
/// a length of 0 (`/proc`-style), is still cut off at the cap and refused.
/// A refused buffer is wiped before it is dropped.
pub(super) fn read_bounded(path: &Path) -> Result<String, ReadError> {
    let file = std::fs::File::open(path).map_err(|e| ReadError::io(&e))?;
    read_bounded_from(file)
}

/// Whether two metadata records describe the same file (always true where
/// the platform gives no identity to compare).
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
}

/// [`read_bounded`] for a file that is already open.
fn read_bounded_from(file: std::fs::File) -> Result<String, ReadError> {
    let mut bytes = Vec::new();
    if let Err(e) = file.take(MAX_SECRET_BYTES + 1).read_to_end(&mut bytes) {
        wipe(&mut bytes);
        return Err(ReadError::io(&e));
    }
    if bytes.len() as u64 > MAX_SECRET_BYTES {
        wipe(&mut bytes);
        return Err(ReadError::other("file is larger than 1 MiB"));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(e) => {
            wipe(&mut e.into_bytes());
            Err(ReadError::other("content is not valid UTF-8"))
        }
    }
}

/// Overwrites a buffer that held secret bytes before it is dropped.
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        *b = 0;
    }
    std::hint::black_box(bytes);
}

/// A reference to a secret kept outside the configuration: `env:NAME` (an
/// environment variable) or `file:/path` (a whole file, absolute or relative
/// to the process's current directory).
///
/// Use it as the type of a config field that must name a secret rather than
/// hold one. Decoding accepts only the two reference forms (prefixes are
/// case-sensitive) and refuses a literal with an error that shows at most its
/// first two characters and its length. [`resolve`](SecretRef::resolve) reads
/// the secret into a redacting [`Secret`](ferroly::hash::Secret).
///
/// A reference is not itself secret: `Debug`, `Display` and
/// [`Config::to_redacted_string`] show it verbatim.
///
/// ```
/// use ferroly::codec::{Decode, Value};
/// use ferroly::config::SecretRef;
///
/// let keys = Vec::<SecretRef>::decode(&Value::Array(vec![
///     Value::Str("env:APP_KEY".into()),
///     Value::Str("file:/run/secrets/key2".into()),
/// ]))?;
/// assert_eq!(keys[0], SecretRef::env("APP_KEY"));
/// assert_eq!(keys[1].to_string(), "file:/run/secrets/key2");
///
/// let err = SecretRef::decode(&Value::Str("sk-live-abc123".into())).unwrap_err();
/// assert!(err.to_string().contains("(got \"sk…\", 14 chars)"));
/// assert!(!err.to_string().contains("abc123"));
/// # Ok::<(), ferroly::codec::CodecError>(())
/// ```
#[cfg(feature = "hash")]
#[derive(Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecretRef {
    /// `env:NAME` — the value of the environment variable `NAME`.
    Env(String),
    /// `file:/path` — the content of the file.
    File(PathBuf),
}

#[cfg(feature = "hash")]
impl SecretRef {
    /// A reference to the environment variable `name`.
    pub fn env(name: impl Into<String>) -> Self {
        SecretRef::Env(name.into())
    }

    /// A reference to the file at `path`.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        SecretRef::File(path.into())
    }

    /// Parses `env:NAME` or `file:/path`. Anything else is an error whose
    /// message never contains more than the first two characters of `text`
    /// (none for a value shorter than eight characters), plus its length.
    pub fn parse(text: &str) -> Result<Self, CodecError> {
        if let Some(name) = text.strip_prefix("env:") {
            if valid_var(name) {
                return Ok(SecretRef::Env(name.to_string()));
            }
            return Err(CodecError::Message(
                "secret reference `env:` must name a variable (non-empty, without `=`)".into(),
            ));
        }
        if let Some(path) = text.strip_prefix("file:") {
            if valid_path(path) {
                return Ok(SecretRef::File(PathBuf::from(path)));
            }
            return Err(CodecError::Message(
                "secret reference `file:` must name a file".into(),
            ));
        }
        let count = text.chars().count();
        let shown: String = if count >= 8 {
            text.chars().take(2).collect()
        } else {
            String::new()
        };
        Err(CodecError::Message(format!(
            "secret must be an env:NAME or file:/path reference, not a literal \
             (got \"{shown}…\", {count} chars)"
        )))
    }

    /// Reads the secret: the environment variable's value, or the whole file
    /// (UTF-8, at most 1 MiB, one trailing `\n` or `\r\n` removed).
    ///
    /// An empty secret is not an error; check
    /// [`Secret::is_empty`](ferroly::hash::Secret::is_empty) if it should be.
    /// Errors are [`ConfigError::Secret`] and name the reference only:
    /// `secret env:APP_KEY: variable is not set`,
    /// `secret file:/run/secrets/k: permission denied`.
    pub fn resolve(&self) -> Result<ferroly::hash::Secret, ConfigError> {
        let fail = |reason: String| ConfigError::Secret {
            reference: self.to_string(),
            reason,
        };
        match self {
            SecretRef::Env(name) => match std::env::var_os(name) {
                None => Err(fail("variable is not set".into())),
                Some(raw) => match raw.into_string() {
                    Ok(text) => Ok(ferroly::hash::Secret::from(text)),
                    Err(raw) => {
                        let mut raw = raw.into_encoded_bytes();
                        wipe(&mut raw);
                        Err(fail("value is not valid UTF-8".into()))
                    }
                },
            },
            SecretRef::File(path) => read_secret_file(path)
                .map(ferroly::hash::Secret::from)
                .map_err(|e| fail(e.to_string())),
        }
    }
}

#[cfg(feature = "hash")]
impl fmt::Display for SecretRef {
    /// The reference as written: `env:NAME` or `file:/path`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Env(name) => write!(f, "env:{name}"),
            SecretRef::File(path) => write!(f, "file:{}", path.display()),
        }
    }
}

#[cfg(feature = "hash")]
impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretRef")
            .field(&format_args!("{self}"))
            .finish()
    }
}

#[cfg(feature = "hash")]
impl std::str::FromStr for SecretRef {
    type Err = CodecError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        SecretRef::parse(text)
    }
}

#[cfg(feature = "hash")]
impl Decode for SecretRef {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        match value {
            Value::Str(text) => SecretRef::parse(text),
            _ => Err(CodecError::Message(
                "secret must be an env:NAME or file:/path reference string".into(),
            )),
        }
    }
}

#[cfg(feature = "hash")]
impl Encode for SecretRef {
    fn encode(&self) -> Value {
        Value::Str(self.to_string())
    }
}

impl Config {
    /// Masks, in [`to_redacted_string`](Config::to_redacted_string), every
    /// value under the given (dotted) keys that is **not** a secret reference
    /// (`env:NAME` / `file:/path`), whatever the key is called.
    ///
    /// For keys that must hold references (`server.api_keys`) where decoding
    /// is lenient or the dump is printed before decoding: a literal secret
    /// someone wrote there is hidden, the references stay visible. The keys
    /// add to those of earlier calls.
    ///
    /// ```
    /// use ferroly::codec::Value;
    /// use ferroly::config::Config;
    ///
    /// let mut cfg = Config::builder()
    ///     .merge_value(Value::Object(vec![(
    ///         "upstreams".into(),
    ///         Value::Array(vec![
    ///             Value::Str("env:UPSTREAM_TOKEN".into()),
    ///             Value::Str("hunter2-literal".into()),
    ///         ]),
    ///     )]))
    ///     .build();
    /// cfg.redact_literals(["upstreams"]);
    /// let dump = cfg.to_redacted_string();
    /// assert!(dump.contains("upstreams[0] = \"env:UPSTREAM_TOKEN\""));
    /// assert!(dump.contains("upstreams[1] = <redacted>"));
    /// ```
    pub fn redact_literals<I, S>(&mut self, keys: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.literal_keys
            .extend(keys.into_iter().map(|k| k.as_ref().to_string()));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_recognised() {
        assert!(is_reference("env:A"));
        assert!(is_reference("file:/run/secrets/x"));
        assert!(is_reference("file:relative/x"));
        assert!(is_reference("env:_APP_KEY2"));
        for not in [
            "env:",
            "file:",
            "ENV:A",
            "env:A=B",
            "plain",
            " env:A",
            "",
            // Secrets that merely start with a prefix.
            "env:hunter2 and more",
            "env:x y z real secret",
            "env:2fa",
            "env:pa$$word",
            "file:/run/my secret",
            "file:/a\tb",
            "file:/a\u{7}",
        ] {
            assert!(!is_reference(not), "{not}");
        }
    }
}
