//! A sandboxed, bounded engine for a defined subset of **Jinja2**, the
//! language LLM chat templates (and many configuration and report templates)
//! are written in.
//!
//! Parse a template once with [`Template::parse`], then render it any number
//! of times, from any number of threads, against a [`codec::Value`] context.
//! Parsing checks the whole template: a syntax error, an unknown filter, test
//! or function, or an unsupported statement is reported there, so a host can
//! refuse a template when it loads it rather than on the first render.
//!
//! ```
//! use ferroly::codec::json;
//! use ferroly::template::{Options, Template, TemplateErrorKind};
//!
//! // ChatML, as SmolLM2 ships it (abridged).
//! let source = "{% for message in messages %}{{ '<|im_start|>' + message['role'] + '\n' \
//!               + message['content'] + '<|im_end|>' + '\n' }}{% endfor %}\
//!               {% if add_generation_prompt %}{{ '<|im_start|>assistant\n' }}{% endif %}";
//! let template = Template::parse(source, &Options::default())?;
//!
//! let context = json::from_str(
//!     r#"{"messages":[{"role":"user","content":"Hi"}],"add_generation_prompt":true}"#,
//! )?;
//! assert_eq!(
//!     template.render(&context)?,
//!     "<|im_start|>user\nHi<|im_end|>\n<|im_start|>assistant\n",
//! );
//!
//! // A template can refuse its input with `raise_exception`.
//! let strict = Template::parse(
//!     "{% if messages|length > 1 %}{{ raise_exception('one message only') }}{% endif %}ok",
//!     &Options::default(),
//! )?;
//! let two = json::from_str(r#"{"messages":[{},{}]}"#)?;
//! let err = strict.render(&two).unwrap_err();
//! assert!(matches!(err.kind(), TemplateErrorKind::Raised(m) if m == "one message only"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # The language
//!
//! | Construct | Notes |
//! |---|---|
//! | `{{ expr }}`, text | values print as Python's `str()` would; **no autoescaping**, and printed data is never parsed again |
//! | `{% if %}` / `{% elif %}` / `{% else %}` | Python truthiness; an `if` opens no scope |
//! | `{% for x in xs [if cond] %}` … `{% else %}` | tuple unpacking (`for k, v in m.items()`), `{% break %}`, `{% continue %}`, `loop.index`, `index0`, `revindex`, `revindex0`, `first`, `last`, `length`, `previtem`, `nextitem`, `cycle()` |
//! | `{% set x = expr %}`, `{% set a, b = … %}`, `{% set x %}…{% endset %}` | a `set` inside a loop body lasts for that iteration; use `namespace()` for state that must survive the loop |
//! | `{% macro m(a, b=1) %}` | called as `m(1, b=2)`; returns its output as a string; one defined inside a loop or macro sees the variables visible there |
//! | `{# comment #}`, `{% raw %}` | |
//! | `{%-`, `-%}`, `{{-`, `-}}`, `{%+`, `+%}` | whitespace control; `trim_blocks` and `lstrip_blocks` are on by default, and one trailing newline at the end of the template is dropped, as in Jinja2 |
//! | literals | `'…'` / `"…"` (escapes `\n \t \\ \' \" \xHH \uXXXX`), integers, floats, `true` / `false` / `none` (and `True` / `False` / `None`), lists, maps |
//! | operators | `or and not`, `== != < > <= >=` (chainable), `in`, `not in`, `+ - ~ * / // % **`, unary `-`, `x if c else y`, `a[i]`, `a[1:-1:2]`, `a.b`, calls |
//! | tests | `defined undefined none string number integer float boolean mapping sequence iterable callable true false lower upper odd even divisibleby equalto eq ne lt le gt ge in sameas escaped` |
//! | filters | `abs attr capitalize center default (d) dictsort escape (e) first float indent int items join last length (count) list lower map max min reject rejectattr replace reverse round safe select selectattr sort string sum title tojson trim unique upper wordcount` |
//! | methods | strings: `strip lstrip rstrip lower upper title capitalize startswith endswith split rsplit splitlines replace find count join isalnum isalpha isdigit isspace islower isupper`; maps: `items keys values get`; lists: `index count` |
//! | globals | `range`, `namespace`, `dict`, `raise_exception` (on by default, see [`Options::raise_exception`]), and host functions ([`Options::function`]) |
//!
//! An unset name is **undefined**: it is false, prints as nothing, iterates
//! as empty and has length 0, but subscripting, calling it or using it in
//! arithmetic is an [`Undefined`](TemplateErrorKind::Undefined) error. A
//! missing map key or list index is undefined too.
//!
//! `tojson` follows the Hugging Face chat-template convention: keys stay in
//! the input order, nothing is HTML-escaped, non-ASCII text is written as is,
//! and `indent`, `separators`, `sort_keys` and `ensure_ascii` are accepted.
//!
//! `include`, `import`, `extends`, `block`, `call`, `filter`, `with` and the
//! other statements outside this subset are refused by `parse` with
//! [`Unsupported`](TemplateErrorKind::Unsupported).
//!
//! # Sandbox and limits
//!
//! A template sees only its context, its own variables and the registered
//! globals: there is no file, environment, clock or host-object access unless
//! the host registers a function for it. Every render is bounded by
//! [`Limits`]: source size, nesting depth (blocks, expressions and macro
//! calls), loop iterations, calls, output bytes, the bytes of strings and
//! lists the template builds, and the total work done (which is what bounds
//! the time a render takes). Exceeding one is a
//! [`LimitExceeded`](TemplateErrorKind::LimitExceeded) error naming it; no
//! input, template or data, makes the engine panic.
//!
//! This module is separate from the Handlebars-style `genai::template`
//! prompt engine; use this one to run templates written for Jinja2.
//!
//! [`codec::Value`]: crate::codec::Value

