//! A small in-house template engine for prompts.
//!
//! | Syntax | Meaning |
//! |---|---|
//! | `{{ name }}`, `{{ user.name }}` | substitute a variable (dot-path lookup) |
//! | `{{ .name }}` | the same — the Go `text/template` leading dot is tolerated |
//! | `{{#if x}}…{{else}}…{{/if}}` | conditional on the truthiness of `x` |
//! | `{{#unless x}}…{{/unless}}` | the inverse |
//! | `{{#each xs}}…{{else}}…{{/each}}` | repeat for every element of an array (or value of an object); `{{else}}` renders when it is empty |
//! | `{{ this }}` / `{{ . }}`, `{{ this.field }}` | the current element inside `#each` |
//! | `{{ @index }}`, `{{ @first }}`, `{{ @last }}`, `{{ @key }}` | loop metadata inside `#each` |
//! | `{{> name }}` | include the partial template `name` |
//! | `{{! note }}` | a comment, renders nothing |
//! | `\{{` | a literal `{{` |
//!
//! Substituted values are inserted verbatim and are **never parsed as
//! template text**, so a variable that contains `{{ … }}` cannot inject
//! template logic or read other variables.
//!
//! # Strict and lenient rendering
//!
//! [`Template`] — the compiled form, and the entry point for new code — is
//! **strict** by default: a variable that is not defined is an error instead
//! of silently producing a prompt with a hole in it. The module-level
//! [`render`] / [`render_value`] functions keep the original lenient behaviour
//! (an undefined variable renders as the empty string). [`RenderOptions`]
//! selects either explicitly.
//!
//! In both modes `{{ . }}` (or an empty tag) at the top level does **not**
//! dump the whole variables object any more — a prompt must not leak every
//! variable by accident. It is an error in strict mode and empty in lenient
//! mode unless [`RenderOptions::allow_root`] is set. Inside `#each` it is the
//! current element, as usual.
//!
//! ```
//! use ferroly::codec::{json, Encode};
//! use ferroly::genai::template::Template;
//!
//! #[derive(Encode)]
//! struct Vars { name: String, items: Vec<String>, vip: bool }
//!
//! let t = Template::parse(
//!     "Hi {{ name }}{{#if vip}} (VIP){{/if}}.\n{{#each items}}{{ @index }}: {{ this }}\n{{/each}}",
//! ).unwrap();
//! let out = t.render(&Vars {
//!     name: "Ada".into(),
//!     items: vec!["tea".into(), "cake".into()],
//!     vip: true,
//! }).unwrap();
//! assert_eq!(out, "Hi Ada (VIP).\n0: tea\n1: cake\n");
//!
//! // Strict by default: a typo in a variable name is an error.
//! let vars = json::from_str(r#"{"name":"Ada"}"#).unwrap();
//! assert!(Template::parse("Hi {{ nmae }}").unwrap().render_value(&vars).is_err());
//! ```

use std::collections::HashMap;

use ferroly::codec::{Encode, Value};

use ferroly::genai::GenAiError;

/// The deepest nesting of `#if` / `#each` blocks a template may have.
const MAX_BLOCK_DEPTH: usize = 32;
/// The deepest chain of partials including partials.
const MAX_PARTIAL_DEPTH: usize = 16;
/// The default cap on rendered output.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Where `{{> name }}` partials come from.
pub trait PartialSource {
    /// The source text of the partial called `name`, if it exists.
    fn partial(&self, name: &str) -> Option<String>;
}

impl PartialSource for HashMap<String, String> {
    fn partial(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

/// How a template is rendered.
#[derive(Clone, Copy)]
pub struct RenderOptions<'a> {
    strict: bool,
    allow_root: bool,
    partials: Option<&'a dyn PartialSource>,
    max_output_bytes: usize,
}

impl std::fmt::Debug for RenderOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderOptions")
            .field("strict", &self.strict)
            .field("allow_root", &self.allow_root)
            .field("partials", &self.partials.is_some())
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

impl Default for RenderOptions<'_> {
    /// Strict rendering.
    fn default() -> Self {
        RenderOptions::strict()
    }
}

