//! A tiny in-house template engine for prompt substitution.
//!
//! Supports `{{ path }}` variable interpolation with dot-path lookups into the
//! encoded variables (`{{ user.name }}`), and tolerates the Go
//! `text/template` leading-dot form (`{{ .name }}`). Undefined variables render
//! as the empty string. This deliberately covers only substitution — no
//! conditionals or loops — matching how prompt templates are actually used.

use ferroly::codec::{Encode, Value};

use ferroly::genai::GenAiError;

/// Renders `template`, substituting `{{ path }}` from the encoded `vars`.
pub fn render<T: Encode>(template: &str, vars: &T) -> Result<String, GenAiError> {
    render_value(template, &vars.encode())
}

/// Renders `template` against an already-built [`Value`].
pub fn render_value(template: &str, vars: &Value) -> Result<String, GenAiError> {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' && chars.get(i + 1) == Some(&'{') {
            let mut j = i + 2;
            while j + 1 < chars.len() && !(chars[j] == '}' && chars[j + 1] == '}') {
                j += 1;
            }
            if j + 1 >= chars.len() {
                return Err(GenAiError::Template("unclosed '{{' in template".into()));
            }
            let expr: String = chars[i + 2..j].iter().collect();
            let path = expr.trim().trim_start_matches('.').trim();
            out.push_str(&value_to_string(lookup(vars, path)));
            i = j + 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn value_to_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Str(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Int(i)) => i.to_string(),
        Some(Value::UInt(u)) => u.to_string(),
        Some(Value::Float(f)) => f.to_string(),
        Some(other) => ferroly::codec::json::to_string(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Encode)]
    struct Vars {
        name: String,
        count: u32,
    }

    #[test]
    fn substitutes_and_ignores_missing() {
        let vars = Vars {
            name: "Ada".into(),
            count: 3,
        };
        assert_eq!(
            render("Hi {{ name }}, {{count}} left", &vars).unwrap(),
            "Hi Ada, 3 left"
        );
        // leading-dot (Go style) and missing var -> empty
        assert_eq!(render("{{ .name }}/{{ missing }}", &vars).unwrap(), "Ada/");
    }

    #[test]
    fn errors_on_unclosed() {
        assert!(render_value("a {{ b", &Value::Null).is_err());
    }
}
