//! `rename_all` case conversions.

/// Applies a `rename_all` rule to an identifier name.
pub fn apply_rename_all(name: &str, rule: Option<&str>) -> String {
    match rule {
        Some("lowercase") => name.to_lowercase(),
        Some("UPPERCASE") => name.to_uppercase(),
        Some("snake_case") => to_snake(name),
        Some("SCREAMING_SNAKE_CASE") => to_snake(name).to_uppercase(),
        Some("kebab-case") => to_snake(name).replace('_', "-"),
        Some("camelCase") => to_camel(name, false),
        Some("PascalCase") => to_camel(name, true),
        _ => name.to_string(),
    }
}

fn to_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn to_camel(s: &str, pascal: bool) -> String {
    let mut out = String::new();
    let mut upper = pascal;
    for part in s.split('_') {
        if part.is_empty() {
            continue;
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            if upper {
                out.extend(first.to_uppercase());
            } else {
                out.extend(first.to_lowercase());
            }
            out.push_str(chars.as_str());
        }
        upper = true;
    }
    out
}