use std::fmt;
use std::sync::Arc;

use crate::codec::Value;

mod ast;
mod builtins;
mod error;
mod lexer;
mod parser;
mod render;
mod value;

pub use error::{TemplateError, TemplateErrorKind};

use builtins::Global;
use error::Fail;
use value::Func;

/// A host function: receives the positional arguments, returns a value or an
/// error message (reported as [`TemplateErrorKind::Function`]).
type HostFn = Arc<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>;

/// Resource limits for parsing and rendering. Every one is enforced; the
/// defaults are generous for real templates (the chat templates of common
/// models need a nesting depth under 10, under a hundred iterations and a few
/// kilobytes of output).
///
/// ```
/// use ferroly::template::Limits;
///
/// let tight = Limits { max_iterations: 10_000, max_output_bytes: 64 * 1024, ..Limits::default() };
/// assert_eq!(tight.max_depth, Limits::default().max_depth);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest template source accepted by [`Template::parse`], in bytes.
    /// Parsing holds the tokens and the syntax tree at once, which peaks at
    /// up to about 100 times the source size.
    pub max_source_bytes: usize,
    /// Deepest nesting of blocks, expression terms and macro calls, checked
    /// at parse time and again while rendering.
    pub max_depth: usize,
    /// Total loop iterations per render (filters that walk a list count its
    /// items too).
    pub max_iterations: usize,
    /// Total filter, test, function, method and macro calls per render.
    pub max_calls: usize,
    /// Largest rendered output, in bytes (macro and `set` block output
    /// included).
    pub max_output_bytes: usize,
    /// Total bytes of strings and lists the template may build during one
    /// render (concatenations, filter results, captured output). Bounds the
    /// memory and copying work a template can cause.
    pub max_alloc_bytes: usize,
    /// Total work per render: one step per statement, expression evaluated,
    /// loop iteration and call, plus steps in proportion to the size of what
    /// an operation walks (one per list item or map entry compared, copied
    /// or scanned, one per 64 bytes of text searched, compared or sliced).
    /// This is what bounds render time: the other limits bound counts and
    /// sizes, this one bounds the work done with them.
    pub max_steps: usize,
}

