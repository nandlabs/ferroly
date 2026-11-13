#![cfg(feature = "genai")]
//! Prompt-template tests: a byte-driven "fuzz-lite" pass that feeds the
//! renderer arbitrary input and asserts it never panics, plus loading
//! templates from a directory.

use std::collections::HashMap;
use std::path::PathBuf;

use ferroly::codec::{json, Value};
use ferroly::genai::template::{self, RenderOptions, Template};
use ferroly::genai::{GenAiError, InMemoryPromptStore, Message, PromptStore, PromptTemplate, Role};

/// A small deterministic generator (xorshift64*), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn vars() -> Value {
    json::from_str(
        r#"{"name":"Ada","n":3,"on":true,"off":false,"none":null,
            "xs":[1,"two",{"k":"v"},[4]],"m":{"a":1,"b":{"c":"deep"}},
            "tpl":"{{ name }}{{#each xs}}{{/each}}"}"#,
    )
    .unwrap()
}

/// Parses and renders `source` every way the engine offers. The only
/// requirement is that nothing panics, loops forever, or blows the size cap.
fn exercise(source: &str, vars: &Value, partials: &HashMap<String, String>) {
    let lenient = template::render_value(source, vars);
    let Ok(parsed) = Template::parse(source) else {
        assert!(
            lenient.is_err(),
            "parse failed but lenient render succeeded"
        );
        return;
    };
    for options in [
        RenderOptions::strict(),
        RenderOptions::lenient(),
        RenderOptions::lenient().allow_root(true),
        RenderOptions::strict().partials(partials),
    ] {
        let options = options.max_output_bytes(64 * 1024);
        if let Ok(out) = parsed.render_with(vars, &options) {
            assert!(out.len() <= 64 * 1024, "output cap not enforced");
        }
    }
    let _ = parsed.variables();
    // Compiling is deterministic.
    assert_eq!(Template::parse(source).unwrap(), parsed);
}

/// Pending B5: arbitrary bytes never make the template engine panic.
#[test]
fn renderer_never_panics_on_arbitrary_bytes() {
    let vars = vars();
    let mut partials = HashMap::new();
    partials.insert("p".to_string(), "[{{ name }}{{> p }}]".to_string());
    partials.insert("x".to_string(), "{{#each xs}}{{ . }}{{/each}}".to_string());
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

    // 1. Raw bytes, decoded lossily (invalid UTF-8 becomes U+FFFD).
    for _ in 0..4_000 {
        let len = rng.below(48);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        exercise(&String::from_utf8_lossy(&bytes), &vars, &partials);
    }

    // 2. Bytes biased toward the characters the syntax is made of.
    let alphabet: &[u8] = b"{}#/>!@\\. \n\taeinstx01\"";
    for _ in 0..4_000 {
        let len = rng.below(40);
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                if rng.below(8) == 0 {
                    rng.next() as u8
                } else {
                    alphabet[rng.below(alphabet.len())]
                }
            })
            .collect();
        exercise(&String::from_utf8_lossy(&bytes), &vars, &partials);
    }

    // 3. Whole tokens glued together at random: well-formed and nearly
    //    well-formed templates, deep nesting, multi-byte text.
    let tokens = [
        "{{",
        "}}",
        "{{ name }}",
        "{{#if on}}",
        "{{#if none}}",
        "{{#unless off}}",
        "{{else}}",
        "{{/if}}",
        "{{/unless}}",
        "{{#each xs}}",
        "{{#each m}}",
        "{{#each n}}",
        "{{/each}}",
        "{{ . }}",
        "{{ this }}",
        "{{ this.k }}",
        "{{ @index }}",
        "{{ @key }}",
        "{{ @last }}",
        "{{> p }}",
        "{{> x }}",
        "{{> nope }}",
        "{{! note }}",
        "\\{{",
        "\\",
        "{{ m.b.c }}",
        "{{ xs.2.k }}",
        "{{ tpl }}",
        "{{ missing }}",
        "{{#if}}",
        "{{#}}",
        "{{/}}",
        "{{>}}",
        "{",
        "}",
        " text ",
        "h\u{e9}llo ",
        "\u{4e16}\u{754c}",
        "\u{1f600}",
        "\n",
    ];
    for _ in 0..6_000 {
        let count = rng.below(14);
        let source: String = (0..count)
            .map(|_| tokens[rng.below(tokens.len())])
            .collect();
        exercise(&source, &vars, &partials);
    }
}

/// Text without any tag comes out unchanged, byte for byte.
#[test]
fn plain_text_is_passed_through_unchanged() {
    let mut rng = Rng(42);
    for _ in 0..2_000 {
        let len = rng.below(64);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let text = String::from_utf8_lossy(&bytes).replace("{{", "{ {");
        assert_eq!(template::render_value(&text, &Value::Null).unwrap(), text);
    }
}

