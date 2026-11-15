//! Golden tests: real-world LLM chat templates rendered over a fixed set of
//! conversations, byte for byte.
//!
//! `template_data/chat_fixtures.json` holds the templates, the contexts and
//! the expected output. The expected output was produced locally by the
//! reference Jinja2 implementation (3.1.4) in the environment Hugging Face
//! `transformers` uses for chat templates (`ImmutableSandboxedEnvironment` with
//! `trim_blocks`, `lstrip_blocks`, loop controls, a `tojson` without HTML
//! escaping and a `raise_exception` global). Jinja2 is only the oracle that
//! wrote the file; it is not a dependency.
//!
//! The templates, and where they come from (`tokenizer_config.json` ->
//! `chat_template` at the pinned revision):
//!
//! | Fixture | Source | Licence |
//! |---|---|---|
//! | `chatml` | `HuggingFaceTB/SmolLM2-135M-Instruct` @ `12fd25f77366fa6b3b4b768ec3050bf629380bac`, verbatim | Apache-2.0 |
//! | `zephyr` | `TinyLlama/TinyLlama-1.1B-Chat-v1.0` @ `fe8a4ea1ffedaf415f4da2f062534de366a451e6`, verbatim | Apache-2.0 |
//! | `mistral-v0.2` | `unsloth/mistral-7b-instruct-v0.2` @ `5d88c8e8dbf7446e665a3633b2f047689f4e6547`, verbatim | Apache-2.0 |
//! | `mistral-v0.3` | `unsloth/mistral-7b-instruct-v0.3` @ `60b99dcbc7f853b7cd18472a607e8d045b96f03e`, verbatim | Apache-2.0 |
//! | `phi-3` | `microsoft/Phi-3-mini-4k-instruct` @ `f39ac1d28e925b323eae81227eaba4464caced4e`, verbatim | MIT |
//! | `qwen2.5` | `Qwen/Qwen2.5-7B-Instruct` @ `a09a35458c702b33eeacc393d103063234e8bc28`, verbatim | Apache-2.0 |
//! | `llama-2-style` | hand-written for ferroly | (this crate's) |
//! | `llama-3-style` | hand-written for ferroly | (this crate's) |
//! | `gemma-2-style` | hand-written for ferroly | (this crate's) |
//!
//! The Llama-2, Llama-3 and Gemma-2 templates are published under Meta's and
//! Google's model licences, so they are not copied here. The three
//! `-style` fixtures are written from scratch to produce the same prompt
//! formats with the same Jinja2 constructs those templates rely on (a slice
//! to drop the system message, the `(a == b) != (c == d)` alternation check
//! with `raise_exception`, `!= false`, `.strip()`, `| trim` binding tighter
//! than `+`, a `set` inside an `if` reassigning a loop-body variable, a role
//! chosen in `if` / `else`).
//!
//! Conversations: one user message; system + user; multi-turn; special
//! characters (quotes, a tab, non-ASCII, an emoji, and literal
//! `{{ not_a_var }}` / `{% if %}` text that must never be parsed); two user
//! messages in a row (refused by the Mistral, Llama-2 and Gemma formats); and,
//! for Mistral v0.3 and Qwen2.5, a tool-calling exchange. As llama.cpp does for
//! a template that refuses a system role, the Gemma context has the system
//! message merged into the first user turn.
#![cfg(feature = "template")]

use std::collections::HashMap;

use ferroly::codec::{json, Value};
use ferroly::template::{Limits, Options, Template, TemplateErrorKind};

const FIXTURES: &str = include_str!("template_data/chat_fixtures.json");

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing {key}"))
}

fn with_flag(context: &Value, flag: Option<bool>) -> Value {
    let mut ctx = context.clone();
    if let (Some(entries), Some(flag)) = (ctx.as_object_mut(), flag) {
        entries.push(("add_generation_prompt".to_string(), Value::Bool(flag)));
    }
    ctx
}

#[test]
fn chat_templates_match_golden_output() {
    let doc = json::from_str(FIXTURES).expect("fixtures parse");
    let templates = doc
        .get("templates")
        .and_then(Value::as_array)
        .expect("templates");
    assert_eq!(templates.len(), 9);

    // Each template is parsed once; that one `Template` renders all its cases.
    let parsed: HashMap<String, Template> = templates
        .iter()
        .map(|t| {
            let name = str_field(t, "name");
            let tpl = Template::parse(str_field(t, "template"), &Options::default())
                .unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
            (name.to_string(), tpl)
        })
        .collect();

    let cases = doc.get("cases").and_then(Value::as_array).expect("cases");
    let mut rendered = 0;
    let mut refused = 0;
    for case in cases {
        let name = str_field(case, "template");
        let conversation = str_field(case, "conversation");
        let generation = case
            .get("add_generation_prompt")
            .and_then(Value::as_bool)
            .expect("flag");
        let template = &parsed[name];
        let context = case.get("context").expect("context");
        // A `nogen` case must give the same bytes with the flag set to false
        // and with it left unset (llama.cpp leaves it unset).
        let variants: &[Option<bool>] = if generation {
            &[Some(true)]
        } else {
            &[Some(false), None]
        };
        for flag in variants {
            let got = template.render(&with_flag(context, *flag));
            let label = format!("{name} / {conversation} / add_generation_prompt={flag:?}");
            if let Some(expect) = case.get("expect").and_then(Value::as_str) {
                let got = got.unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(got, expect, "{label}");
                assert_eq!(got.len(), expect.len(), "{label}");
                rendered += 1;
            } else {
                let message = str_field(case, "raised");
                let err = got.expect_err(&label);
                assert!(
                    matches!(err.kind(), TemplateErrorKind::Raised(m) if m == message),
                    "{label}: {err}"
                );
                refused += 1;
            }
        }
    }
    // 86 render cases (plus their unset-flag variants) and 8 refusals.
    assert_eq!(cases.len(), 94);
    assert_eq!(rendered, 86 + 43);
    assert_eq!(refused, 8 + 4);
}

