//! `${NAME}` / `${NAME:-default}` placeholder expansion over a config tree,
//! with the `${env:NAME}` and `${file:/path}` forms.

use std::collections::{BTreeMap, BTreeSet};

use ferroly::codec::Value;

use super::{navigate, secret, ConfigError, Source};

/// Deepest chain of placeholders referring to other placeholders.
const MAX_DEPTH: usize = 32;
/// Largest value a single expansion may grow to.
const MAX_LEN: usize = 1024 * 1024;

/// Which layers may use the `${env:NAME}` and `${file:/path}` placeholders,
/// for [`ConfigBuilder::interpolate_with`](super::ConfigBuilder::interpolate_with).
///
/// These two forms read the process environment and the file system, so by
/// default they are expanded only in values that came from a config file, a
/// value layer (code), a `*_FILE` secret file or a secrets directory. In a
/// value set by an environment variable or a command-line argument they are
/// an [`ConfigError::Interpolation`] error, so whoever can set a variable or
/// pass an argument cannot read an arbitrary file (or variable) into the
/// configuration. Each method here opts one more kind of layer in.
///
/// Plain `${NAME}` placeholders (a config key, then an environment variable
/// of that name) are expanded in every layer, as before these forms existed.
///
/// ```
/// use ferroly::config::InterpolateOptions;
///
/// // A deployment that sets `APP_DB__URL='postgres://app:${file:/run/secrets/pw}@db/app'`.
/// let options = InterpolateOptions::new().trust_env();
/// # let _ = options;
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct InterpolateOptions {
    env: bool,
    args: bool,
}

impl InterpolateOptions {
    /// The default: `${env:…}` and `${file:…}` only in values from config
    /// files, value layers, secret files and secrets directories.
    pub fn new() -> Self {
        Self::default()
    }

    /// Also expands them in values set by environment variables
    /// ([`Source::Env`]).
    pub fn trust_env(mut self) -> Self {
        self.env = true;
        self
    }

    /// Also expands them in values set by command-line arguments
    /// ([`Source::Args`], [`Source::Cli`], including `cli` argument
    /// defaults).
    pub fn trust_args(mut self) -> Self {
        self.args = true;
        self
    }

    /// Whether a value from `source` may read the environment and files.
    fn trusts(&self, source: Option<&Source>) -> bool {
        match source {
            Some(Source::File(_) | Source::Value | Source::SecretFile { .. })
            | Some(Source::SecretDir(_)) => true,
            Some(Source::Env(_)) => self.env,
            Some(Source::Args | Source::Cli) => self.args,
            // Fail closed: a value whose layer is unknown is not trusted.
            None => false,
        }
    }
}

/// Returns a copy of `root` with every placeholder in every string value
/// expanded (keys are left alone), and the leaf paths — spelled as
/// [`Config::to_redacted_string`](super::Config::to_redacted_string) spells
/// them — whose value now embeds a secret: the content of a `${file:…}` or
/// `${env:…}` placeholder, a key that came from a secret file (`origins`),
/// or a key an earlier pass already found secret-derived (`derived`).
pub(super) fn interpolate(
    root: &Value,
    origins: &BTreeMap<String, Source>,
    derived: &BTreeSet<String>,
    options: &InterpolateOptions,
) -> Result<(Value, BTreeSet<String>), ConfigError> {
    let mut cx = Expander {
        root,
        origins,
        before: derived,
        options,
        origin: None,
        stack: Vec::new(),
        indices: Vec::new(),
        tainted: false,
        derived: BTreeSet::new(),
    };
    let value = cx.walk(root, &mut Vec::new(), None)?;
    Ok((value, cx.derived))
}

struct Expander<'a> {
    root: &'a Value,
    origins: &'a BTreeMap<String, Source>,
    /// Paths found secret-derived by earlier passes.
    before: &'a BTreeSet<String>,
    options: &'a InterpolateOptions,
    /// The layer of the text being expanded right now.
    origin: Option<&'a Source>,
    /// The config keys currently being expanded, outermost first.
    stack: Vec<String>,
    /// Parallel to the walked path: whether each segment is a list index.
    indices: Vec<bool>,
    /// Whether the value being expanded has pulled in a secret.
    tainted: bool,
    /// Leaf paths whose expansion pulled in a secret.
    derived: BTreeSet<String>,
}

fn fail(at: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Interpolation {
        key: at.to_string(),
        reason: reason.into(),
    }
}

