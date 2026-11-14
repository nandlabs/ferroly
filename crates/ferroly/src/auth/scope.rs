//! Scope and role checks over verified claims.

use ferroly::codec::Value;
use ferroly_derive::FerrolyError;

/// A set of granted scopes or roles, taken from verified token claims.
///
/// OAuth 2.0 carries scopes as one space-delimited string (`"scope": "read
/// write"`, RFC 8693 §4.2 / RFC 9068); some issuers use an array instead
/// (`"scp": ["read", "write"]`, `"roles": ["admin"]`). [`Scopes`] reads
/// either shape.
///
/// Matching is exact and case-sensitive: `orders:read` does not imply
/// `orders` and there are no wildcards.
///
/// ```
/// use ferroly::auth::Scopes;
/// use ferroly::codec::Value;
///
/// let claims = Value::Object(vec![
///     ("scope".into(), "orders:read orders:write".into()),
///     ("roles".into(), Value::Array(vec!["admin".into()])),
/// ]);
///
/// let scopes = Scopes::from_claims(&claims);
/// assert!(scopes.has_scope("orders:read"));
/// assert!(scopes.require_all(&["orders:read", "orders:write"]).is_ok());
/// assert!(scopes.require_any(&["billing:read", "orders:read"]).is_ok());
/// assert!(scopes.require_all(&["orders:delete"]).is_err());
///
/// let roles = Scopes::roles(&claims);
/// assert!(roles.has_scope("admin"));
/// ```
///
/// Only build a `Scopes` from claims returned by a successful verification;
/// it performs no signature check of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scopes {
    items: Vec<String>,
}

/// A scope or role requirement that the token does not meet.
#[derive(Debug, Clone, FerrolyError, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeError {
    /// [`Scopes::require_all`] failed; these required scopes are absent.
    #[error("missing required scope(s): {0}")]
    Missing(String),
    /// [`Scopes::require_any`] failed; none of the acceptable scopes is
    /// present.
    #[error("none of the accepted scopes is present: {0}")]
    NoneOf(String),
}

impl Scopes {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses a space-delimited scope string (`"read write"`). Repeated and
    /// surrounding whitespace is ignored; duplicates are dropped.
    pub fn parse(scope: &str) -> Self {
        let mut scopes = Scopes::new();
        for item in scope.split_ascii_whitespace() {
            scopes.insert(item);
        }
        scopes
    }

    /// Reads the standard scope claims: `scope` (space-delimited string or
    /// array) merged with `scp` (the same, as issued by Microsoft Entra ID
    /// and Okta). Missing or mistyped claims contribute nothing.
    pub fn from_claims(claims: &Value) -> Self {
        let mut scopes = Scopes::from_claim(claims, "scope");
        for item in Scopes::from_claim(claims, "scp").items {
            scopes.insert(&item);
        }
        scopes
    }

    /// Reads the `roles` claim (array of strings, or a space-delimited
    /// string).
    pub fn roles(claims: &Value) -> Self {
        Scopes::from_claim(claims, "roles")
    }

    /// Reads one claim that holds either a space-delimited string or an array
    /// of strings (non-string array elements are ignored). A missing claim or
    /// any other JSON type yields an empty set — absence of a grant is never
    /// an error, it is simply no grant.
    pub fn from_claim(claims: &Value, name: &str) -> Self {
        match claims.get(name) {
            Some(Value::Str(text)) => Scopes::parse(text),
            Some(Value::Array(items)) => {
                let mut scopes = Scopes::new();
                for item in items {
                    if let Value::Str(text) = item {
                        if !text.is_empty() {
                            scopes.insert(text);
                        }
                    }
                }
                scopes
            }
            _ => Scopes::new(),
        }
    }

    fn insert(&mut self, item: &str) {
        if !self.has_scope(item) {
            self.items.push(item.to_string());
        }
    }

    /// Whether `scope` was granted (exact, case-sensitive match).
    pub fn has_scope(&self, scope: &str) -> bool {
        self.items.iter().any(|item| item == scope)
    }

    /// Whether every one of `required` was granted (true for an empty list).
    pub fn has_all(&self, required: &[&str]) -> bool {
        required.iter().all(|scope| self.has_scope(scope))
    }

    /// Whether at least one of `accepted` was granted (false for an empty
    /// list).
    pub fn has_any(&self, accepted: &[&str]) -> bool {
        accepted.iter().any(|scope| self.has_scope(scope))
    }

