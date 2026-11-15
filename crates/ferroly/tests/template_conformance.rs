//! Differential conformance: `template_data/conformance.json` holds small
//! templates with their context and the output (or error class) that the
//! reference Jinja2 implementation (3.1.4) produced for them, run in the
//! environment Hugging Face `transformers` uses for chat templates
//! (`ImmutableSandboxedEnvironment(trim_blocks=True, lstrip_blocks=True)` with
//! loop controls, a `tojson` without HTML escaping, and `raise_exception`).
//! Jinja2 was used only as a local oracle to produce the expectations; it is
//! not a dependency. Every case must agree with it.
#![cfg(feature = "template")]

use ferroly::codec::{json, Value};
use ferroly::template::{Options, Template, TemplateErrorKind};

const CASES: &str = include_str!("template_data/conformance.json");

fn field<'a>(case: &'a Value, key: &str) -> Option<&'a Value> {
    case.get(key)
}

#[test]
fn agrees_with_reference_jinja2() {
    let doc = json::from_str(CASES).expect("conformance.json parses");
    let cases = doc.get("cases").and_then(Value::as_array).expect("cases");
    assert!(cases.len() > 150, "conformance set unexpectedly small");
    let mut failures = Vec::new();
    for case in cases {
        let source = field(case, "template")
            .and_then(Value::as_str)
            .expect("template");
        let context = field(case, "context").cloned().unwrap_or(Value::Null);
        let parsed = Template::parse(source, &Options::default());
        let outcome = parsed.and_then(|t| t.render(&context));
        let expect = field(case, "expect").and_then(Value::as_str);
        let error = field(case, "error").and_then(Value::as_str);
        let ok = match (&outcome, expect, error) {
            (Ok(out), Some(want), _) => out == want,
            (Err(e), None, Some("parse")) => matches!(
                e.kind(),
                TemplateErrorKind::Syntax(_)
                    | TemplateErrorKind::UnknownFilter(_)
                    | TemplateErrorKind::UnknownTest(_)
                    | TemplateErrorKind::UnknownFunction(_)
            ),
            (Err(e), None, Some("undefined")) => {
                matches!(e.kind(), TemplateErrorKind::Undefined(_))
            }
            (Err(e), None, Some("type")) => matches!(e.kind(), TemplateErrorKind::Type(_)),
            (Err(e), None, Some("raised")) => {
                let want = field(case, "message").and_then(Value::as_str).unwrap_or("");
                e.raised_message() == Some(want)
            }
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "template {source:?}\n   expected {:?}\n   got      {:?}",
                expect
                    .map(str::to_string)
                    .or(error.map(|e| format!("error: {e}"))),
                outcome.map_err(|e| e.to_string())
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ from Jinja2:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