impl<'a> Expander<'a> {
    /// `list` is the origin of the list `value` is in, if any: a list is one
    /// leaf of provenance, so its items have no origin of their own.
    fn walk(
        &mut self,
        value: &Value,
        path: &mut Vec<String>,
        list: Option<&'a Source>,
    ) -> Result<Value, ConfigError> {
        Ok(match value {
            Value::Str(s) if s.contains('$') => {
                let at = path.join(".");
                self.origin = list.or_else(|| self.origins.get(&at));
                self.stack.push(at.clone());
                self.tainted = false;
                let expanded = self.expand(s, &at);
                self.stack.pop();
                if self.tainted {
                    self.derived.insert(self.dump_path(path));
                }
                Value::Str(expanded?)
            }
            Value::Array(items) => {
                let list = list.or_else(|| self.origins.get(&path.join(".")));
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    path.push(i.to_string());
                    self.indices.push(true);
                    let v = self.walk(item, path, list);
                    self.indices.pop();
                    path.pop();
                    out.push(v?);
                }
                Value::Array(out)
            }
            Value::Object(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    path.push(k.clone());
                    self.indices.push(false);
                    let v = self.walk(v, path, list);
                    self.indices.pop();
                    path.pop();
                    out.push((k.clone(), v?));
                }
                Value::Object(out)
            }
            other => other.clone(),
        })
    }

    /// `path` spelled the way the redacted dump spells it: `a.b[0].c`.
    fn dump_path(&self, path: &[String]) -> String {
        let mut out = String::new();
        for (seg, index) in path.iter().zip(&self.indices) {
            if *index {
                out.push('[');
                out.push_str(seg);
                out.push(']');
            } else {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(seg);
            }
        }
        out
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
                    let reason = match name.strip_prefix("file:") {
                        Some(path) => format!("file '{path}' does not exist and has no default"),
                        None => format!("'{name}' is not set and has no default"),
                    };
                    return Err(fail(at, reason));
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

    /// Refuses `${env:…}` / `${file:…}` (`form`) in a value from a layer the
    /// options do not trust.
    fn check_trusted(&self, form: &str, at: &str) -> Result<(), ConfigError> {
        if self.options.trusts(self.origin) {
            return Ok(());
        }
        let from = self
            .origin
            .map_or_else(|| "an unknown layer".to_string(), |s| s.to_string());
        Err(fail(
            at,
            format!(
                "'${{{form}:…}}' is not expanded in a value from {from}; \
                 opt that layer in with InterpolateOptions"
            ),
        ))
    }

    /// Resolves a placeholder name: a config key at that dotted path first,
    /// then an environment variable of that exact name.
    ///
    /// A key that is already being expanded is not read again — that would
    /// never end — so a self-reference such as `PORT: "${PORT}"` falls through
    /// to the environment, and is a cycle error only if that is unset too.
    fn lookup(&mut self, name: &str, at: &str) -> Result<Option<String>, ConfigError> {
        if let Some(var) = name.strip_prefix("env:") {
            self.check_trusted("env", at)?;
            if var.is_empty() {
                return Err(fail(at, "empty variable name in '${env:}'"));
            }
            let found = env_var(var, at)?;
            if found.is_some() {
                self.tainted = true;
            }
            return Ok(found);
        }
        if let Some(path) = name.strip_prefix("file:") {
            self.check_trusted("file", at)?;
            if path.is_empty() {
                return Err(fail(at, "empty file path in '${file:}'"));
            }
            return match secret::read_secret_file(std::path::Path::new(path)) {
                Ok(text) => {
                    self.tainted = true;
                    Ok(Some(text))
                }
                Err(e) if e.kind() == Some(std::io::ErrorKind::NotFound) => Ok(None),
                Err(e) => Err(fail(at, format!("file '{path}': {e}"))),
            };
        }

        // The key as provenance and the dump spell it: `navigate` skips
        // empty segments, so `${db..pw}` and `${.db.pw}` name `db.pw`.
        let key = name
            .split('.')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(".");
        let root = self.root;
        let in_progress = self.stack.contains(&key);
        let configured = navigate(root, &key).filter(|v| !v.is_null());

        if let (Some(value), false) = (configured, in_progress) {
            let origin = self.origins.get(&key);
            if matches!(
                origin,
                Some(Source::SecretFile { .. } | Source::SecretDir(_))
            ) || self.before.contains(&key)
            {
                self.tainted = true;
            }
            return match value {
                Value::Str(s) if s.contains('$') => {
                    if self.stack.len() >= MAX_DEPTH {
                        return Err(fail(at, "placeholders are nested too deeply"));
                    }
                    // The referenced text is expanded as its own layer
                    // allows, not as the referring one does.
                    let outer = std::mem::replace(&mut self.origin, origin);
                    self.stack.push(key);
                    let expanded = self.expand(s, at);
                    self.stack.pop();
                    self.origin = outer;
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

        if let Some(found) = env_var(name, at)? {
            return Ok(Some(found));
        }
        if in_progress && configured.is_some() {
            let mut cycle = self.stack.clone();
            cycle.push(key);
            return Err(fail(at, format!("cycle: {}", cycle.join(" -> "))));
        }
        Ok(None)
    }
}

/// The environment variable `name`, or `None` if it is unset.
fn env_var(name: &str, at: &str) -> Result<Option<String>, ConfigError> {
    match std::env::var_os(name) {
        Some(raw) => raw
            .into_string()
            .map(Some)
            .map_err(|_| fail(at, format!("environment variable '{name}' is not UTF-8"))),
        None => Ok(None),
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

    #[test]
    fn only_file_value_and_secret_layers_are_trusted_by_default() {
        let o = InterpolateOptions::new();
        assert!(o.trusts(Some(&Source::File("a.yaml".into()))));
        assert!(o.trusts(Some(&Source::Value)));
        assert!(o.trusts(Some(&Source::SecretDir("/s/k".into()))));
        for s in [Source::Env("A".into()), Source::Args, Source::Cli] {
            assert!(!o.trusts(Some(&s)), "{s}");
        }
        assert!(!o.trusts(None));
        let o = InterpolateOptions::new().trust_env().trust_args();
        for s in [Source::Env("A".into()), Source::Args, Source::Cli] {
            assert!(o.trusts(Some(&s)), "{s}");
        }
    }
}