impl<'a> RenderOptions<'a> {
    /// An undefined variable is an error. The default.
    pub fn strict() -> Self {
        RenderOptions {
            strict: true,
            allow_root: false,
            partials: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// An undefined variable renders as the empty string.
    pub fn lenient() -> Self {
        RenderOptions {
            strict: false,
            ..RenderOptions::strict()
        }
    }

    /// Lets `{{ . }}` at the top level render the whole variables value (as
    /// JSON when it is an object or array). Off by default.
    pub fn allow_root(mut self, allow: bool) -> Self {
        self.allow_root = allow;
        self
    }

    /// Where `{{> name }}` looks partials up. Without a source every partial
    /// is an error.
    pub fn partials(mut self, partials: &'a dyn PartialSource) -> Self {
        self.partials = Some(partials);
        self
    }

    /// Caps the size of the rendered output (default 8 MiB), so a loop over
    /// attacker-sized data cannot exhaust memory.
    pub fn max_output_bytes(mut self, max: usize) -> Self {
        self.max_output_bytes = max;
        self
    }
}

/// What a `{{ … }}` expression refers to.
#[derive(Debug, Clone, PartialEq)]
enum Path {
    /// `.`, `this`, or an empty tag: the current element (or the root).
    Current,
    /// `this.a.b`: a path inside the current element.
    InCurrent(Vec<String>),
    /// `a.b`: looked up in the innermost scope that defines `a`.
    Scoped(Vec<String>),
    /// `@index`, `@first`, `@last`, `@key`.
    Meta(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Var(Path),
    If {
        cond: Path,
        negate: bool,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
    Each {
        list: Path,
        body: Vec<Node>,
        otherwise: Vec<Node>,
    },
    Partial(String),
}

fn err(message: impl Into<String>) -> GenAiError {
    GenAiError::Template(message.into())
}

fn parse_path(expr: &str) -> Path {
    let expr = expr.trim();
    if let Some(meta) = expr.strip_prefix('@') {
        return Path::Meta(meta.trim().to_string());
    }
    let segments = |text: &str| -> Vec<String> {
        text.split('.')
            .map(str::trim)
            .filter(|seg| !seg.is_empty())
            .map(str::to_string)
            .collect()
    };
    if expr == "this" {
        return Path::Current;
    }
    if let Some(rest) = expr.strip_prefix("this.") {
        let segs = segments(rest);
        return if segs.is_empty() {
            Path::Current
        } else {
            Path::InCurrent(segs)
        };
    }
    let segs = segments(expr);
    if segs.is_empty() {
        Path::Current // `{{ . }}`, `{{ }}`
    } else {
        Path::Scoped(segs)
    }
}

/// A block that has been opened and not yet closed.
struct Frame {
    name: &'static str,
    path: Path,
    negate: bool,
    then: Vec<Node>,
    otherwise: Option<Vec<Node>>,
}

impl Frame {
    fn nodes(&mut self) -> &mut Vec<Node> {
        match &mut self.otherwise {
            Some(nodes) => nodes,
            None => &mut self.then,
        }
    }

    fn close(self) -> Node {
        let otherwise = self.otherwise.unwrap_or_default();
        if self.name == "each" {
            Node::Each {
                list: self.path,
                body: self.then,
                otherwise,
            }
        } else {
            Node::If {
                cond: self.path,
                negate: self.negate,
                then: self.then,
                otherwise,
            }
        }
    }
}

fn push_text(nodes: &mut Vec<Node>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Node::Text(existing)) = nodes.last_mut() {
        existing.push_str(text);
    } else {
        nodes.push(Node::Text(text.to_string()));
    }
}

/// A compiled template.
///
/// Parse once with [`Template::parse`], render many times. Rendering is strict
/// unless [`RenderOptions::lenient`] is passed to
/// [`render_with`](Template::render_with).
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    nodes: Vec<Node>,
}

impl Template {
    /// Compiles a template, reporting unclosed tags, unknown or mismatched
    /// blocks, and blocks nested deeper than 32 levels.
    pub fn parse(source: &str) -> Result<Template, GenAiError> {
        let mut root: Vec<Node> = Vec::new();
        let mut stack: Vec<Frame> = Vec::new();
        let mut pos = 0usize;

        while let Some(found) = source[pos..].find("{{") {
            let open = pos + found;
            let current = match stack.last_mut() {
                Some(frame) => frame.nodes(),
                None => &mut root,
            };
            // `\{{` is a literal `{{`.
            if source[..open].ends_with('\\') {
                push_text(current, source.get(pos..open - 1).unwrap_or(""));
                push_text(current, "{{");
                pos = open + 2;
                continue;
            }
            push_text(current, &source[pos..open]);
            let Some(close) = source[open + 2..].find("}}") else {
                return Err(err("unclosed '{{' in template"));
            };
            let expr = source[open + 2..open + 2 + close].trim();
            pos = open + 2 + close + 2;

            if expr.starts_with('!') {
                continue; // comment
            }
            if let Some(rest) = expr.strip_prefix('#') {
                let rest = rest.trim();
                let (name, arg) = match rest.split_once(char::is_whitespace) {
                    Some((name, arg)) => (name, arg.trim()),
                    None => (rest, ""),
                };
                let (name, negate): (&'static str, bool) = match name {
                    "if" => ("if", false),
                    "unless" => ("unless", true),
                    "each" => ("each", false),
                    other => return Err(err(format!("unknown block '#{other}'"))),
                };
                if arg.is_empty() {
                    return Err(err(format!("'#{name}' needs an expression")));
                }
                if stack.len() >= MAX_BLOCK_DEPTH {
                    return Err(err(format!(
                        "blocks are nested deeper than {MAX_BLOCK_DEPTH} levels"
                    )));
                }
                stack.push(Frame {
                    name,
                    path: parse_path(arg),
                    negate,
                    then: Vec::new(),
                    otherwise: None,
                });
                continue;
            }
            if let Some(rest) = expr.strip_prefix('/') {
                let name = rest.trim();
                match stack.pop() {
                    Some(frame) if frame.name == name => {
                        let node = frame.close();
                        match stack.last_mut() {
                            Some(parent) => parent.nodes().push(node),
                            None => root.push(node),
                        }
                    }
                    Some(frame) => {
                        return Err(err(format!(
                            "'/{name}' closes '#{}' — blocks must be closed in order",
                            frame.name
                        )))
                    }
                    None => return Err(err(format!("'/{name}' has no matching block"))),
                }
                continue;
            }
            if expr == "else" {
                match stack.last_mut() {
                    Some(frame) if frame.otherwise.is_none() => frame.otherwise = Some(Vec::new()),
                    Some(frame) => {
                        return Err(err(format!("'#{}' has more than one 'else'", frame.name)))
                    }
                    None => return Err(err("'else' outside of a block")),
                }
                continue;
            }
            let node = match expr.strip_prefix('>') {
                Some(name) => {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(err("'>' needs a partial name"));
                    }
                    Node::Partial(name.to_string())
                }
                None => Node::Var(parse_path(expr)),
            };
            match stack.last_mut() {
                Some(frame) => frame.nodes().push(node),
                None => root.push(node),
            }
        }
        if let Some(frame) = stack.last() {
            return Err(err(format!("'#{}' is never closed", frame.name)));
        }
        push_text(&mut root, &source[pos..]);
        Ok(Template { nodes: root })
    }

    /// Renders with the encoded `vars`, strictly.
    pub fn render<T: Encode>(&self, vars: &T) -> Result<String, GenAiError> {
        self.render_with(&vars.encode(), &RenderOptions::strict())
    }

    /// Renders against an already-built [`Value`], strictly.
    pub fn render_value(&self, vars: &Value) -> Result<String, GenAiError> {
        self.render_with(vars, &RenderOptions::strict())
    }

    /// Renders with explicit options.
    pub fn render_with(
        &self,
        vars: &Value,
        options: &RenderOptions<'_>,
    ) -> Result<String, GenAiError> {
        let mut out = String::new();
        let root = Scope {
            value: vars,
            item: None,
            parent: None,
        };
        render_nodes(&self.nodes, &root, options, 0, &mut out)?;
        Ok(out)
    }

    /// The top-level variable names the template refers to, sorted and
    /// de-duplicated — what a caller must supply for a strict render (names
    /// used only inside `#each` bodies may belong to the loop element instead
    /// and are included too).
    pub fn variables(&self) -> Vec<String> {
        fn add(path: &Path, out: &mut Vec<String>) {
            if let Path::Scoped(segs) = path {
                out.push(segs[0].clone());
            }
        }
        fn walk(nodes: &[Node], out: &mut Vec<String>) {
            for node in nodes {
                match node {
                    Node::Var(path) => add(path, out),
                    Node::If {
                        cond,
                        then,
                        otherwise,
                        ..
                    } => {
                        add(cond, out);
                        walk(then, out);
                        walk(otherwise, out);
                    }
                    Node::Each {
                        list,
                        body,
                        otherwise,
                    } => {
                        add(list, out);
                        walk(body, out);
                        walk(otherwise, out);
                    }
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.nodes, &mut out);
        out.sort();
        out.dedup();
        out
    }
}

/// Loop position of the element a scope stands for.
#[derive(Clone, Copy)]
struct Item<'a> {
    index: usize,
    len: usize,
    key: Option<&'a str>,
}

/// One level of variable lookup: the root variables, or a loop element.
struct Scope<'a> {
    value: &'a Value,
    item: Option<Item<'a>>,
    parent: Option<&'a Scope<'a>>,
}

fn descend<'a>(mut value: &'a Value, segments: &[String]) -> Option<&'a Value> {
    for seg in segments {
        value = match value {
            Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            other => other.get(seg)?,
        };
    }
    Some(value)
}

