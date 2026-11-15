//! The unified error type for encoding, decoding, and codec lookup.
//!
//! Hand-written (not via the derive) to keep the encoding core free of
//! bootstrapping concerns.

use std::fmt;

use super::Value;

/// Errors raised across parsing, value conversion, and content-type resolution.
///
/// # Field paths
///
/// A decode error raised inside a field, element or map entry is wrapped in
/// [`At`](CodecError::At), which records where it happened as a
/// [`path`](CodecError::path) of wire names and indexes. `Display` prefixes the
/// message with that path (`messages[1].role: expected string, found number`);
/// an error raised at the root has no path and prints exactly as before.
///
/// ```
/// use ferroly::codec::{json, CodecError, Decode};
///
/// #[derive(Decode, Debug)]
/// struct Message { role: String }
/// #[derive(Decode, Debug)]
/// struct Chat { messages: Vec<Message>, temperature: Option<f32> }
///
/// let err = json::decode::<Chat>(r#"{"messages":[{"role":"user"},{"role":7}]}"#)
///     .unwrap_err();
/// assert_eq!(err.to_string(), "messages[1].role: expected string, found number");
/// assert_eq!(err.path_string(), "messages[1].role");
/// assert_eq!(err.path()[0].to_string(), "messages");
///
/// let err = json::decode::<Chat>(r#"{"messages":[],"temperature":"0.5"}"#).unwrap_err();
/// assert_eq!(err.path_string(), "temperature");
/// assert!(matches!(
///     err.root_cause(),
///     CodecError::TypeMismatch { expected: "number", found: "string" }
/// ));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    /// No codec is registered for the requested content type.
    UnsupportedContentType(String),
    /// A syntax error while parsing an input format.
    Parse(String),
    /// A value of the wrong shape was encountered (e.g. expected an object).
    Expected(&'static str),
    /// A required struct field was absent.
    MissingField(String),
    /// An enum string did not match any known variant.
    UnknownVariant(String),
    /// A numeric value did not fit the target type.
    OutOfRange(&'static str),
    /// A general message.
    Message(String),
    /// The input exceeded a configured [`Limits`](super::Limits) budget
    /// (nesting depth, input size, or entry count).
    LimitExceeded(String),
    /// A value cannot be represented in the target format (raised by the
    /// fallible `try_encode` entry points).
    Unrepresentable(String),
    /// A value of the wrong type, where the type actually found is known
    /// (built by [`CodecError::type_mismatch`]).
    TypeMismatch {
        /// What the target type needed (`"string"`).
        expected: &'static str,
        /// The kind of value the input held (`"number"`); see
        /// [`CodecError::type_mismatch`] for the names.
        found: &'static str,
    },
    /// `source` was raised at `path` inside the value being decoded. Built by
    /// [`in_field`](CodecError::in_field) and
    /// [`in_index`](CodecError::in_index), which keep a single `At` layer
    /// with a non-empty path.
    ///
    /// The fields are public, so a value can also be built by hand; it is
    /// still handled sensibly: an empty path prints no `": "` prefix, and
    /// nested `At` layers render as one joined path and are merged by the
    /// next `in_field`/`in_index`.
    At {
        /// Where the error happened, outermost segment first (never empty
        /// when built by the helpers).
        path: Vec<PathSegment>,
        /// The error itself (never another `At` when built by the helpers).
        source: Box<CodecError>,
    },
}

/// One step of a [`CodecError`] path: a map key or struct field (by its wire
/// name), or a sequence index.
///
/// `Display` prints the bare key or index (`messages`, `2`), which is what an
/// API reporting the offending parameter wants; [`CodecError::path_string`]
/// renders a whole path. A key is the input's own text, printed unescaped
/// and in full here, so escape it before logging it on its own; the
/// `CodecError` renderings already do.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSegment {
    /// A struct field or map key, as it is spelled on the wire.
    Key(String),
    /// A sequence or tuple element.
    Index(usize),
}