    /// Succeeds only if every one of `required` was granted; the error names
    /// the missing ones.
    pub fn require_all(&self, required: &[&str]) -> Result<(), ScopeError> {
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|scope| !self.has_scope(scope))
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ScopeError::Missing(missing.join(" ")))
        }
    }

    /// Succeeds if at least one of `accepted` was granted. An empty
    /// `accepted` list always fails (nothing is acceptable).
    pub fn require_any(&self, accepted: &[&str]) -> Result<(), ScopeError> {
        if self.has_any(accepted) {
            Ok(())
        } else {
            Err(ScopeError::NoneOf(accepted.join(" ")))
        }
    }

    /// The granted scopes, in first-seen order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(String::as_str)
    }

    /// The number of distinct scopes.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no scope was granted.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl std::fmt::Display for Scopes {
    /// The space-delimited form, as it appears in a `scope` claim.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.items.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(members: Vec<(&str, Value)>) -> Value {
        Value::Object(
            members
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    #[test]
    fn parses_space_delimited_scopes() {
        let scopes = Scopes::parse("  read   write\tread admin ");
        assert_eq!(
            scopes.iter().collect::<Vec<_>>(),
            ["read", "write", "admin"]
        );
        assert_eq!(scopes.len(), 3);
        assert_eq!(scopes.to_string(), "read write admin");
        assert!(Scopes::parse("").is_empty());
        assert!(Scopes::parse("   ").is_empty());
    }

    #[test]
    fn reads_string_and_array_claims() {
        let c = claims(vec![
            ("scope", "a b".into()),
            ("scp", Value::Array(vec!["b".into(), "c".into()])),
            (
                "roles",
                Value::Array(vec!["admin".into(), Value::Int(7), "".into(), "ops".into()]),
            ),
        ]);
        let scopes = Scopes::from_claims(&c);
        assert_eq!(scopes.iter().collect::<Vec<_>>(), ["a", "b", "c"]);
        let roles = Scopes::roles(&c);
        assert_eq!(roles.iter().collect::<Vec<_>>(), ["admin", "ops"]);
        assert_eq!(Scopes::from_claim(&c, "scp").len(), 2);
    }

    #[test]
    fn missing_or_mistyped_claims_grant_nothing() {
        let c = claims(vec![("scope", Value::Int(1)), ("roles", Value::Bool(true))]);
        assert!(Scopes::from_claims(&c).is_empty());
        assert!(Scopes::roles(&c).is_empty());
        assert!(Scopes::from_claims(&Value::Null).is_empty());
        assert!(Scopes::from_claims(&claims(vec![])).is_empty());
    }

    #[test]
    fn matching_is_exact_and_case_sensitive() {
        let scopes = Scopes::parse("orders:read Admin");
        assert!(scopes.has_scope("orders:read"));
        assert!(!scopes.has_scope("orders"));
        assert!(!scopes.has_scope("orders:read:all"));
        assert!(!scopes.has_scope("admin"));
        assert!(!scopes.has_scope(""));
    }

    #[test]
    fn require_all_names_what_is_missing() {
        let scopes = Scopes::parse("a b");
        assert!(scopes.require_all(&[]).is_ok());
        assert!(scopes.require_all(&["a", "b"]).is_ok());
        assert!(scopes.has_all(&["a", "b"]));
        assert_eq!(
            scopes.require_all(&["a", "c", "d"]),
            Err(ScopeError::Missing("c d".into()))
        );
        assert!(!scopes.has_all(&["a", "c"]));
        assert_eq!(
            scopes.require_all(&["c"]).unwrap_err().to_string(),
            "missing required scope(s): c"
        );
    }

    #[test]
    fn require_any_needs_at_least_one() {
        let scopes = Scopes::parse("a b");
        assert!(scopes.require_any(&["x", "b"]).is_ok());
        assert!(scopes.has_any(&["x", "b"]));
        assert_eq!(
            scopes.require_any(&["x", "y"]),
            Err(ScopeError::NoneOf("x y".into()))
        );
        // Nothing acceptable means nothing passes.
        assert!(scopes.require_any(&[]).is_err());
        assert!(!scopes.has_any(&[]));
        assert!(Scopes::new().require_any(&["a"]).is_err());
    }
}