impl Limits {
    /// Default [`max_source_bytes`](Self::max_source_bytes): 256 KiB (real
    /// chat templates are under 20 KB).
    pub const DEFAULT_MAX_SOURCE_BYTES: usize = 256 << 10;
    /// Default [`max_depth`](Self::max_depth): 64.
    pub const DEFAULT_MAX_DEPTH: usize = 64;
    /// Default [`max_iterations`](Self::max_iterations): one million.
    pub const DEFAULT_MAX_ITERATIONS: usize = 1_000_000;
    /// Default [`max_calls`](Self::max_calls): one million.
    pub const DEFAULT_MAX_CALLS: usize = 1_000_000;
    /// Default [`max_output_bytes`](Self::max_output_bytes): 16 MiB.
    pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 << 20;
    /// Default [`max_alloc_bytes`](Self::max_alloc_bytes): 256 MiB.
    pub const DEFAULT_MAX_ALLOC_BYTES: usize = 256 << 20;
    /// Default [`max_steps`](Self::max_steps): one hundred million, a few
    /// seconds of work (a real chat template needs well under a hundred
    /// thousand).
    pub const DEFAULT_MAX_STEPS: usize = 100_000_000;
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_source_bytes: Self::DEFAULT_MAX_SOURCE_BYTES,
            max_depth: Self::DEFAULT_MAX_DEPTH,
            max_iterations: Self::DEFAULT_MAX_ITERATIONS,
            max_calls: Self::DEFAULT_MAX_CALLS,
            max_output_bytes: Self::DEFAULT_MAX_OUTPUT_BYTES,
            max_alloc_bytes: Self::DEFAULT_MAX_ALLOC_BYTES,
            max_steps: Self::DEFAULT_MAX_STEPS,
        }
    }
}

/// How a template is parsed and what it may call.
///
/// The defaults match Hugging Face `transformers` and llama.cpp for chat
/// templates: `trim_blocks` and `lstrip_blocks` on, one trailing newline
/// dropped, `raise_exception` available, no autoescaping (there is none).
///
/// ```
/// use ferroly::codec::Value;
/// use ferroly::template::{Limits, Options, Template};
///
/// let options = Options::default()
///     .function("strftime_now", |args: &[Value]| -> Result<Value, String> {
///         let format = args.first().and_then(Value::as_str).unwrap_or("");
///         Ok(Value::from(format!("<date {format}>")))
///     })
///     .limits(Limits { max_output_bytes: 1 << 20, ..Limits::default() });
/// let t = Template::parse("{{ strftime_now('%d %b %Y') }}", &options).unwrap();
/// assert_eq!(t.render(&Value::Null).unwrap(), "<date %d %b %Y>");
/// ```
#[derive(Clone)]
pub struct Options {
    trim_blocks: bool,
    lstrip_blocks: bool,
    keep_trailing_newline: bool,
    raise_exception: bool,
    functions: Vec<(String, HostFn)>,
    limits: Limits,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            trim_blocks: true,
            lstrip_blocks: true,
            keep_trailing_newline: false,
            raise_exception: true,
            functions: Vec::new(),
            limits: Limits::default(),
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("trim_blocks", &self.trim_blocks)
            .field("lstrip_blocks", &self.lstrip_blocks)
            .field("keep_trailing_newline", &self.keep_trailing_newline)
            .field("raise_exception", &self.raise_exception)
            .field(
                "functions",
                &self
                    .functions
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("limits", &self.limits)
            .finish()
    }
}

impl Options {
    /// The defaults (same as [`Options::default`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the first newline after a `{% … %}` or `{# … #}` tag (default
    /// on).
    pub fn trim_blocks(mut self, on: bool) -> Self {
        self.trim_blocks = on;
        self
    }

