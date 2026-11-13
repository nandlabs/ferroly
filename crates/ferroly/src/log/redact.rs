//! Field redaction: values of sensitive-looking field names are masked before
//! a record is formatted.

use std::borrow::Cow;

use ferroly::codec::Value;

/// The field-name patterns a default [`Redactor`] masks.
pub const DEFAULT_REDACTED: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "authorization",
    "api_key",
    "apikey",
    "cookie",
    "credential",
    "credentials",
    "private_key",
];

/// The default replacement for a redacted value.
pub const DEFAULT_MASK: &str = "***";

/// Decides which fields are sensitive and replaces their values with a mask.
///
/// A pattern is one or more words (`"token"`, `"api_key"`). A field name
/// matches when the pattern's words appear as **consecutive whole words** of
/// the name, ignoring case. Names are split into words at `_`, `-`, `.`, any
/// other non-alphanumeric character, and lower-to-upper camelCase boundaries:
///
/// | field name | pattern `token` | pattern `api_key` |
/// |---|---|---|
/// | `token`, `access_token`, `X-Auth-Token`, `authToken` | match | — |
/// | `api_key`, `x-api-key`, `apiKey` | — | match |
/// | `tokens_used`, `max_tokens` | no match (`tokens` is another word) | — |
///
/// Redaction applies to a record's context-provider fields, the logger's
/// carried fields and the per-call fields, in both output formats, and looks
/// inside nested objects and arrays. A matching field's whole value is
/// replaced. The message text is never inspected.
///
/// ```
/// use ferroly::log::Redactor;
///
/// let r = Redactor::default().pattern("ssn");
/// assert!(r.matches("user_password"));
/// assert!(r.matches("SSN"));
/// assert!(!r.matches("max_tokens"));
/// assert!(!Redactor::none().matches("password"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redactor {
    /// Each pattern as lowercase words.
    patterns: Vec<Vec<String>>,
    mask: String,
}

impl Default for Redactor {
    /// Masks the [`DEFAULT_REDACTED`] patterns with [`DEFAULT_MASK`].
    fn default() -> Self {
        Self::with_patterns(DEFAULT_REDACTED)
    }
}

/// Splits a field name (or pattern) into lowercase words.
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_numeric();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Redactor {
    /// The default redactor (same as [`Redactor::default`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// A redactor that masks nothing.
    pub fn none() -> Self {
        Redactor {
            patterns: Vec::new(),
            mask: DEFAULT_MASK.to_string(),
        }
    }

    /// A redactor that masks exactly `patterns` (not the defaults).
    pub fn with_patterns(patterns: &[&str]) -> Self {
        let mut r = Self::none();
        for p in patterns {
            r = r.pattern(p);
        }
        r
    }

    /// Adds one more pattern. A pattern without any letter or digit is ignored.
    pub fn pattern(mut self, pattern: &str) -> Self {
        let w = words(pattern);
        if !w.is_empty() && !self.patterns.contains(&w) {
            self.patterns.push(w);
        }
        self
    }

    /// Sets the text that replaces a redacted value (default `"***"`).
    pub fn mask(mut self, mask: impl Into<String>) -> Self {
        self.mask = mask.into();
        self
    }

    /// True when a field called `name` would be redacted.
    pub fn matches(&self, name: &str) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        // Cheap pre-filter: most names share no letters with any pattern, and
        // then no word split (and no allocation) is needed.
        let lower_hit = self.patterns.iter().any(|p| {
            p.first()
                .is_some_and(|first| contains_ignore_ascii_case(name, first))
        });
        let has_non_ascii = !name.is_ascii();
        if !lower_hit && !has_non_ascii {
            return false;
        }
        let name_words = words(name);
        self.patterns.iter().any(|p| {
            name_words
                .windows(p.len())
                .any(|window| window == p.as_slice())
        })
    }

    /// The value to print for field `name`: the mask when the name matches,
    /// otherwise the value with any sensitive nested fields masked.
    pub(super) fn apply<'a>(&self, name: &str, value: &'a Value) -> Cow<'a, Value> {
        if self.patterns.is_empty() {
            return Cow::Borrowed(value);
        }
        if self.matches(name) {
            return Cow::Owned(Value::Str(self.mask.clone()));
        }
        if self.has_sensitive_child(value) {
            Cow::Owned(self.deep(value))
        } else {
            Cow::Borrowed(value)
        }
    }

    fn has_sensitive_child(&self, value: &Value) -> bool {
        match value {
            Value::Object(entries) => entries
                .iter()
                .any(|(k, v)| self.matches(k) || self.has_sensitive_child(v)),
            Value::Array(items) => items.iter().any(|v| self.has_sensitive_child(v)),
            _ => false,
        }
    }

    fn deep(&self, value: &Value) -> Value {
        match value {
            Value::Object(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(k, v)| {
                        let v = if self.matches(k) {
                            Value::Str(self.mask.clone())
                        } else {
                            self.deep(v)
                        };
                        (k.clone(), v)
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.deep(v)).collect()),
            other => other.clone(),
        }
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_splitting() {
        assert_eq!(words("access_token"), ["access", "token"]);
        assert_eq!(words("X-Auth-Token"), ["x", "auth", "token"]);
        assert_eq!(words("authToken"), ["auth", "token"]);
        assert_eq!(words("APIKey"), ["apikey"]);
        assert_eq!(words("db.password"), ["db", "password"]);
        assert_eq!(words("__"), Vec::<String>::new());
    }

    #[test]
    fn default_patterns_match_whole_words_only() {
        let r = Redactor::default();
        for hit in [
            "password",
            "PASSWORD",
            "user_password",
            "db.passwd",
            "client_secret",
            "access_token",
            "X-Auth-Token",
            "refreshToken",
            "Authorization",
            "proxy-authorization",
            "api_key",
            "x-api-key",
            "apiKey",
            "APIKEY",
            "cookie",
            "Set-Cookie",
            "aws_credentials",
            "private_key",
        ] {
            assert!(r.matches(hit), "{hit} should be redacted");
        }
        for miss in [
            "tokens_used",
            "max_tokens",
            "prompt_tokens",
            "path",
            "status",
            "secretary",
            "key",
            "passport",
            "trace_id",
        ] {
            assert!(!r.matches(miss), "{miss} should not be redacted");
        }
    }

    #[test]
    fn nested_values_are_masked_in_place() {
        let r = Redactor::default().mask("[hidden]");
        let v = Value::Object(vec![
            ("user".into(), Value::Str("ada".into())),
            ("password".into(), Value::Str("hunter2".into())),
            (
                "headers".into(),
                Value::Array(vec![Value::Object(vec![(
                    "Authorization".into(),
                    Value::Str("Bearer abc".into()),
                )])]),
            ),
        ]);
        let out = r.apply("request", &v).into_owned();
        assert_eq!(out.get("user"), Some(&Value::Str("ada".into())));
        assert_eq!(out.get("password"), Some(&Value::Str("[hidden]".into())));
        let header = out.get("headers").unwrap().get_index(0).unwrap();
        assert_eq!(
            header.get("Authorization"),
            Some(&Value::Str("[hidden]".into()))
        );
        // Untouched values are borrowed, not cloned.
        let clean = Value::Object(vec![("a".into(), Value::Int(1))]);
        assert!(matches!(r.apply("clean", &clean), Cow::Borrowed(_)));
    }
}
