//! Format-string handling for `#[error("...")]`.
//!
//! Placeholders that name a field (`{0}`, `{path}`, `{code:?}`, `{0:>width$}`)
//! are rewritten to the local the field is pattern-bound to, so the generated
//! `write!` captures it. Everything else — literal text, `{{` / `}}` escapes,
//! and placeholders naming something that is not a field — is passed through
//! untouched for `write!` to interpret.

/// A rewritten format string plus the fields it formats.
#[derive(Debug, PartialEq, Eq)]
pub struct Rewritten {
    pub text: String,
    /// `(field index, core::fmt trait name)` for each field placeholder, in
    /// order of appearance; used to infer bounds on generic error types.
    pub uses: Vec<(usize, &'static str)>,
    /// Indices of fields used as a `name$` width or precision argument. These
    /// must be passed to `write!` by value (`usize`), not as the bound reference.
    pub counts: Vec<usize>,
}

/// Rewrites `fmt`. `lookup` maps a placeholder name (`"0"`, `"path"`) to the
/// field's index and the name of the local it is bound to.
pub fn rewrite(fmt: &str, lookup: &dyn Fn(&str) -> Option<(usize, String)>) -> Rewritten {
    let mut text = String::with_capacity(fmt.len() + 16);
    let mut uses = Vec::new();
    let mut counts = Vec::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push_str("{{");
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push_str("}}");
            }
            '{' => {
                let mut inner = String::new();
                let mut closed = false;
                for d in chars.by_ref() {
                    if d == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(d);
                }
                text.push('{');
                if !closed {
                    // Malformed; leave it for `write!` to report.
                    text.push_str(&inner);
                    break;
                }
                let (arg, spec) = match inner.find(':') {
                    Some(i) => (&inner[..i], Some(&inner[i + 1..])),
                    None => (inner.as_str(), None),
                };
                match lookup(arg.trim()) {
                    Some((index, local)) => {
                        uses.push((index, fmt_trait(spec.unwrap_or(""))));
                        text.push_str(&local);
                    }
                    None => text.push_str(arg),
                }
                if let Some(spec) = spec {
                    text.push(':');
                    text.push_str(&rewrite_spec(spec, lookup, &mut counts));
                }
                text.push('}');
            }
            _ => text.push(c),
        }
    }
    Rewritten { text, uses, counts }
}

/// Rewrites `name$` width/precision arguments that refer to a field.
fn rewrite_spec(
    spec: &str,
    lookup: &dyn Fn(&str) -> Option<(usize, String)>,
    counts: &mut Vec<usize>,
) -> String {
    let mut out = String::with_capacity(spec.len());
    let mut run = String::new();
    for c in spec.chars() {
        if c.is_alphanumeric() || c == '_' {
            run.push(c);
            continue;
        }
        match lookup(&run) {
            Some((index, local)) if c == '$' => {
                if !counts.contains(&index) {
                    counts.push(index);
                }
                out.push_str(&local);
            }
            _ => out.push_str(&run),
        }
        run.clear();
        out.push(c);
    }
    out.push_str(&run);
    out
}

