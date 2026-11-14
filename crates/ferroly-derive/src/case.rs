//! `rename_all` case conversions.
//!
//! An identifier is first split into lowercase words, then the words are
//! re-joined in the requested style. Word boundaries are:
//!
//! - `_` and `-` separators;
//! - a lowercase letter or digit followed by an uppercase letter (`maxConns`
//!   → `max`, `conns`; `Sha256Hash` → `sha256`, `hash`);
//! - the last capital of an acronym run that is followed by a lowercase letter
//!   (`HTTPServer` → `http`, `server`).

/// A case rule accepted by `#[ferroly(rename_all = "...")]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameRule {
    /// `lowercase` — the identifier lowercased as-is (separators kept).
    Lower,
    /// `UPPERCASE` — the identifier uppercased as-is (separators kept).
    Upper,
    /// `snake_case`.
    Snake,
    /// `SCREAMING_SNAKE_CASE`.
    ScreamingSnake,
    /// `kebab-case`.
    Kebab,
    /// `SCREAMING-KEBAB-CASE`.
    ScreamingKebab,
    /// `camelCase`.
    Camel,
    /// `PascalCase`.
    Pascal,
}

/// Every spelling [`RenameRule::parse`] accepts, for error messages.
pub const RULES: &[&str] = &[
    "lowercase",
    "UPPERCASE",
    "snake_case",
    "SCREAMING_SNAKE_CASE",
    "kebab-case",
    "SCREAMING-KEBAB-CASE",
    "camelCase",
    "PascalCase",
];

impl RenameRule {
    /// Parses the string form used in the attribute; `None` if unknown.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "lowercase" => Self::Lower,
            "UPPERCASE" => Self::Upper,
            "snake_case" => Self::Snake,
            "SCREAMING_SNAKE_CASE" => Self::ScreamingSnake,
            "kebab-case" => Self::Kebab,
            "SCREAMING-KEBAB-CASE" => Self::ScreamingKebab,
            "camelCase" => Self::Camel,
            "PascalCase" => Self::Pascal,
            _ => return None,
        })
    }

    /// Applies the rule to an identifier name (already stripped of `r#`).
    pub fn apply(self, name: &str) -> String {
        match self {
            Self::Lower => name.to_lowercase(),
            Self::Upper => name.to_uppercase(),
            Self::Snake => format!("{}{}", leading_underscores(name), join(name, "_")),
            Self::ScreamingSnake => format!(
                "{}{}",
                leading_underscores(name),
                join(name, "_").to_uppercase()
            ),
            Self::Kebab => join(name, "-"),
            Self::ScreamingKebab => join(name, "-").to_uppercase(),
            Self::Camel => capitalized(name, false),
            Self::Pascal => capitalized(name, true),
        }
    }
}

/// Applies an optional rule; no rule leaves the name untouched.
pub fn apply_rename_all(name: &str, rule: Option<RenameRule>) -> String {
    match rule {
        Some(rule) => rule.apply(name),
        None => name.to_string(),
    }
}

/// Splits an identifier into lowercase words (see the module docs).
pub fn split_words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary =
                prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_is_lower);
            if boundary {
                words.push(std::mem::take(&mut cur));
            }
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn join(name: &str, sep: &str) -> String {
    split_words(name).join(sep)
}

/// The run of `_` an identifier starts with (`_id` keeps its prefix in the
/// snake-style rules, which are otherwise the identity on field names).
fn leading_underscores(name: &str) -> &str {
    let end = name.len() - name.trim_start_matches('_').len();
    &name[..end]
}

