//! Rust naming for generated items.

/// Words that need `r#` (or, for the four that cannot be raw, a trailing `_`).
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false",
    "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where",
    "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv", "try",
    "typeof", "unsized", "virtual", "yield",
];

/// A word that is a valid Rust identifier once written as is, raw, or with a
/// trailing underscore.
pub fn ident(word: &str) -> String {
    match word {
        "self" | "super" | "crate" | "Self" | "_" => format!("{word}_"),
        w if KEYWORDS.contains(&w) => format!("r#{w}"),
        w => w.to_owned(),
    }
}

/// Splits `FooBar`, `fooBar`, `foo_bar`, `FOO_BAR` and `HTTPServer` into
/// lowercase words.
fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '.' || c == '-' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            // `fooBar` → foo|Bar; `HTTPServer` → HTTP|Server; `Foo2Bar` → Foo2|Bar.
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `snake_case`.
pub fn snake(name: &str) -> String {
    let w = words(name);
    if w.is_empty() {
        return "_".to_owned();
    }
    w.join("_")
}

/// `PascalCase`.
pub fn pascal(name: &str) -> String {
    let mut out = String::new();
    for w in words(name) {
        let mut cs = w.chars();
        if let Some(first) = cs.next() {
            out.extend(first.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// `SCREAMING_SNAKE_CASE`.
pub fn screaming(name: &str) -> String {
    snake(name).to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_conversions() {
        assert_eq!(snake("FooBar"), "foo_bar");
        assert_eq!(snake("fooBar"), "foo_bar");
        assert_eq!(snake("HTTPServer"), "http_server");
        assert_eq!(snake("foo_bar"), "foo_bar");
        assert_eq!(snake("Foo2Bar"), "foo2_bar");
        assert_eq!(pascal("foo_bar"), "FooBar");
        assert_eq!(pascal("CORPUS_WEB"), "CorpusWeb");
        assert_eq!(pascal("HelloRequest"), "HelloRequest");
        assert_eq!(screaming("SayHello"), "SAY_HELLO");
        assert_eq!(ident("type"), "r#type");
        assert_eq!(ident("self"), "self_");
        assert_eq!(ident("name"), "name");
    }
}