/// What an expression evaluates to.
enum Resolved<'a> {
    Value(&'a Value),
    Owned(Value),
    Missing,
    /// `{{ . }}` at the top level without `allow_root`.
    RootDenied,
}

fn resolve<'a>(path: &Path, scope: &'a Scope<'a>, options: &RenderOptions<'_>) -> Resolved<'a> {
    match path {
        Path::Current => {
            if scope.parent.is_none() && !options.allow_root {
                Resolved::RootDenied
            } else {
                Resolved::Value(scope.value)
            }
        }
        Path::InCurrent(segs) => match descend(scope.value, segs) {
            Some(v) => Resolved::Value(v),
            None => Resolved::Missing,
        },
        Path::Scoped(segs) => {
            let mut level = Some(scope);
            while let Some(s) = level {
                // The innermost scope that defines the first segment decides.
                if s.value.get(&segs[0]).is_some() {
                    return match descend(s.value, segs) {
                        Some(v) => Resolved::Value(v),
                        None => Resolved::Missing,
                    };
                }
                level = s.parent;
            }
            Resolved::Missing
        }
        Path::Meta(name) => {
            let Some(item) = scope.item else {
                return Resolved::Missing;
            };
            match name.as_str() {
                "index" => Resolved::Owned(Value::UInt(item.index as u64)),
                "first" => Resolved::Owned(Value::Bool(item.index == 0)),
                "last" => Resolved::Owned(Value::Bool(item.index + 1 == item.len)),
                "key" => match item.key {
                    Some(key) => Resolved::Owned(Value::Str(key.to_string())),
                    None => Resolved::Owned(Value::UInt(item.index as u64)),
                },
                _ => Resolved::Missing,
            }
        }
    }
}