fn capitalized(name: &str, pascal: bool) -> String {
    let mut out = String::new();
    for (i, word) in split_words(name).iter().enumerate() {
        let mut chars = word.chars();
        match chars.next() {
            Some(first) if pascal || i > 0 => {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
            _ => out.push_str(word),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(name: &str) -> Vec<String> {
        split_words(name)
    }

    #[test]
    fn splits_snake_and_camel_identifiers() {
        assert_eq!(words("max_conns"), ["max", "conns"]);
        assert_eq!(words("maxConns"), ["max", "conns"]);
        assert_eq!(words("OnHold"), ["on", "hold"]);
        assert_eq!(words("MAX_RETRIES"), ["max", "retries"]);
        assert_eq!(words("kebab-name"), ["kebab", "name"]);
        assert_eq!(words("x"), ["x"]);
        assert!(words("").is_empty());
        assert!(words("__").is_empty());
    }

    #[test]
    fn splits_acronyms_at_the_last_capital() {
        assert_eq!(words("HTTPServer"), ["http", "server"]);
        assert_eq!(words("XMLHttpRequest"), ["xml", "http", "request"]);
        assert_eq!(words("parseHTTP"), ["parse", "http"]);
        assert_eq!(words("userID"), ["user", "id"]);
        assert_eq!(words("IOError"), ["io", "error"]);
        assert_eq!(words("ID"), ["id"]);
    }

    #[test]
    fn digits_stay_with_the_preceding_word() {
        assert_eq!(words("Sha256Hash"), ["sha256", "hash"]);
        assert_eq!(words("field_1"), ["field", "1"]);
        assert_eq!(words("V2"), ["v2"]);
        assert_eq!(words("utf8mb4"), ["utf8mb4"]);
    }

    #[test]
    fn parse_accepts_every_documented_rule_and_nothing_else() {
        for rule in RULES {
            assert!(RenameRule::parse(rule).is_some(), "{rule}");
        }
        assert_eq!(RenameRule::parse("snake_case"), Some(RenameRule::Snake));
        assert_eq!(RenameRule::parse("snakecase"), None);
        assert_eq!(RenameRule::parse("Snake_Case"), None);
        assert_eq!(RenameRule::parse(""), None);
    }

    /// One row per input: the expected output of each rule, in `RULES` order.
    #[test]
    fn every_rule_on_representative_identifiers() {
        let table: &[(&str, [&str; 8])] = &[
            (
                "max_conns",
                [
                    "max_conns",
                    "MAX_CONNS",
                    "max_conns",
                    "MAX_CONNS",
                    "max-conns",
                    "MAX-CONNS",
                    "maxConns",
                    "MaxConns",
                ],
            ),
            (
                "OnHold",
                [
                    "onhold", "ONHOLD", "on_hold", "ON_HOLD", "on-hold", "ON-HOLD", "onHold",
                    "OnHold",
                ],
            ),
            (
                "HTTPServer",
                [
                    "httpserver",
                    "HTTPSERVER",
                    "http_server",
                    "HTTP_SERVER",
                    "http-server",
                    "HTTP-SERVER",
                    "httpServer",
                    "HttpServer",
                ],
            ),
            (
                "userID",
                [
                    "userid", "USERID", "user_id", "USER_ID", "user-id", "USER-ID", "userId",
                    "UserId",
                ],
            ),
            (
                "Sha256Hash",
                [
                    "sha256hash",
                    "SHA256HASH",
                    "sha256_hash",
                    "SHA256_HASH",
                    "sha256-hash",
                    "SHA256-HASH",
                    "sha256Hash",
                    "Sha256Hash",
                ],
            ),
            ("id", ["id", "ID", "id", "ID", "id", "ID", "id", "Id"]),
        ];
        for (input, expected) in table {
            for (rule, want) in RULES.iter().zip(expected) {
                let got = RenameRule::parse(rule).unwrap().apply(input);
                assert_eq!(&got, want, "{rule} on {input}");
            }
        }
    }

    #[test]
    fn snake_rules_are_the_identity_on_snake_field_names() {
        for name in ["a", "max_conns", "_id", "__private", "field_1", "utf8"] {
            assert_eq!(RenameRule::Snake.apply(name), name);
        }
        assert_eq!(RenameRule::ScreamingSnake.apply("_id"), "_ID");
    }

    #[test]
    fn camel_rules_drop_separators() {
        assert_eq!(RenameRule::Camel.apply("_id"), "id");
        assert_eq!(RenameRule::Camel.apply("field_1"), "field1");
        assert_eq!(RenameRule::Pascal.apply("a__b"), "AB");
        assert_eq!(RenameRule::Camel.apply("SCREAMING_NAME"), "screamingName");
    }

    #[test]
    fn no_rule_is_the_identity() {
        assert_eq!(apply_rename_all("HTTPServer", None), "HTTPServer");
        assert_eq!(
            apply_rename_all("HTTPServer", Some(RenameRule::Snake)),
            "http_server"
        );
    }
}
