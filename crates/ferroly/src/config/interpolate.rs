//! `${NAME}` / `${NAME:-default}` placeholder expansion over a config tree.

use ferroly::codec::Value;

use super::{navigate, ConfigError};

/// Deepest chain of placeholders referring to other placeholders.
const MAX_DEPTH: usize = 32;
/// Largest value a single expansion may grow to.
const MAX_LEN: usize = 1024 * 1024;

/// Returns a copy of `root` with every placeholder in every string value
/// expanded. Keys are left alone.
pub(super) fn interpolate(root: &Value) -> Result<Value, ConfigError> {
    let mut cx = Expander {
        root,
        stack: Vec::new(),
    };
    cx.walk(root, &mut Vec::new())
}

struct Expander<'a> {
    root: &'a Value,
    /// The config keys currently being expanded, outermost first.
    stack: Vec<String>,
}

fn fail(at: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Interpolation {
        key: at.to_string(),
        reason: reason.into(),
    }
}

impl Expander<'_> {
    fn walk(&mut self, value: &Value, path: &mut Vec<String>) -> Result<Value, ConfigError> {
        Ok(match value {
            Value::Str(s) if s.contains('$') => {
                let at = path.join(".");
                self.stack.push(at.clone());
                let expanded = self.expand(s, &at);
                self.stack.pop();
                Value::Str(expanded?)
            }
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    path.push(i.to_string());
                    let v = self.walk(item, path);
                    path.pop();
                    out.push(v?);
                }
                Value::Array(out)
            }
            Value::Object(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    path.push(k.clone());
                    let v = self.walk(v, path);
                    path.pop();
                    out.push((k.clone(), v?));
                }
                Value::Object(out)
            }
            other => other.clone(),
        })
    }

    /// Expands the placeholders in `text`; `at` is the key being built, for
    /// error messages.
    fn expand(&mut self, text: &str, at: &str) -> Result<String, ConfigError> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(idx) = rest.find('$') {
            out.push_str(&rest[..idx]);
            let tail = &rest[idx..];
            if tail.starts_with("$${") {
                // `$${` is an escaped, literal `${`.
                out.push('$');
                rest = &tail[2..];
                continue;
            }
            if !tail.starts_with("${") {
                out.push('$');
                rest = &tail[1..];
                continue;
            }
            let body = &tail[2..];
            let end = closing_brace(body).ok_or_else(|| fail(at, "unterminated '${'"))?;
            let inner = &body[..end];
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n.trim(), Some(d)),
                None => (inner.trim(), None),
            };
            if name.is_empty() {
                return Err(fail(at, "empty placeholder '${}'"));
            }
            // Like the shell's `:-`, the default covers unset and empty alike.
            match (self.lookup(name, at)?, default) {
                (Some(v), _) if !v.is_empty() => out.push_str(&v),
                (_, Some(d)) => {
                    let d = self.expand(d, at)?;
                    out.push_str(&d);
                }
                (Some(_), None) => {}
                (None, None) => {
                    return Err(fail(at, format!("'{name}' is not set and has no default")));
                }
            }
            if out.len() > MAX_LEN {
                return Err(fail(at, "expanded value is too large"));
            }
            rest = &body[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Resolves a placeholder name: a config key at that dotted path first,
    /// then an environment variable of that exact name.
    ///
    /// A key that is already being expanded is not read again — that would
    /// never end — so a self-reference such as `PORT: "${PORT}"` falls through
    /// to the environment, and is a cycle error only if that is unset too.
    fn lookup(&mut self, name: &str, at: &str) -> Result<Option<String>, ConfigError> {
        let root = self.root;
        let in_progress = self.stack.iter().any(|k| k == name);
        let configured = navigate(root, name).filter(|v| !v.is_null());

        if let (Some(value), false) = (configured, in_progress) {
            return match value {
                Value::Str(s) if s.contains('$') => {
                    if self.stack.len() >= MAX_DEPTH {
                        return Err(fail(at, "placeholders are nested too deeply"));
                    }
                    self.stack.push(name.to_string());
                    let expanded = self.expand(s, at);
                    self.stack.pop();
                    expanded.map(Some)
                }
                Value::Str(s) => Ok(Some(s.clone())),
                Value::Bool(b) => Ok(Some(b.to_string())),
                Value::Int(i) => Ok(Some(i.to_string())),
                Value::UInt(u) => Ok(Some(u.to_string())),
                Value::Float(f) => Ok(Some(f.to_string())),
                _ => Err(fail(at, format!("'{name}' is not a scalar value"))),
            };
        }

        if let Some(raw) = std::env::var_os(name) {
            return raw
                .into_string()
                .map(Some)
                .map_err(|_| fail(at, format!("environment variable '{name}' is not UTF-8")));
        }
        if in_progress && configured.is_some() {
            let mut cycle = self.stack.clone();
            cycle.push(name.to_string());
            return Err(fail(at, format!("cycle: {}", cycle.join(" -> "))));
        }
        Ok(None)
    }
}

/// Byte index of the `}` closing a placeholder whose body starts at `body`,
/// accounting for placeholders nested in the default.
fn closing_brace(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut depth = 1usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                depth += 1;
                i += 2;
                continue;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_brace_skips_nested_placeholders() {
        assert_eq!(closing_brace("A}"), Some(1));
        assert_eq!(closing_brace("A:-${B:-x}}tail"), Some(10));
        assert_eq!(closing_brace("A:-${B"), None);
    }
}
