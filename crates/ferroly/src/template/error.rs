//! The typed error every parse and render failure is reported as.

use std::fmt;

/// What went wrong. Carried by [`TemplateError`] together with the position
/// in the template source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TemplateErrorKind {
    /// The template text is malformed: an unclosed tag or string, a stray
    /// `{% endfor %}`, an unexpected token.
    Syntax(String),
    /// A `|filter` name that the engine does not provide.
    UnknownFilter(String),
    /// An `is test` name that the engine does not provide.
    UnknownTest(String),
    /// A call to a function that is neither a built-in global, a registered
    /// host function, nor a macro or variable of the template.
    UnknownFunction(String),
    /// A real Jinja2 construct this engine deliberately leaves out
    /// (`include`, `import`, `extends`, `block`, `call` blocks, ...).
    Unsupported(String),
    /// An operation applied to values of the wrong type (`'a' + [1]`, a
    /// division by zero, a bad filter argument).
    Type(String),
    /// An undefined value was used where a value is required: subscripted,
    /// called, or used in arithmetic or ordering.
    Undefined(String),
    /// The template called `raise_exception(message)`.
    Raised(String),
    /// A host function registered with
    /// [`Options::function`](super::Options::function) returned an error.
    Function {
        /// The function's name.
        name: String,
        /// The message it returned.
        message: String,
    },
    /// A [`Limits`](super::Limits) budget was exceeded; the payload is the
    /// limit's field name (`"max_depth"`, `"max_iterations"`, ...).
    LimitExceeded(&'static str),
}

impl fmt::Display for TemplateErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateErrorKind::Syntax(m) => write!(f, "syntax error: {m}"),
            TemplateErrorKind::UnknownFilter(n) => write!(f, "unknown filter '{n}'"),
            TemplateErrorKind::UnknownTest(n) => write!(f, "unknown test '{n}'"),
            TemplateErrorKind::UnknownFunction(n) => write!(f, "unknown function '{n}'"),
            TemplateErrorKind::Unsupported(m) => write!(f, "unsupported: {m}"),
            TemplateErrorKind::Type(m) => write!(f, "type error: {m}"),
            TemplateErrorKind::Undefined(m) => write!(f, "undefined value: {m}"),
            TemplateErrorKind::Raised(m) => write!(f, "template raised an exception: {m}"),
            TemplateErrorKind::Function { name, message } => {
                write!(f, "function '{name}' failed: {message}")
            }
            TemplateErrorKind::LimitExceeded(l) => write!(f, "limit exceeded: {l}"),
        }
    }
}

/// A parse or render failure, with its position in the template source.
///
/// `offset` is the byte offset into the source passed to
/// [`Template::parse`](super::Template::parse); `line` and `column` are
/// 1-based, the column counted in characters. `\n`, `\r\n` and `\r` each end a
/// line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    kind: TemplateErrorKind,
    offset: usize,
    line: u32,
    column: u32,
}

impl TemplateError {
    /// What went wrong.
    pub fn kind(&self) -> &TemplateErrorKind {
        &self.kind
    }

    /// The byte offset in the template source.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The 1-based line number.
    pub fn line(&self) -> u32 {
        self.line
    }

    /// The 1-based column, in characters.
    pub fn column(&self) -> u32 {
        self.column
    }

    /// The message passed to `raise_exception`, when that is what stopped
    /// rendering.
    pub fn raised_message(&self) -> Option<&str> {
        match &self.kind {
            TemplateErrorKind::Raised(m) => Some(m),
            _ => None,
        }
    }

    pub(crate) fn locate(kind: TemplateErrorKind, offset: usize, source: &str) -> Self {
        let offset = offset.min(source.len());
        let mut line: u32 = 1;
        let mut column: u32 = 1;
        let mut prev_cr = false;
        for (i, c) in source.char_indices() {
            if i >= offset {
                break;
            }
            match c {
                '\n' if prev_cr => {}
                '\n' | '\r' => {
                    line = line.saturating_add(1);
                    column = 1;
                }
                _ => column = column.saturating_add(1),
            }
            prev_cr = c == '\r';
        }
        TemplateError {
            kind,
            offset,
            line,
            column,
        }
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (line {}, column {})",
            self.kind, self.line, self.column
        )
    }
}

impl std::error::Error for TemplateError {}

/// An error before its position has been turned into a line and column.
#[derive(Debug)]
pub(crate) struct Fail {
    pub(crate) kind: TemplateErrorKind,
    pub(crate) pos: usize,
}

impl Fail {
    pub(crate) fn new(kind: TemplateErrorKind, pos: usize) -> Self {
        Fail { kind, pos }
    }
    pub(crate) fn syntax(msg: impl Into<String>, pos: usize) -> Self {
        Fail::new(TemplateErrorKind::Syntax(msg.into()), pos)
    }
    pub(crate) fn ty(msg: impl Into<String>, pos: usize) -> Self {
        Fail::new(TemplateErrorKind::Type(msg.into()), pos)
    }
    pub(crate) fn undefined(msg: impl Into<String>, pos: usize) -> Self {
        Fail::new(TemplateErrorKind::Undefined(msg.into()), pos)
    }
    pub(crate) fn limit(name: &'static str, pos: usize) -> Self {
        Fail::new(TemplateErrorKind::LimitExceeded(name), pos)
    }
    pub(crate) fn locate(self, source: &str) -> TemplateError {
        TemplateError::locate(self.kind, self.pos, source)
    }
}

pub(crate) type R<T> = Result<T, Fail>;