impl fmt::Display for PathSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathSegment::Key(k) => f.write_str(k),
            PathSegment::Index(i) => write!(f, "{i}"),
        }
    }
}

/// How many characters of a key a rendered path shows; a longer key is cut
/// there and ends in `…`. Keys come from the input, so without a cap a huge
/// key would make an equally huge error message.
const MAX_RENDERED_KEY_CHARS: usize = 64;

/// Whether `key` can be written bare in a rendered path (`a.b`); anything else
/// is quoted (`["my key"]`).
fn is_plain_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_RENDERED_KEY_CHARS
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Characters a rendered key shows as an escape: control characters (a raw
/// newline or `ESC` in a logged message could forge log lines or drive a
/// terminal) and the invisible Unicode line separators and bidirectional
/// controls, which can make a message read differently from what it holds.
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Writes a quoted key: `"` and `\` are backslash-escaped, newline, carriage
/// return and tab are `\n`, `\r`, `\t`, other [unsafe](is_unsafe_char)
/// characters are `\u{..}`, and the key is cut at
/// [`MAX_RENDERED_KEY_CHARS`].
fn write_quoted_key(f: &mut impl fmt::Write, key: &str) -> fmt::Result {
    f.write_str("[\"")?;
    for (n, c) in key.chars().enumerate() {
        if n == MAX_RENDERED_KEY_CHARS {
            f.write_char('…')?;
            break;
        }
        match c {
            '"' | '\\' => {
                f.write_char('\\')?;
                f.write_char(c)?;
            }
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if is_unsafe_char(c) => write!(f, "\\u{{{:x}}}", c as u32)?,
            c => f.write_char(c)?,
        }
    }
    f.write_str("\"]")
}

/// Renders `path` as `a.b[2].c`, quoting keys that are not plain
/// (`a["my key"]`). `started` says whether segments were already written
/// before this slice (so a leading plain key needs its `.`); returns whether
/// anything has been written now.
fn write_path(
    f: &mut impl fmt::Write,
    path: &[PathSegment],
    mut started: bool,
) -> Result<bool, fmt::Error> {
    for segment in path {
        match segment {
            PathSegment::Index(n) => write!(f, "[{n}]")?,
            PathSegment::Key(k) if is_plain_key(k) => {
                if started {
                    f.write_char('.')?;
                }
                f.write_str(k)?;
            }
            PathSegment::Key(k) => write_quoted_key(f, k)?,
        }
        started = true;
    }
    Ok(started)
}

/// Renders the whole path of `err`, through every [`At`](CodecError::At)
/// layer (the helpers keep one, but the variant can be built by hand), and
/// returns the error underneath with whether any segment was written.
fn write_full_path<'e>(
    f: &mut impl fmt::Write,
    mut err: &'e CodecError,
) -> Result<(&'e CodecError, bool), fmt::Error> {
    let mut started = false;
    while let CodecError::At { path, source } = err {
        started = write_path(f, path, started)?;
        err = source;
    }
    Ok((err, started))
}

impl CodecError {
    /// A syntax error in the input as a whole.
    ///
    /// The parsers raise it with the line and column, and it is never given
    /// a [path](CodecError::path): [`in_field`](CodecError::in_field) returns it
    /// unchanged. A hand-written [`Decode`](super::Decode) that rejects one
    /// field's *value* (a string that is not a valid port, say) should use
    /// [`expected`](CodecError::expected) or [`Message`](CodecError::Message)
    /// instead, so the error is located:
    ///
    /// ```
    /// use ferroly::codec::CodecError;
    ///
    /// let located = CodecError::Message("not a valid port".into()).in_field("port");
    /// assert_eq!(located.to_string(), "port: not a valid port");
    /// let unlocated = CodecError::parse("not a valid port").in_field("port");
    /// assert_eq!(unlocated.to_string(), "parse error: not a valid port");
    /// ```
    pub fn parse(msg: impl Into<String>) -> Self {
        CodecError::Parse(msg.into())
    }
    /// The value had the right type but the wrong shape or content (`1.5`
    /// for an integer, `-1` for an unsigned one, an array of the wrong
    /// length). For a value of the wrong type, use
    /// [`type_mismatch`](CodecError::type_mismatch), which also names what was
    /// found.
    pub fn expected(what: &'static str) -> Self {
        CodecError::Expected(what)
    }
    /// The value had the wrong type, and `found` is what it actually was:
    /// `Display` reads `expected string, found number`. `found` is named by its
    /// kind: `null`, `bool`, `number`, `string`, `bytes`, `array` or `object`.
    pub fn type_mismatch(expected: &'static str, found: &Value) -> Self {
        CodecError::TypeMismatch {
            expected,
            found: kind_of(found),
        }
    }
    /// A required field was missing.
    pub fn missing_field(name: impl Into<String>) -> Self {
        CodecError::MissingField(name.into())
    }
    /// An unknown enum variant string.
    pub fn unknown_variant(name: impl Into<String>) -> Self {
        CodecError::UnknownVariant(name.into())
    }
    /// A numeric out-of-range conversion for the named type.
    pub fn out_of_range(ty: &'static str) -> Self {
        CodecError::OutOfRange(ty)
    }
    /// A value the target format cannot represent.
    pub fn unrepresentable(msg: impl Into<String>) -> Self {
        CodecError::Unrepresentable(msg.into())
    }

    /// Records that this error was raised inside the field or map entry `key`
    /// (its wire name), prefixing the path.
    ///
    /// An error that already has a path gains one more leading segment rather
    /// than another `At` layer. Errors about the input as a whole — [`Parse`],
    /// [`LimitExceeded`] and [`UnsupportedContentType`] — already say where
    /// they happened and are returned unchanged.
    ///
    /// [`Parse`]: CodecError::Parse
    /// [`LimitExceeded`]: CodecError::LimitExceeded
    /// [`UnsupportedContentType`]: CodecError::UnsupportedContentType
    ///
    /// ```
    /// use ferroly::codec::CodecError;
    ///
    /// let err = CodecError::expected("string").in_field("role").in_index(1).in_field("messages");
    /// assert_eq!(err.to_string(), "messages[1].role: expected string");
    /// ```
    #[must_use]
    pub fn in_field(self, key: &str) -> Self {
        self.prefixed(|| PathSegment::Key(key.to_owned()))
    }

    /// Records that this error was raised inside sequence element `index`; see
    /// [`in_field`](CodecError::in_field).
    #[must_use]
    pub fn in_index(self, index: usize) -> Self {
        self.prefixed(|| PathSegment::Index(index))
    }

    fn prefixed(self, segment: impl FnOnce() -> PathSegment) -> Self {
        match self {
            CodecError::Parse(_)
            | CodecError::LimitExceeded(_)
            | CodecError::UnsupportedContentType(_) => self,
            CodecError::At {
                mut path,
                mut source,
            } => {
                // A hand-built nested `At` is merged into this layer.
                while let CodecError::At {
                    path: inner,
                    source: deeper,
                } = *source
                {
                    path.extend(inner);
                    source = deeper;
                }
                path.insert(0, segment());
                CodecError::At { path, source }
            }
            other => CodecError::At {
                path: vec![segment()],
                source: Box::new(other),
            },
        }
    }

    /// The path to where the error was raised, outermost first; empty for an
    /// error raised at the root.
    ///
    /// This is the outer [`At`](CodecError::At) layer's path, which is the
    /// whole path for every error the codec builds. An `At` nested by hand
    /// inside another shows only the outer part here, while
    /// [`path_string`](CodecError::path_string) and `Display` render all of
    /// it; [`in_field`](CodecError::in_field) merges such layers.
    pub fn path(&self) -> &[PathSegment] {
        match self {
            CodecError::At { path, .. } => path,
            _ => &[],
        }
    }

    /// The path rendered as `a.b[2].c`; empty for an error raised at the
    /// root.
    ///
    /// Keys that are not plain words are quoted (`a["my key"]`). Keys come
    /// from the input, so the rendering is made safe to log: control
    /// characters, Unicode line separators and bidirectional controls are
    /// escaped (`\n`, `\u{1b}`), and a key longer than 64 characters is cut
    /// and ends in `…`. [`path`](CodecError::path) keeps every key exactly as
    /// it was.
    pub fn path_string(&self) -> String {
        let mut out = String::new();
        let _ = write_full_path(&mut out, self);
        out
    }

    /// The underlying error, with the [`At`](CodecError::At) layer removed.
    pub fn root_cause(&self) -> &CodecError {
        match self {
            CodecError::At { source, .. } => source.root_cause(),
            other => other,
        }
    }
}