/// The `core::fmt` trait a format spec (the text after `:`) selects.
pub fn fmt_trait(spec: &str) -> &'static str {
    match spec.chars().last() {
        Some('?') => "Debug",
        Some('x') => "LowerHex",
        Some('X') => "UpperHex",
        Some('o') => "Octal",
        Some('b') => "Binary",
        Some('e') => "LowerExp",
        Some('E') => "UpperExp",
        Some('p') => "Pointer",
        _ => "Display",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tuple-style lookup over `n` fields: `"0"` → `__0`, …
    fn positional(n: usize) -> impl Fn(&str) -> Option<(usize, String)> {
        move |name| {
            let i: usize = name.parse().ok()?;
            (i < n && name == i.to_string()).then(|| (i, format!("__{i}")))
        }
    }

    /// Named-style lookup: `"path"` → `__field_path`.
    fn named(fields: &'static [&'static str]) -> impl Fn(&str) -> Option<(usize, String)> {
        move |name| {
            fields
                .iter()
                .position(|f| *f == name)
                .map(|i| (i, format!("__field_{name}")))
        }
    }

    #[test]
    fn positional_placeholders() {
        let r = rewrite("code {0}: {1}", &positional(2));
        assert_eq!(r.text, "code {__0}: {__1}");
        assert_eq!(r.uses, [(0, "Display"), (1, "Display")]);
    }

    #[test]
    fn named_placeholders() {
        let r = rewrite("bad '{field}': {reason}", &named(&["field", "reason"]));
        assert_eq!(r.text, "bad '{__field_field}': {__field_reason}");
        assert_eq!(r.uses, [(0, "Display"), (1, "Display")]);
    }

    #[test]
    fn format_specs_are_preserved() {
        let r = rewrite("{0:?} {0:#?} {1:>8.3} {1:#x}", &positional(2));
        assert_eq!(r.text, "{__0:?} {__0:#?} {__1:>8.3} {__1:#x}");
        assert_eq!(
            r.uses,
            [(0, "Debug"), (0, "Debug"), (1, "Display"), (1, "LowerHex")]
        );
        let r = rewrite("{path:?}", &named(&["path"]));
        assert_eq!(r.text, "{__field_path:?}");
        assert_eq!(r.uses, [(0, "Debug")]);
    }

    #[test]
    fn escaped_braces_are_left_alone() {
        let r = rewrite("{{0}} {{{0}}} }}{{", &positional(1));
        assert_eq!(r.text, "{{0}} {{{__0}}} }}{{");
        assert_eq!(r.uses, [(0, "Display")]);

        let r = rewrite("{{field}}", &named(&["field"]));
        assert_eq!(r.text, "{{field}}");
        assert!(r.uses.is_empty());
    }

    #[test]
    fn non_field_placeholders_pass_through() {
        // Out-of-range index, unknown name, and the implicit `{}` are not ours.
        let r = rewrite("{2} {other} {} {:?}", &positional(2));
        assert_eq!(r.text, "{2} {other} {} {:?}");
        assert!(r.uses.is_empty());
        // A named placeholder in a tuple shape, and vice versa.
        assert_eq!(rewrite("{name}", &positional(1)).text, "{name}");
        assert_eq!(rewrite("{0}", &named(&["name"])).text, "{0}");
    }

    #[test]
    fn width_and_precision_arguments() {
        let r = rewrite("{0:>1$} {0:.1$} {0:1$.2$}", &positional(3));
        assert_eq!(r.text, "{__0:>__1$} {__0:.__1$} {__0:__1$.__2$}");
        assert_eq!(r.counts, [1, 2]);
        let r = rewrite("{value:width$}", &named(&["value", "width"]));
        assert_eq!(r.text, "{__field_value:__field_width$}");
        assert_eq!(r.uses, [(0, "Display")]);
        assert_eq!(r.counts, [1]);
        // A plain number in the spec is a literal width, not an argument.
        let r = rewrite("{0:1}", &positional(2));
        assert_eq!(r.text, "{__0:1}");
        assert!(r.counts.is_empty());
        // `x` here is the hex flag, not the field `x`.
        assert_eq!(rewrite("{x:x}", &named(&["x"])).text, "{__field_x:x}");
    }

    #[test]
    fn malformed_strings_are_passed_through_for_rustc() {
        assert_eq!(rewrite("open {0", &positional(1)).text, "open {0");
        assert_eq!(
            rewrite("stray } brace", &positional(1)).text,
            "stray } brace"
        );
        assert_eq!(rewrite("", &positional(1)).text, "");
    }

    #[test]
    fn whitespace_and_unicode() {
        let r = rewrite("héllo {0 } wörld", &positional(1));
        assert_eq!(r.text, "héllo {__0} wörld");
        let r = rewrite("{0:é>4}", &positional(1));
        assert_eq!(r.text, "{__0:é>4}");
    }

    #[test]
    fn trait_selection() {
        let cases = [
            ("", "Display"),
            (">8", "Display"),
            ("?", "Debug"),
            ("#?", "Debug"),
            ("x?", "Debug"),
            ("x", "LowerHex"),
            ("#X", "UpperHex"),
            ("o", "Octal"),
            ("#b", "Binary"),
            ("e", "LowerExp"),
            ("E", "UpperExp"),
            ("p", "Pointer"),
            ("width$", "Display"),
        ];
        for (spec, want) in cases {
            assert_eq!(fmt_trait(spec), want, "{spec:?}");
        }
    }
}