#[test]
fn templates_render_concurrently_from_one_parse() {
    let doc = json::from_str(FIXTURES).expect("fixtures parse");
    let case = doc
        .get("cases")
        .and_then(Value::as_array)
        .and_then(|c| {
            c.iter().find(|c| {
                c.get("template").and_then(Value::as_str) == Some("qwen2.5")
                    && c.get("conversation").and_then(Value::as_str) == Some("tools")
            })
        })
        .expect("qwen tools case")
        .clone();
    let source = doc
        .get("templates")
        .and_then(Value::as_array)
        .and_then(|t| {
            t.iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some("qwen2.5"))
        })
        .map(|t| str_field(t, "template").to_string())
        .expect("qwen template");
    let template = Template::parse(&source, &Options::default()).expect("parses");
    let flag = case.get("add_generation_prompt").and_then(Value::as_bool);
    let context = with_flag(case.get("context").expect("context"), flag);
    let expect = str_field(&case, "expect").to_string();
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                for _ in 0..25 {
                    assert_eq!(template.render(&context).expect("renders"), expect);
                }
            });
        }
    });
}

#[test]
fn chat_templates_need_a_small_fraction_of_the_work_budget() {
    // `Limits::max_steps` defaults to 10^8; a real chat template renders a
    // conversation in a few thousand steps.
    let doc = json::from_str(FIXTURES).expect("fixtures parse");
    let options = Options::default().limits(Limits {
        max_steps: 20_000,
        ..Limits::default()
    });
    let templates = doc
        .get("templates")
        .and_then(Value::as_array)
        .expect("templates");
    let parsed: HashMap<&str, Template> = templates
        .iter()
        .map(|t| {
            let tpl = Template::parse(str_field(t, "template"), &options).expect("parses");
            (str_field(t, "name"), tpl)
        })
        .collect();
    for case in doc.get("cases").and_then(Value::as_array).expect("cases") {
        let flag = case.get("add_generation_prompt").and_then(Value::as_bool);
        let context = with_flag(case.get("context").expect("context"), flag);
        if let Err(e) = parsed[str_field(case, "template")].render(&context) {
            assert!(
                matches!(e.kind(), TemplateErrorKind::Raised(_)),
                "{}: {e}",
                str_field(case, "template")
            );
        }
    }
}

// ---- the issue's fixture manifest, from outside the repository -----------

