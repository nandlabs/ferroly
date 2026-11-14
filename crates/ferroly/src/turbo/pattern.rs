//! Route patterns and the one segment matcher shared by routes, group scopes
//! and mounts.
//!
//! Everything that decides "does this request belong here" goes through
//! [`Pattern::match_full`] or [`match_prefix`], on the path split by
//! [`split_path`] — so a filter scope and a route can never disagree about
//! where a segment boundary is.

use super::error::RouteError;

/// One segment of a route pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seg {
    /// A literal segment, in the canonical (normalised) spelling.
    Static(String),
    /// `:name` — exactly one segment.
    Param(String),
    /// `*name` — the rest of the path (zero or more segments); last only.
    Wildcard(String),
}

impl Seg {
    /// Specificity rank: lower is more specific.
    fn rank(&self) -> u8 {
        match self {
            Seg::Static(_) => 0,
            Seg::Param(_) => 1,
            Seg::Wildcard(_) => 2,
        }
    }
}

/// A parsed route pattern.
#[derive(Debug, Clone)]
pub(crate) struct Pattern {
    pub(crate) segments: Vec<Seg>,
    /// Whether the registered pattern ended with `/` (used by strict-slash).
    pub(crate) trailing_slash: bool,
}

/// What a pattern captured from a request path. Values are still
/// percent-encoded, exactly as they appear in the normalised path.
#[derive(Debug, Default)]
pub(crate) struct Captures<'p> {
    pub(crate) params: Vec<(&'p str, &'p str)>,
    pub(crate) tail: Option<(&'p str, Vec<&'p str>)>,
}

/// Splits a normalised request path into its non-empty segments.
pub(crate) fn split_path(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// The canonical spelling of a literal pattern segment: the spelling a
/// normalised request path uses for the same text. Escapes of unreserved
/// characters are decoded, other escapes upper-cased, and bytes that cannot
/// appear raw in a request path (space, control and non-ASCII bytes, `?`, `#`
/// and a `%` that does not start an escape) are percent-encoded.
fn canonical_static(seg: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let unreserved = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
    let bytes = seg.as_bytes();
    let mut out = String::with_capacity(seg.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let escape = match (
            b,
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => Some(hi << 4 | lo),
            _ => None,
        };
        match escape {
            Some(value) if unreserved(value) => {
                out.push(value as char);
                i += 3;
            }
            Some(value) => {
                out.push('%');
                out.push(HEX[usize::from(value >> 4)] as char);
                out.push(HEX[usize::from(value & 0x0f)] as char);
                i += 3;
            }
            None => {
                if b <= 0x20 || b >= 0x7f || matches!(b, b'?' | b'#' | b'%') {
                    out.push('%');
                    out.push(HEX[usize::from(b >> 4)] as char);
                    out.push(HEX[usize::from(b & 0x0f)] as char);
                } else {
                    out.push(b as char);
                }
                i += 1;
            }
        }
    }
    out
}

impl Pattern {
    /// Parses a route pattern such as `/users/:id` or `/files/*path`.
    pub(crate) fn parse(path: &str) -> Result<Pattern, RouteError> {
        let invalid = |reason: &str| RouteError::InvalidPattern {
            pattern: path.to_string(),
            reason: reason.to_string(),
        };
        let raw: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut segments = Vec::with_capacity(raw.len());
        let mut names: Vec<&str> = Vec::new();
        for (i, seg) in raw.iter().enumerate() {
            if let Some(name) = seg.strip_prefix(':') {
                if name.is_empty() {
                    return Err(invalid("a `:` parameter needs a name"));
                }
                if names.contains(&name) {
                    return Err(invalid("a parameter name is used twice"));
                }
                names.push(name);
                segments.push(Seg::Param(name.to_string()));
            } else if let Some(name) = seg.strip_prefix('*') {
                if i + 1 != raw.len() {
                    return Err(invalid("a `*` catch-all must be the last segment"));
                }
                let name = if name.is_empty() { "*" } else { name };
                if names.contains(&name) {
                    return Err(invalid("a parameter name is used twice"));
                }
                names.push(name);
                segments.push(Seg::Wildcard(name.to_string()));
            } else if *seg == "." || *seg == ".." {
                return Err(invalid("`.` and `..` segments can never match a request"));
            } else {
                segments.push(Seg::Static(canonical_static(seg)));
            }
        }
        Ok(Pattern {
            segments,
            trailing_slash: path.len() > 1 && path.ends_with('/'),
        })
    }

    /// Parses a group or mount prefix: a pattern without a catch-all.
    pub(crate) fn parse_prefix(prefix: &str) -> Result<Vec<Seg>, RouteError> {
        let pattern = Pattern::parse(prefix)?;
        if pattern.has_wildcard() {
            return Err(RouteError::InvalidPattern {
                pattern: prefix.to_string(),
                reason: "a group or mount prefix cannot contain a `*` catch-all".to_string(),
            });
        }
        Ok(pattern.segments)
    }

    pub(crate) fn has_wildcard(&self) -> bool {
        matches!(self.segments.last(), Some(Seg::Wildcard(_)))
    }

    /// This pattern with `prefix` in front of it.
    pub(crate) fn prefixed(mut self, prefix: &[Seg]) -> Pattern {
        let mut segments = prefix.to_vec();
        segments.append(&mut self.segments);
        self.segments = segments;
        self
    }

    /// The route template, e.g. `/users/:id` — what metrics and spans are
    /// labelled with.
    pub(crate) fn template(&self) -> String {
        template_of(&self.segments)
    }

    /// Matches the whole path (already split into segments).
    pub(crate) fn match_full<'p>(&'p self, parts: &[&'p str]) -> Option<Captures<'p>> {
        let mut caps = Captures::default();
        let mut i = 0;
        for seg in &self.segments {
            match seg {
                Seg::Static(s) => {
                    if parts.get(i).copied() != Some(s.as_str()) {
                        return None;
                    }
                    i += 1;
                }
                Seg::Param(name) => {
                    let part = parts.get(i).copied()?;
                    caps.params.push((name.as_str(), part));
                    i += 1;
                }
                Seg::Wildcard(name) => {
                    caps.tail = Some((name.as_str(), parts[i.min(parts.len())..].to_vec()));
                    i = parts.len();
                }
            }
        }
        (i == parts.len()).then_some(caps)
    }

    /// Whether two patterns match exactly the same set of paths: the same
    /// literals in the same places, parameters and catch-alls regardless of
    /// their names.
    pub(crate) fn same_shape(&self, other: &Pattern) -> bool {
        self.segments.len() == other.segments.len()
            && self
                .segments
                .iter()
                .zip(&other.segments)
                .all(|(a, b)| match (a, b) {
                    (Seg::Static(x), Seg::Static(y)) => x == y,
                    (Seg::Param(_), Seg::Param(_)) | (Seg::Wildcard(_), Seg::Wildcard(_)) => true,
                    _ => false,
                })
    }

    /// The sort key that puts more specific patterns first: literal before
    /// parameter before catch-all, compared segment by segment.
    pub(crate) fn specificity(&self) -> Vec<u8> {
        self.segments.iter().map(Seg::rank).collect()
    }
}

/// The template text of a list of segments (`/` for none).
pub(crate) fn template_of(segments: &[Seg]) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }
    let mut out = String::new();
    for seg in segments {
        out.push('/');
        match seg {
            Seg::Static(s) => out.push_str(s),
            Seg::Param(n) => {
                out.push(':');
                out.push_str(n);
            }
            Seg::Wildcard(n) => {
                out.push('*');
                if n != "*" {
                    out.push_str(n);
                }
            }
        }
    }
    out
}