fn describe(path: &Path) -> String {
    match path {
        Path::Current => ".".to_string(),
        Path::InCurrent(segs) => format!("this.{}", segs.join(".")),
        Path::Scoped(segs) => segs.join("."),
        Path::Meta(name) => format!("@{name}"),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::UInt(u) => *u != 0,
        Value::Float(f) => *f != 0.0 && !f.is_nan(),
        Value::Str(s) => !s.is_empty(),
        Value::Bytes(b) => !b.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => {}
        Value::Str(s) => out.push_str(s),
        Value::Bool(b) => out.push_str(&b.to_string()),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::UInt(u) => out.push_str(&u.to_string()),
        Value::Float(f) => out.push_str(&f.to_string()),
        other => out.push_str(&ferroly::codec::json::to_string(other)),
    }
}

fn check_size(out: &str, options: &RenderOptions<'_>) -> Result<(), GenAiError> {
    if out.len() > options.max_output_bytes {
        Err(err(format!(
            "rendered output exceeds {} bytes",
            options.max_output_bytes
        )))
    } else {
        Ok(())
    }
}

fn render_nodes(
    nodes: &[Node],
    scope: &Scope<'_>,
    options: &RenderOptions<'_>,
    partial_depth: usize,
    out: &mut String,
) -> Result<(), GenAiError> {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Var(path) => match resolve(path, scope, options) {
                Resolved::Value(v) => write_value(v, out),
                Resolved::Owned(v) => write_value(&v, out),
                Resolved::Missing if options.strict => {
                    return Err(err(format!("undefined variable '{}'", describe(path))))
                }
                Resolved::RootDenied if options.strict => {
                    return Err(err(
                        "'{{ . }}' would render the whole variables object; name a variable, \
                         or opt in with RenderOptions::allow_root",
                    ))
                }
                Resolved::Missing | Resolved::RootDenied => {}
            },
            Node::If {
                cond,
                negate,
                then,
                otherwise,
            } => {
                // Testing for presence is what `#if` is for, so an undefined
                // variable is simply false, also in strict mode.
                let truth = match resolve(cond, scope, options) {
                    Resolved::Value(v) => truthy(v),
                    Resolved::Owned(v) => truthy(&v),
                    Resolved::Missing | Resolved::RootDenied => false,
                };
                let branch = if truth != *negate { then } else { otherwise };
                render_nodes(branch, scope, options, partial_depth, out)?;
            }
            Node::Each {
                list,
                body,
                otherwise,
            } => {
                let owned;
                let value: &Value = match resolve(list, scope, options) {
                    Resolved::Value(v) => v,
                    Resolved::Owned(v) => {
                        owned = v;
                        &owned
                    }
                    Resolved::Missing | Resolved::RootDenied if options.strict => {
                        return Err(err(format!(
                            "undefined variable '{}' in '#each'",
                            describe(list)
                        )))
                    }
                    Resolved::Missing | Resolved::RootDenied => &Value::Null,
                };
                let mut rendered_any = false;
                match value {
                    Value::Array(items) => {
                        for (index, item) in items.iter().enumerate() {
                            rendered_any = true;
                            let inner = Scope {
                                value: item,
                                item: Some(Item {
                                    index,
                                    len: items.len(),
                                    key: None,
                                }),
                                parent: Some(scope),
                            };
                            render_nodes(body, &inner, options, partial_depth, out)?;
                            check_size(out, options)?;
                        }
                    }
                    Value::Object(entries) => {
                        for (index, (key, item)) in entries.iter().enumerate() {
                            rendered_any = true;
                            let inner = Scope {
                                value: item,
                                item: Some(Item {
                                    index,
                                    len: entries.len(),
                                    key: Some(key),
                                }),
                                parent: Some(scope),
                            };
                            render_nodes(body, &inner, options, partial_depth, out)?;
                            check_size(out, options)?;
                        }
                    }
                    Value::Null => {}
                    _ if options.strict => {
                        return Err(err(format!(
                            "'#each {}' needs an array or an object",
                            describe(list)
                        )))
                    }
                    _ => {}
                }
                if !rendered_any {
                    render_nodes(otherwise, scope, options, partial_depth, out)?;
                }
            }
            Node::Partial(name) => {
                if partial_depth >= MAX_PARTIAL_DEPTH {
                    return Err(err(format!(
                        "partials are nested deeper than {MAX_PARTIAL_DEPTH} levels \
                         (is '{name}' including itself?)"
                    )));
                }
                let source = options
                    .partials
                    .and_then(|p| p.partial(name))
                    .ok_or_else(|| err(format!("partial '{name}' not found")))?;
                let partial = Template::parse(&source)
                    .map_err(|e| err(format!("in partial '{name}': {e}")))?;
                render_nodes(&partial.nodes, scope, options, partial_depth + 1, out)?;
            }
        }
        check_size(out, options)?;
    }
    Ok(())
}