/// The kind name of `value`, as [`CodecError::type_mismatch`] reports it.
pub(crate) fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) | Value::UInt(_) | Value::Float(_) => "number",
        Value::Str(_) => "string",
        Value::Bytes(_) => "bytes",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::UnsupportedContentType(ct) => {
                write!(f, "no codec registered for content type: {ct}")
            }
            CodecError::Parse(m) => write!(f, "parse error: {m}"),
            CodecError::Expected(w) => write!(f, "expected {w}"),
            CodecError::MissingField(n) => write!(f, "missing field: {n}"),
            CodecError::UnknownVariant(n) => write!(f, "unknown variant: {n}"),
            CodecError::OutOfRange(t) => write!(f, "value out of range for {t}"),
            CodecError::Message(m) => write!(f, "{m}"),
            CodecError::LimitExceeded(m) => write!(f, "limit exceeded: {m}"),
            CodecError::Unrepresentable(m) => write!(f, "cannot encode: {m}"),
            CodecError::TypeMismatch { expected, found } => {
                write!(f, "expected {expected}, found {found}")
            }
            CodecError::At { .. } => {
                let (source, wrote_path) = write_full_path(f, self)?;
                if wrote_path {
                    f.write_str(": ")?;
                }
                write!(f, "{source}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_render_and_collapse_into_one_layer() {
        let err = CodecError::missing_field("c")
            .in_field("b")
            .in_index(2)
            .in_field("a");
        assert_eq!(err.to_string(), "a[2].b: missing field: c");
        assert_eq!(err.path_string(), "a[2].b");
        assert!(matches!(err.root_cause(), CodecError::MissingField(_)));
        match &err {
            CodecError::At { source, .. } => assert!(!matches!(**source, CodecError::At { .. })),
            _ => panic!("{err:?}"),
        }
    }

    #[test]
    fn odd_keys_are_quoted() {
        let err = CodecError::expected("x")
            .in_field("say \"hi\"")
            .in_field("my key")
            .in_field("top");
        assert_eq!(err.path_string(), r#"top["my key"]["say \"hi\""]"#);
        assert_eq!(err.path()[1].to_string(), "my key");
        let err = CodecError::expected("x").in_field("").in_index(0);
        assert_eq!(err.path_string(), r#"[0][""]"#);
        let err = CodecError::expected("x").in_field("max-tokens_2");
        assert_eq!(err.path_string(), "max-tokens_2");
    }

    #[test]
    fn keys_from_the_input_render_safely() {
        // Control characters cannot break the message over lines or reach a
        // terminal raw (CDC-4).
        let err = CodecError::expected("x")
            .in_field("a\nb\r\tc\u{1b}[31m\u{7f}\u{202e}")
            .in_field("top");
        let text = err.to_string();
        assert_eq!(
            text,
            r#"top["a\nb\r\tc\u{1b}[31m\u{7f}\u{202e}"]: expected x"#
        );
        assert!(!text.chars().any(char::is_control), "{text:?}");
        // The segment itself keeps the key as it was.
        assert_eq!(
            err.path()[1],
            PathSegment::Key("a\nb\r\tc\u{1b}[31m\u{7f}\u{202e}".into())
        );

        // A huge key is cut, plain or not, on a character boundary.
        let huge = "k".repeat(1 << 20);
        let err = CodecError::expected("x").in_field(&huge);
        let text = err.to_string();
        assert!(text.len() < 200, "{} bytes", text.len());
        assert_eq!(text, format!("[\"{}…\"]: expected x", "k".repeat(64)));
        let wide = "é".repeat(100);
        let rendered = CodecError::expected("x").in_field(&wide).path_string();
        assert_eq!(rendered, format!("[\"{}…\"]", "é".repeat(64)));
        // A plain key of exactly the cap is still written bare.
        let edge = "k".repeat(64);
        assert_eq!(
            CodecError::expected("x").in_field(&edge).path_string(),
            edge
        );
        assert_eq!(err.path()[0], PathSegment::Key(huge));
    }

    #[test]
    fn hand_built_at_values_are_tolerated() {
        // CDC-7: the fields are public, so these can exist.
        let empty = CodecError::At {
            path: vec![],
            source: Box::new(CodecError::expected("x")),
        };
        assert_eq!(empty.to_string(), "expected x");
        assert_eq!(empty.path_string(), "");

        let nested = CodecError::At {
            path: vec![PathSegment::Key("a".into())],
            source: Box::new(CodecError::At {
                path: vec![PathSegment::Index(1), PathSegment::Key("b".into())],
                source: Box::new(CodecError::missing_field("c")),
            }),
        };
        assert_eq!(nested.to_string(), "a[1].b: missing field: c");
        assert_eq!(nested.path_string(), "a[1].b");
        assert!(matches!(nested.root_cause(), CodecError::MissingField(_)));
        // The next segment merges the layers into one.
        let merged = nested.in_field("top");
        assert_eq!(merged.path_string(), "top.a[1].b");
        assert_eq!(merged.path().len(), 4);
        match &merged {
            CodecError::At { source, .. } => {
                assert!(matches!(**source, CodecError::MissingField(_)))
            }
            other => panic!("{other:?}"),
        }
        // An empty hand-built layer gains the segment like any other.
        let empty = CodecError::At {
            path: vec![],
            source: Box::new(CodecError::expected("x")),
        };
        assert_eq!(empty.in_index(0).to_string(), "[0]: expected x");
    }

    #[test]
    fn value_errors_from_hand_written_impls_are_located_but_parse_is_not() {
        // CDC-3: `parse` is for syntax errors in the input as a whole.
        let err = CodecError::Message("bad port".into()).in_field("port");
        assert_eq!(err.path_string(), "port");
        let err = CodecError::expected("port number").in_field("port");
        assert_eq!(err.to_string(), "port: expected port number");
        let err = CodecError::parse("bad port").in_field("port");
        assert_eq!(err, CodecError::Parse("bad port".into()));
    }

    #[test]
    fn root_errors_are_unchanged() {
        let err = CodecError::expected("string");
        assert_eq!(err.to_string(), "expected string");
        assert!(err.path().is_empty());
        assert_eq!(err.path_string(), "");
        assert_eq!(err.root_cause(), &err);
    }

    #[test]
    fn whole_input_errors_take_no_path() {
        for err in [
            CodecError::parse("bad"),
            CodecError::LimitExceeded("deep".into()),
            CodecError::UnsupportedContentType("x/y".into()),
        ] {
            assert_eq!(err.clone().in_field("a").in_index(1), err);
        }
    }

    #[test]
    fn type_mismatch_names_the_found_kind() {
        let cases = [
            (Value::Null, "null"),
            (Value::Bool(true), "bool"),
            (Value::Int(1), "number"),
            (Value::UInt(1), "number"),
            (Value::Float(1.5), "number"),
            (Value::Str("s".into()), "string"),
            (Value::Bytes(vec![1]), "bytes"),
            (Value::Array(vec![]), "array"),
            (Value::Object(vec![]), "object"),
        ];
        for (value, kind) in cases {
            let err = CodecError::type_mismatch("thing", &value);
            assert_eq!(err.to_string(), format!("expected thing, found {kind}"));
        }
    }
}