/// Whether the leading segments of a path match `prefix` — the scope test for
/// group filters. Uses the same literal and parameter rules as
/// [`Pattern::match_full`], on the same segments.
pub(crate) fn match_prefix(prefix: &[Seg], parts: &[&str]) -> bool {
    if parts.len() < prefix.len() {
        return false;
    }
    prefix.iter().zip(parts).all(|(seg, part)| match seg {
        Seg::Static(s) => s == part,
        Seg::Param(_) => true,
        Seg::Wildcard(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_and_captures() {
        let p = Pattern::parse("/items/:id/detail").unwrap();
        let caps = p.match_full(&split_path("/items/42/detail")).unwrap();
        assert_eq!(caps.params, vec![("id", "42")]);
        assert!(p.match_full(&split_path("/items/42")).is_none());
        assert!(p.match_full(&split_path("/items/42/other")).is_none());
    }

    #[test]
    fn catch_all_takes_the_rest() {
        let p = Pattern::parse("/files/*path").unwrap();
        let caps = p.match_full(&split_path("/files/a/b")).unwrap();
        assert_eq!(caps.tail, Some(("path", vec!["a", "b"])));
        let caps = p.match_full(&split_path("/files")).unwrap();
        assert_eq!(caps.tail, Some(("path", vec![])));
        assert!(p.match_full(&split_path("/other/a")).is_none());
    }

    #[test]
    fn rejects_bad_patterns() {
        assert!(Pattern::parse("/a/:").is_err());
        assert!(Pattern::parse("/a/:x/:x").is_err());
        assert!(Pattern::parse("/a/*rest/b").is_err());
        assert!(Pattern::parse("/a/../b").is_err());
        assert!(Pattern::parse_prefix("/a/*rest").is_err());
    }

    #[test]
    fn prefix_respects_segment_boundaries_and_params() {
        let api = Pattern::parse_prefix("/api").unwrap();
        assert!(match_prefix(&api, &split_path("/api/secret")));
        assert!(match_prefix(&api, &split_path("/api")));
        assert!(!match_prefix(&api, &split_path("/apix/secret")));
        assert!(!match_prefix(&api, &split_path("/API/secret")));

        let tenant = Pattern::parse_prefix("/t/:id").unwrap();
        assert!(match_prefix(&tenant, &split_path("/t/7/items")));
        assert!(!match_prefix(&tenant, &split_path("/t")));
    }

    #[test]
    fn literal_segments_use_the_normalised_spelling() {
        let p = Pattern::parse("/a b/%7euser/x%2fy").unwrap();
        assert_eq!(p.template(), "/a%20b/~user/x%2Fy");
    }

    #[test]
    fn specificity_orders_literal_first() {
        let lit = Pattern::parse("/items/new").unwrap().specificity();
        let param = Pattern::parse("/items/:id").unwrap().specificity();
        let all = Pattern::parse("/items/*rest").unwrap().specificity();
        let exact = Pattern::parse("/items").unwrap().specificity();
        assert!(lit < param && param < all && exact < all);
    }
}