/// Renders `template`, substituting from the encoded `vars`, **leniently**: an
/// undefined variable renders as the empty string. Prefer [`Template`] (strict)
/// for new code.
pub fn render<T: Encode>(template: &str, vars: &T) -> Result<String, GenAiError> {
    render_value(template, &vars.encode())
}

/// Renders `template` against an already-built [`Value`], **leniently**.
pub fn render_value(template: &str, vars: &Value) -> Result<String, GenAiError> {
    Template::parse(template)?.render_with(vars, &RenderOptions::lenient())
}

/// Renders `template` from the encoded `vars`, **strictly**: an undefined
/// variable is an error.
pub fn render_strict<T: Encode>(template: &str, vars: &T) -> Result<String, GenAiError> {
    Template::parse(template)?.render(vars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::json;

    #[derive(Encode)]
    struct Vars {
        name: String,
        count: u32,
    }

    fn vars() -> Vars {
        Vars {
            name: "Ada".into(),
            count: 3,
        }
    }

    fn v(text: &str) -> Value {
        json::from_str(text).unwrap()
    }

    #[test]
    fn substitutes_and_ignores_missing_when_lenient() {
        assert_eq!(
            render("Hi {{ name }}, {{count}} left", &vars()).unwrap(),
            "Hi Ada, 3 left"
        );
        // leading-dot (Go style) and missing var -> empty
        assert_eq!(
            render("{{ .name }}/{{ missing }}", &vars()).unwrap(),
            "Ada/"
        );
    }

    #[test]
    fn errors_on_unclosed() {
        assert!(render_value("a {{ b", &Value::Null).is_err());
    }

    #[test]
    fn strict_mode_rejects_undefined_variables() {
        let e = render_strict("Hi {{ nmae }}", &vars())
            .unwrap_err()
            .to_string();
        assert!(e.contains("undefined variable 'nmae'"), "{e}");
        // A defined null is not "undefined".
        let t = Template::parse("[{{ x }}]").unwrap();
        assert_eq!(t.render_value(&v(r#"{"x":null}"#)).unwrap(), "[]");
        // A path through a missing field is undefined.
        assert!(Template::parse("{{ a.b }}")
            .unwrap()
            .render_value(&v(r#"{"a":{}}"#))
            .is_err());
    }

    #[test]
    fn literal_braces_and_comments() {
        assert_eq!(
            render(
                r"use \{{ name }} to greet {{ name }}{{! internal note }}",
                &vars()
            )
            .unwrap(),
            "use {{ name }} to greet Ada"
        );
    }

    #[test]
    fn root_is_not_dumped_unless_allowed() {
        let vars = v(r#"{"secret":"s3cr3t"}"#);
        assert_eq!(render_value("[{{ . }}][{{ }}]", &vars).unwrap(), "[][]");
        assert!(Template::parse("{{ . }}")
            .unwrap()
            .render_value(&vars)
            .is_err());
        let allowed = RenderOptions::strict().allow_root(true);
        assert_eq!(
            Template::parse("{{ . }}")
                .unwrap()
                .render_with(&vars, &allowed)
                .unwrap(),
            r#"{"secret":"s3cr3t"}"#
        );
    }

    #[test]
    fn substituted_values_are_never_re_rendered() {
        let vars = v(r#"{"user":"{{ secret }}{{#each x}}","secret":"s3cr3t"}"#);
        assert_eq!(
            render_value("hello {{ user }}", &vars).unwrap(),
            "hello {{ secret }}{{#each x}}"
        );
    }

    #[test]
    fn conditionals() {
        let t =
            Template::parse("{{#if on}}yes{{else}}no{{/if}}|{{#unless on}}off{{/unless}}").unwrap();
        assert_eq!(t.render_value(&v(r#"{"on":true}"#)).unwrap(), "yes|");
        assert_eq!(t.render_value(&v(r#"{"on":0}"#)).unwrap(), "no|off");
        assert_eq!(t.render_value(&v(r#"{"on":[]}"#)).unwrap(), "no|off");
        // Presence test: an undefined variable is false even when strict.
        assert_eq!(t.render_value(&v("{}")).unwrap(), "no|off");
    }

    #[test]
    fn loops_with_scopes_and_metadata() {
        let t = Template::parse(
            "{{#each users}}{{#unless @first}}, {{/unless}}{{ @index }}:{{ name }}@{{ org }}{{#if @last}}.{{/if}}{{/each}}",
        )
        .unwrap();
        let vars = v(r#"{"org":"acme","users":[{"name":"a"},{"name":"b"}]}"#);
        assert_eq!(t.render_value(&vars).unwrap(), "0:a@acme, 1:b@acme.");

        let t = Template::parse("{{#each m}}{{ @key }}={{ this }};{{else}}none{{/each}}").unwrap();
        assert_eq!(
            t.render_value(&v(r#"{"m":{"x":1,"y":2}}"#)).unwrap(),
            "x=1;y=2;"
        );
        assert_eq!(t.render_value(&v(r#"{"m":[]}"#)).unwrap(), "none");
        assert_eq!(t.render_value(&v(r#"{"m":null}"#)).unwrap(), "none");
        // Strict: looping over nothing-at-all or a scalar is a mistake.
        assert!(t.render_value(&v("{}")).is_err());
        assert!(t.render_value(&v(r#"{"m":5}"#)).is_err());

        // Nested loops; `this.field` and array indexes.
        let t = Template::parse(
            "{{#each rows}}{{#each this.cells}}{{ . }}{{/each}}/{{/each}}{{ rows.1.cells.0 }}",
        )
        .unwrap();
        assert_eq!(
            t.render_value(&v(r#"{"rows":[{"cells":[1,2]},{"cells":[3]}]}"#))
                .unwrap(),
            "12/3/3"
        );
    }

    #[test]
    fn partials() {
        let mut partials = HashMap::new();
        partials.insert("sig".to_string(), "-- {{ name }}".to_string());
        partials.insert("loop".to_string(), "{{> loop }}".to_string());
        let opts = RenderOptions::strict().partials(&partials);
        let vars = v(r#"{"name":"Ada"}"#);
        let t = Template::parse("Bye\n{{> sig }}").unwrap();
        assert_eq!(t.render_with(&vars, &opts).unwrap(), "Bye\n-- Ada");
        // No source, unknown partial, and runaway recursion are all errors.
        assert!(t.render_value(&vars).is_err());
        assert!(Template::parse("{{> nope }}")
            .unwrap()
            .render_with(&vars, &opts)
            .is_err());
        let e = Template::parse("{{> loop }}")
            .unwrap()
            .render_with(&vars, &opts)
            .unwrap_err()
            .to_string();
        assert!(e.contains("nested deeper"), "{e}");
    }

    #[test]
    fn syntax_errors() {
        for (src, needle) in [
            ("{{#if x}}", "never closed"),
            ("{{/if}}", "no matching block"),
            ("{{#if x}}{{/each}}", "closed in order"),
            ("{{else}}", "outside of a block"),
            ("{{#if x}}{{else}}{{else}}{{/if}}", "more than one 'else'"),
            ("{{#when x}}{{/when}}", "unknown block"),
            ("{{#each}}{{/each}}", "needs an expression"),
            ("{{>}}", "needs a partial name"),
        ] {
            let e = Template::parse(src).unwrap_err().to_string();
            assert!(e.contains(needle), "{src:?} gave {e:?}");
        }
        let deep = "{{#if a}}".repeat(40);
        assert!(Template::parse(&deep)
            .unwrap_err()
            .to_string()
            .contains("nested deeper"));
    }

    #[test]
    fn output_is_capped() {
        let t = Template::parse("{{#each xs}}0123456789{{/each}}").unwrap();
        let vars = v(r#"{"xs":[1,2,3,4,5,6,7,8,9,10]}"#);
        let small = RenderOptions::strict().max_output_bytes(50);
        assert!(t.render_with(&vars, &small).is_err());
        assert_eq!(t.render_value(&vars).unwrap().len(), 100);
    }

    #[test]
    fn lists_referenced_variables() {
        let t = Template::parse(
            "{{ a.b }}{{#if c}}{{ d }}{{/if}}{{#each e}}{{ this }}{{ @index }}{{/each}}{{ a }}",
        )
        .unwrap();
        assert_eq!(t.variables(), vec!["a", "c", "d", "e"]);
    }
}