/// The fixture set the feature request offers (seven chat templates
/// extracted byte for byte, among them the Llama-2, Llama-3 and Gemma-2
/// templates whose licences keep them out of this repository, and goldens
/// rendered by llama.cpp b9850). Point `FERROLY_TEMPLATE_FIXTURES` at the
/// directory holding its `manifest.json` to run it; without the variable the
/// test does nothing.
///
/// The manifest is read leniently. It is an array of cases, or an object
/// with one under `"cases"` (and optionally a `"templates"` object or array
/// naming template files). Each case gives:
///
/// - the template: `"template"` (a file in the directory, a key of
///   `"templates"`, or the source itself) or `"template_file"`;
/// - the context: `"context"` (an object) or `"context_file"`;
/// - the outcome: `"golden"` / `"expected_file"` / `"output_file"` (a file
///   with the expected bytes, also checked against `"length"` / `"bytes"`
///   and, with the `hash` feature, `"sha256"` when given), `"expected"` /
///   `"expect"` (the bytes inline), or `"error"` / `"raise_exception"` /
///   `"raised"` (the message `raise_exception` must raise).
///
/// As acceptance item 1 asks, each template is parsed once and that one
/// `Template` renders all its cases, and a case whose context leaves
/// `add_generation_prompt` unset or false gives the same result both ways.
#[test]
fn external_fixture_manifest_matches() {
    let Some(dir) = std::env::var_os("FERROLY_TEMPLATE_FIXTURES") else {
        eprintln!("FERROLY_TEMPLATE_FIXTURES is not set; skipping the external manifest");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let read = |name: &str| -> Vec<u8> {
        let path = dir.join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    };
    let text = |name: &str| -> String {
        String::from_utf8(read(name)).unwrap_or_else(|_| panic!("{name} is not UTF-8"))
    };
    let first_str = |case: &Value, keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| case.get(k).and_then(Value::as_str))
            .map(str::to_string)
    };
    let manifest = json::from_str(&text("manifest.json")).expect("manifest.json parses");
    let cases: &[Value] = match &manifest {
        Value::Array(cases) => cases,
        other => other
            .get("cases")
            .and_then(Value::as_array)
            .expect("the manifest has no \"cases\" array"),
    };
    let named = manifest.get("templates");
    // A case's template, as (cache key, source).
    let template_of = |case: &Value| -> (String, String) {
        if let Some(file) = first_str(case, &["template_file", "template_path"]) {
            return (file.clone(), text(&file));
        }
        let t = first_str(case, &["template"]).expect("a case has no template");
        let entry = match named {
            Some(Value::Object(_)) => named.and_then(|n| n.get(&t)),
            Some(Value::Array(list)) => list.iter().find(|e| {
                ["name", "id"]
                    .iter()
                    .any(|k| e.get(k).and_then(Value::as_str) == Some(t.as_str()))
            }),
            _ => None,
        };
        match entry {
            Some(Value::Str(file)) => (t, text(file)),
            Some(e) => match first_str(e, &["file", "path", "template_file"]) {
                Some(file) => (t, text(&file)),
                None => {
                    let source = first_str(e, &["template", "source"]).expect("template entry");
                    (t, source)
                }
            },
            None if dir.join(&t).is_file() => (t.clone(), text(&t)),
            None => (t.clone(), t),
        }
    };

    let mut parsed: HashMap<String, Template> = HashMap::new();
    let (mut rendered, mut refused) = (0, 0);
    for (i, case) in cases.iter().enumerate() {
        let label = first_str(case, &["name", "id", "case"]).unwrap_or_else(|| format!("case {i}"));
        let (key, source) = template_of(case);
        let template = parsed.entry(key).or_insert_with(|| {
            Template::parse(&source, &Options::default())
                .unwrap_or_else(|e| panic!("{label}: the template does not parse: {e}"))
        });
        let context = match first_str(case, &["context_file"]) {
            Some(file) => json::from_str(&text(&file)).expect("the context file parses"),
            None => case.get("context").cloned().unwrap_or(Value::Null),
        };
        // A "no generation prompt" case: the flag unset and the flag false
        // must give the same result.
        let flag = context
            .get("add_generation_prompt")
            .and_then(Value::as_bool);
        let contexts = if flag == Some(true) {
            vec![context]
        } else {
            let mut unset = context;
            if let Some(entries) = unset.as_object_mut() {
                entries.retain(|(k, _)| k != "add_generation_prompt");
            }
            let mut off = unset.clone();
            if let Some(entries) = off.as_object_mut() {
                entries.push(("add_generation_prompt".to_string(), Value::Bool(false)));
            }
            vec![unset, off]
        };
        let raised = first_str(
            case,
            &["error", "raise_exception", "raised", "expected_error"],
        )
        .or_else(|| {
            ["error", "raise_exception"]
                .iter()
                .find_map(|k| case.get(k))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let golden = first_str(
            case,
            &["golden", "golden_file", "expected_file", "output_file"],
        );
        let expected: Option<Vec<u8>> = match golden {
            Some(file) => Some(read(&file)),
            None => first_str(case, &["expected", "expect", "output"]).map(String::into_bytes),
        };
        for ctx in &contexts {
            let got = template.render(ctx);
            match (&expected, &raised) {
                (Some(want), _) => {
                    let got = got.unwrap_or_else(|e| panic!("{label}: {e}"));
                    assert_eq!(got.as_bytes(), &want[..], "{label}: output differs");
                    if let Some(len) = ["length", "bytes", "size"]
                        .iter()
                        .find_map(|k| case.get(k).and_then(Value::as_u64))
                    {
                        assert_eq!(got.len() as u64, len, "{label}: length");
                    }
                    #[cfg(feature = "hash")]
                    if let Some(sum) = first_str(case, &["sha256"]) {
                        assert_eq!(
                            ferroly::hash::sha256(got.as_bytes()).to_hex(),
                            sum.to_ascii_lowercase(),
                            "{label}: sha256"
                        );
                    }
                    rendered += 1;
                }
                (None, Some(message)) => {
                    let err = got.expect_err(&label);
                    assert_eq!(
                        err.raised_message(),
                        Some(message.as_str()),
                        "{label}: {err}"
                    );
                    refused += 1;
                }
                (None, None) => panic!("{label}: the case gives neither an output nor an error"),
            }
        }
    }
    assert!(!cases.is_empty(), "the manifest has no cases");
    eprintln!(
        "{} cases from {}: {rendered} renders and {refused} refusals checked, {} templates each parsed once",
        cases.len(),
        dir.display(),
        parsed.len()
    );
}