/// Deeply nested and very long inputs are handled without recursion blow-up.
#[test]
fn pathological_inputs_are_bounded() {
    let vars = vars();
    let deep = format!("{}x{}", "{{#if on}}".repeat(33), "{{/if}}".repeat(33));
    assert!(Template::parse(&deep).is_err(), "nesting is capped");
    let ok = format!("{}x{}", "{{#if on}}".repeat(32), "{{/if}}".repeat(32));
    assert_eq!(
        Template::parse(&ok).unwrap().render_value(&vars).unwrap(),
        "x"
    );

    // A million braces: linear time, clean error.
    let braces = "{".repeat(1_000_000);
    assert!(Template::parse(&braces).is_err());
    let closers = "}".repeat(1_000_000);
    assert_eq!(template::render_value(&closers, &vars).unwrap(), closers);

    // Nested loops over the same list multiply the output; the cap stops it.
    let bomb = "{{#each xs}}{{#each xs}}{{#each xs}}{{#each xs}}0123456789{{/each}}{{/each}}{{/each}}{{/each}}";
    let big: Value = Value::Object(vec![(
        "xs".to_string(),
        Value::Array(vec![Value::Int(1); 200]),
    )]);
    let err = Template::parse(bomb)
        .unwrap()
        .render_with(&big, &RenderOptions::strict().max_output_bytes(1 << 20))
        .unwrap_err();
    assert!(err.to_string().contains("exceeds"), "{err}");
}

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ferroly-genai-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    fn write(&self, relative: &str, content: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(ferroly::codec::Encode)]
struct Vars {
    name: String,
    items: Vec<String>,
}

/// Templates are loaded from a directory with ids, versions and partials.
#[test]
fn loads_versioned_templates_from_a_directory() {
    let dir = TempDir::new("prompts");
    dir.write("greet.prompt", "Hi {{ name }}");
    dir.write(
        "greet@2.prompt",
        "Hello {{ name }}!\n{{> shared/signature }}",
    );
    dir.write(
        "greet@10.prompt",
        "Dear {{ name }},\n{{> shared/signature }}",
    );
    dir.write("shared/signature.tmpl", "-- the team");
    dir.write(
        "support/list.md",
        "{{#each items}}- {{ this }}\n{{else}}(nothing)\n{{/each}}",
    );
    dir.write("notes.rs", "not a template: {{ unclosed");
    dir.write(".hidden.prompt", "{{ unclosed");

    let store = InMemoryPromptStore::load_dir(&dir.0).unwrap();
    assert_eq!(store.ids(), ["greet", "shared/signature", "support/list"]);
    assert_eq!(store.versions("greet"), ["2", "10"]);

    let vars = Vars {
        name: "Ada".into(),
        items: vec!["tea".into(), "cake".into()],
    };
    // The latest version, with the partial resolved from the same store.
    assert_eq!(
        store.render("greet", &vars).unwrap(),
        "Dear Ada,\n-- the team"
    );
    assert_eq!(
        store.render_version("greet", "2", &vars).unwrap(),
        "Hello Ada!\n-- the team"
    );
    assert_eq!(store.get("greet").unwrap().version.as_deref(), Some("10"));
    assert_eq!(
        store.render("support/list", &vars).unwrap(),
        "- tea\n- cake\n"
    );

    // Strict by default through the new entry point; the legacy one is lenient.
    let err = Message::from_prompt(Role::User, &store, "greet", &()).unwrap_err();
    assert!(
        matches!(&err, GenAiError::Template(m) if m.contains("greet") && m.contains("undefined variable 'name'")),
        "{err:?}"
    );
    let legacy = Message::from_prompt_id(Role::User, &store, "greet", &()).unwrap_err();
    // (the latest version uses a partial, which the legacy path cannot resolve)
    assert!(matches!(legacy, GenAiError::Template(_)));
    let mut plain = InMemoryPromptStore::new();
    plain.add(PromptTemplate::new("g", "G", "Hi {{ name }}."));
    let msg = Message::from_prompt_id(Role::User, &plain, "g", &()).unwrap();
    assert_eq!(msg.text_content(), "Hi .");
}

/// A broken template file is reported with its path at load time.
#[test]
fn load_dir_reports_bad_files() {
    let dir = TempDir::new("bad-prompts");
    dir.write("ok.prompt", "fine");
    dir.write("broken.prompt", "{{#if x}}never closed");
    let err = InMemoryPromptStore::load_dir(&dir.0)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("broken.prompt") && err.contains("never closed"),
        "{err}"
    );

    let missing = dir.0.join("does-not-exist");
    assert!(matches!(
        InMemoryPromptStore::load_dir(&missing),
        Err(GenAiError::Template(_))
    ));
}