    /// Strip spaces and tabs before a `{% … %}` or `{# … #}` tag that starts
    /// a line (default on).
    pub fn lstrip_blocks(mut self, on: bool) -> Self {
        self.lstrip_blocks = on;
        self
    }

    /// Keep a single newline at the very end of the template (default off,
    /// as in Jinja2).
    pub fn keep_trailing_newline(mut self, on: bool) -> Self {
        self.keep_trailing_newline = on;
        self
    }

    /// Provide the `raise_exception(message)` global (default on). Templates
    /// in the wild assume it; turn it off only if the host supplies its own
    /// with [`function`](Self::function).
    pub fn raise_exception(mut self, on: bool) -> Self {
        self.raise_exception = on;
        self
    }

    /// Registers a host function callable from templates as `name(args…)`.
    /// It receives the positional arguments as [`Value`]s (keyword arguments
    /// are refused) and returns a value or an error message. A host function
    /// shadows a built-in global of the same name; registering a name again
    /// replaces it.
    pub fn function<F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&[Value]) -> Result<Value, String> + Send + Sync + 'static,
    {
        let name = name.into();
        self.functions.retain(|(n, _)| *n != name);
        self.functions.push((name, Arc::new(f)));
        self
    }

    /// Sets the resource limits.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
}

pub(crate) struct Inner {
    source: Box<str>,
    body: Vec<ast::Stmt>,
    limits: Limits,
    raise_exception: bool,
    functions: Vec<(String, HostFn)>,
}

impl Inner {
    fn global(&self, name: &str) -> Option<Func> {
        if let Some(i) = self.functions.iter().position(|(n, _)| n == name) {
            return Some(Func::Host(i));
        }
        Global::from_name(name, self.raise_exception).map(Func::Global)
    }
}

/// A parsed template: immutable, `Send + Sync`, cheap to clone, and
/// renderable any number of times without parsing again.
#[derive(Clone)]
pub struct Template {
    inner: Arc<Inner>,
}

impl fmt::Debug for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Template")
            .field("source_bytes", &self.inner.source.len())
            .field("limits", &self.inner.limits)
            .finish_non_exhaustive()
    }
}

impl Template {
    /// Parses and checks `source`.
    ///
    /// Fails on a syntax error, an unknown filter, test or function, an
    /// unsupported statement, a nesting depth over [`Limits::max_depth`] or a
    /// source over [`Limits::max_source_bytes`].
    pub fn parse(source: &str, options: &Options) -> Result<Template, TemplateError> {
        let limits = options.limits;
        if source.len() > limits.max_source_bytes {
            return Err(Fail::limit("max_source_bytes", 0).locate(source));
        }
        let lex = lexer::LexOptions {
            trim_blocks: options.trim_blocks,
            lstrip_blocks: options.lstrip_blocks,
            keep_trailing_newline: options.keep_trailing_newline,
        };
        let tokens = lexer::tokenize(source, lex).map_err(|f| f.locate(source))?;
        let functions = &options.functions;
        let raise = options.raise_exception;
        let is_global = |name: &str| {
            functions.iter().any(|(n, _)| n == name) || Global::from_name(name, raise).is_some()
        };
        let body = parser::Parser::new(tokens, limits.max_depth, &is_global)
            .parse()
            .map_err(|f| f.locate(source))?;
        Ok(Template {
            inner: Arc::new(Inner {
                source: source.into(),
                body,
                limits,
                raise_exception: options.raise_exception,
                functions: options.functions.clone(),
            }),
        })
    }

    /// Renders the template. `context` is a map ([`Value::Object`]) whose
    /// entries are the template's variables; [`Value::Null`] is an empty
    /// context. The context is only read, and maps iterate in the `Value`'s
    /// own key order, so output is deterministic.
    pub fn render(&self, context: &Value) -> Result<String, TemplateError> {
        render::render(&self.inner, context)
    }

    /// The source text the template was parsed from.
    pub fn source(&self) -> &str {
        &self.inner.source
    }
}
