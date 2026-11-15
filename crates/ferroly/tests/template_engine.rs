//! `ferroly::template`: parse-time errors and their positions, limits,
//! options, host functions, and a generative test that no template or
//! context makes the engine panic or run away.
#![cfg(feature = "template")]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ferroly::codec::{json, Value};
use ferroly::template::{Limits, Options, Template, TemplateError, TemplateErrorKind};

fn parse(src: &str) -> Result<Template, TemplateError> {
    Template::parse(src, &Options::default())
}

fn render(src: &str, ctx: &str) -> Result<String, TemplateError> {
    parse(src)?.render(&json::from_str(ctx).expect("context json"))
}

fn ok(src: &str, ctx: &str) -> String {
    render(src, ctx).unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

fn parse_err(src: &str) -> TemplateError {
    match parse(src) {
        Ok(_) => panic!("{src:?} parsed"),
        Err(e) => e,
    }
}

fn render_err(src: &str, ctx: &str) -> TemplateError {
    let t = parse(src).unwrap_or_else(|e| panic!("{src:?} does not parse: {e}"));
    match t.render(&json::from_str(ctx).expect("context json")) {
        Ok(out) => panic!("{src:?} rendered {out:?}"),
        Err(e) => e,
    }
}

fn limit_name(e: &TemplateError) -> &'static str {
    match e.kind() {
        TemplateErrorKind::LimitExceeded(name) => name,
        other => panic!("expected a limit error, got {other:?}"),
    }
}

// ---- parse-time errors, with positions ---------------------------------

#[test]
fn unclosed_tag_is_a_syntax_error_at_the_tag() {
    let e = parse_err("Hello {{ name");
    assert!(matches!(e.kind(), TemplateErrorKind::Syntax(_)), "{e}");
    assert_eq!((e.offset(), e.line(), e.column()), (6, 1, 7));

    let e = parse_err("ok\n{% if x");
    assert!(matches!(e.kind(), TemplateErrorKind::Syntax(_)), "{e}");
    assert_eq!((e.offset(), e.line(), e.column()), (3, 2, 1));

    let e = parse_err("a {# never closed");
    assert!(matches!(e.kind(), TemplateErrorKind::Syntax(_)), "{e}");
    assert_eq!(e.offset(), 2);
}

#[test]
fn stray_end_tag_is_a_syntax_error_at_the_tag() {
    let e = parse_err("text {% endfor %}");
    assert!(
        matches!(e.kind(), TemplateErrorKind::Syntax(m) if m.contains("endfor")),
        "{e}"
    );
    assert_eq!((e.line(), e.column()), (1, 9));

    let e = parse_err("{% for x in y %}\n{% endif %}");
    assert!(matches!(e.kind(), TemplateErrorKind::Syntax(_)), "{e}");
    assert_eq!(e.line(), 2);
}

#[test]
fn unclosed_block_points_at_its_opening_statement() {
    let e = parse_err("a\n  {% if x %}never closed");
    assert!(
        matches!(e.kind(), TemplateErrorKind::Syntax(m) if m.contains("endif")),
        "{e}"
    );
    assert_eq!((e.line(), e.column()), (2, 6));
}

#[test]
fn unknown_filter_test_and_function_fail_at_parse_time() {
    let e = parse_err("a\nb\n  {{ x | bogus }}");
    assert_eq!(e.kind(), &TemplateErrorKind::UnknownFilter("bogus".into()));
    assert_eq!((e.line(), e.column()), (3, 10));

    let e = parse_err("{% if x is wobbly %}{% endif %}");
    assert_eq!(e.kind(), &TemplateErrorKind::UnknownTest("wobbly".into()));
    assert_eq!(e.offset(), 11);

    let e = parse_err("{{ nope(1) }}");
    assert_eq!(e.kind(), &TemplateErrorKind::UnknownFunction("nope".into()));
    assert_eq!(e.offset(), 3);

    // A test or filter named in a string literal is checked too.
    let e = parse_err("{{ xs | select('wobbly') }}");
    assert_eq!(e.kind(), &TemplateErrorKind::UnknownTest("wobbly".into()));
    let e = parse_err("{{ xs | map('bogus') }}");
    assert_eq!(e.kind(), &TemplateErrorKind::UnknownFilter("bogus".into()));

    // Names the template defines itself are callable.
    assert_eq!(ok("{% macro m() %}m{% endmacro %}{{ m() }}", "{}"), "m");
}

#[test]
fn unterminated_string_is_a_syntax_error_at_the_quote() {
    let e = parse_err("{{ 'abc }}");
    assert!(
        matches!(e.kind(), TemplateErrorKind::Syntax(m) if m.contains("string")),
        "{e}"
    );
    assert_eq!(e.offset(), 3);
}

#[test]
fn other_syntax_errors() {
    for src in [
        "{{ }}",
        "{{ 1 + }}",
        "{{ (1 }}",
        "{{ ] }}",
        "{% wobble %}",
        "{% break %}",
        "{% for x in y %}{% macro m() %}{% break %}{% endmacro %}{% endfor %}",
        "{% set %}",
        "{{ f(a=1, 2) }}",
        "{{ 99999999999999999999999999999999999999999 }}",
        "{{ x ! y }}",
        "{% raw %} never ended",
    ] {
        let e = parse_err(src);
        assert!(
            matches!(
                e.kind(),
                TemplateErrorKind::Syntax(_) | TemplateErrorKind::UnknownFunction(_)
            ),
            "{src:?}: {e}"
        );
    }
}

#[test]
fn unsupported_statements_are_refused_at_parse_time() {
    for src in [
        "{% include 'x.html' %}",
        "{% import 'x' as y %}",
        "{% from 'x' import y %}",
        "{% extends 'base' %}",
        "{% block body %}{% endblock %}",
        "{% call m() %}{% endcall %}",
        "{% filter upper %}x{% endfilter %}",
        "{% with a = 1 %}{% endwith %}",
        "{% for x in y recursive %}{% endfor %}",
        "{{ f(*args) }}",
    ] {
        let e = parse_err(src);
        assert!(
            matches!(e.kind(), TemplateErrorKind::Unsupported(_)),
            "{src:?}: {e}"
        );
    }
}

#[test]
fn error_display_names_the_position() {
    let e = parse_err("x\n{{ y | bogus }}");
    let text = e.to_string();
    assert!(
        text.contains("bogus") && text.contains("line 2") && text.contains("column 8"),
        "{text}"
    );
    // It converts into the crate-wide error.
    let wrapped: ferroly::Error = e.into();
    assert!(wrapped.to_string().contains("bogus"));
    #[cfg(feature = "errutils")]
    {
        use ferroly::errutils::{Classify, ErrorKind};
        assert_eq!(wrapped.kind(), ErrorKind::InvalidInput);
        let unsupported: ferroly::Error = parse_err("{% include 'x' %}").into();
        assert_eq!(unsupported.kind(), ErrorKind::Unsupported);
    }
}

// ---- runtime errors ----------------------------------------------------

#[test]
fn typed_runtime_errors_carry_positions() {
    let e = render_err("ok\n{{ 'a' + [1] }}", "{}");
    assert!(matches!(e.kind(), TemplateErrorKind::Type(_)), "{e}");
    assert_eq!(e.line(), 2);

    let e = render_err("{{ messages[0]['role'] }}", "{}");
    assert!(
        matches!(e.kind(), TemplateErrorKind::Undefined(m) if m.contains("messages")),
        "{e}"
    );

    let e = render_err("{{ 1 // 0 }}", "{}");
    assert!(matches!(e.kind(), TemplateErrorKind::Type(_)), "{e}");

    let e = render_err("{% set x = 1 %}{% set x.a = 2 %}", "{}");
    assert!(matches!(e.kind(), TemplateErrorKind::Type(_)), "{e}");

    let e = render_err("{{ raise_exception('stop here') }}", "{}");
    assert_eq!(e.raised_message(), Some("stop here"));
    assert_eq!(e.offset(), 3);
}

#[test]
fn lenient_undefined_and_mixed_comparisons() {
    // Unset names are false, print as nothing, iterate as empty.
    assert_eq!(
        ok(
            "[{{ nope }}]{% if nope %}y{% else %}n{% endif %}{% for x in nope %}x{% endfor %}",
            "{}"
        ),
        "[]n"
    );
    assert_eq!(ok("{{ system_message is defined }}", "{}"), "False");
    // Mixed types compare unequal without an error.
    assert_eq!(
        ok("{{ s != false }}{{ 1 == '1' }}", r#"{"s":"x"}"#),
        "TrueFalse"
    );
}

#[test]
fn data_is_printed_verbatim_and_never_parsed() {
    let out = ok("{{ m }}", r#"{"m":"{{ secret }} {% if %} <&\"'>"}"#);
    assert_eq!(out, "{{ secret }} {% if %} <&\"'>");
}

#[test]
fn tojson_keeps_input_key_order() {
    let out = ok(
        "{{ x | tojson }}",
        r#"{"x":{"z":1,"a":{"m":2,"b":[true,null]}}}"#,
    );
    assert_eq!(out, r#"{"z": 1, "a": {"m": 2, "b": [true, null]}}"#);
    let out = ok("{{ x | tojson(indent=2) }}", r#"{"x":{"z":1,"a":[]}}"#);
    assert_eq!(out, "{\n  \"z\": 1,\n  \"a\": []\n}");
}

#[test]
fn context_must_be_a_map() {
    let t = parse("x").expect("parses");
    assert_eq!(t.render(&Value::Null).expect("null is empty"), "x");
    let e = t
        .render(&Value::from(vec![Value::from(1)]))
        .expect_err("array context");
    assert!(matches!(e.kind(), TemplateErrorKind::Type(_)), "{e}");
}

// ---- options -----------------------------------------------------------

#[test]
fn whitespace_options() {
    let src = "  {% if true %}\nx\n{% endif %}\n";
    assert_eq!(
        Template::parse(src, &Options::default())
            .unwrap()
            .render(&Value::Null)
            .unwrap(),
        "x\n"
    );
    let raw = Options::default().trim_blocks(false).lstrip_blocks(false);
    assert_eq!(
        Template::parse(src, &raw)
            .unwrap()
            .render(&Value::Null)
            .unwrap(),
        "  \nx\n"
    );
    let keep = Options::default().keep_trailing_newline(true);
    assert_eq!(
        Template::parse("x\n", &keep)
            .unwrap()
            .render(&Value::Null)
            .unwrap(),
        "x\n"
    );
    assert_eq!(
        Template::parse("x\n", &Options::default())
            .unwrap()
            .render(&Value::Null)
            .unwrap(),
        "x"
    );
}

#[test]
fn raise_exception_can_be_turned_off() {
    let opts = Options::default().raise_exception(false);
    let e = Template::parse("{{ raise_exception('x') }}", &opts).expect_err("unknown");
    assert_eq!(
        e.kind(),
        &TemplateErrorKind::UnknownFunction("raise_exception".into())
    );
}

#[test]
fn host_functions_receive_values_and_report_errors() {
    let opts = Options::default()
        .function("add", |args: &[Value]| -> Result<Value, String> {
            let sum: i64 = args.iter().filter_map(Value::as_i64).sum();
            Ok(Value::from(sum))
        })
        .function("fail", |_: &[Value]| -> Result<Value, String> {
            Err("no clock here".into())
        })
        .function("echo", |args: &[Value]| -> Result<Value, String> {
            Ok(args.first().cloned().unwrap_or(Value::Null))
        });
    let t = Template::parse("{{ add(1, 2, 3) }}|{{ echo({'k': [1, 'v']}).k[1] }}", &opts).unwrap();
    assert_eq!(t.render(&Value::Null).unwrap(), "6|v");

    let t = Template::parse("{{ fail() }}", &opts).unwrap();
    let e = t.render(&Value::Null).unwrap_err();
    assert_eq!(
        e.kind(),
        &TemplateErrorKind::Function {
            name: "fail".into(),
            message: "no clock here".into()
        }
    );
    let t = Template::parse("{{ add(a=1) }}", &opts).unwrap();
    assert!(matches!(
        t.render(&Value::Null).unwrap_err().kind(),
        TemplateErrorKind::Type(_)
    ));
}

#[test]
fn template_is_send_sync_and_cheap_to_clone() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<Template>();
    assert_send_sync::<Options>();
    let t = parse("{{ x }}").unwrap();
    let u = t.clone();
    assert_eq!(u.source(), "{{ x }}");
}

// ---- limits ------------------------------------------------------------

/// Runs `f` on its own thread with `stack` bytes of stack, and fails the
/// test if it has not finished within `limit`: a hang is reported as a
/// failure instead of stalling the suite, and a panic is passed on.
fn bounded<T: Send + 'static>(
    limit: Duration,
    stack: usize,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            let _ = tx.send(f());
        })
        .expect("spawn the bounded thread");
    match rx.recv_timeout(limit) {
        Ok(v) => {
            let _ = worker.join();
            v
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("did not finish within {limit:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => panic!("the bounded thread exited without a result"),
        },
    }
}

/// [`bounded`] with a generous limit (debug builds are slow) and the main
/// thread's usual stack.
fn quick<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    bounded(Duration::from_secs(30), 8 << 20, f)
}

#[test]
fn source_size_limit() {
    let opts = Options::default().limits(Limits {
        max_source_bytes: 10,
        ..Limits::default()
    });
    let e = Template::parse("{{ 'this is too long' }}", &opts).unwrap_err();
    assert_eq!(limit_name(&e), "max_source_bytes");
}

#[test]
fn deep_nesting_is_refused_at_parse_time() {
    let blocks = "{% if true %}".repeat(200) + &"{% endif %}".repeat(200);
    assert_eq!(limit_name(&parse_err(&blocks)), "max_depth");

    let parens = "{{ ".to_string() + &"(".repeat(5000) + "1" + &")".repeat(5000) + " }}";
    assert_eq!(limit_name(&parse_err(&parens)), "max_depth");

    let lists = "{{ ".to_string() + &"[".repeat(5000) + &"]".repeat(5000) + " }}";
    assert_eq!(limit_name(&parse_err(&lists)), "max_depth");

    let unary = "{{ ".to_string() + &"-".repeat(5000) + "1 }}";
    assert_eq!(limit_name(&parse_err(&unary)), "max_depth");

    let nots = "{{ ".to_string() + &"not ".repeat(5000) + "1 }}";
    assert_eq!(limit_name(&parse_err(&nots)), "max_depth");

    // A long run of one operator is kept flat: it is not nesting.
    let chain = "{{ 1".to_string() + &" + 1".repeat(1000) + " }}";
    assert_eq!(ok(&chain, "{}"), "1001");

    // Just under the limit parses and renders.
    let depth = Limits::default().max_depth - 2;
    let fine = "{% if true %}".repeat(depth) + "ok" + &"{% endif %}".repeat(depth);
    assert_eq!(ok(&fine, "{}"), "ok");
}

#[test]
fn runaway_macro_recursion_hits_the_depth_limit() {
    let e = quick(|| {
        render_err(
            "{% macro f(n) %}{{ f(n + 1) }}{% endmacro %}{{ f(0) }}",
            "{}",
        )
    });
    assert_eq!(limit_name(&e), "max_depth");
}

#[test]
fn nested_loops_over_a_large_list_hit_the_iteration_limit() {
    let xs: Vec<String> = (0..1000).map(|i| i.to_string()).collect();
    let ctx = format!("{{\"xs\":[{}]}}", xs.join(","));
    let src =
        "{% for a in xs %}{% for b in xs %}{% for c in xs %}{% endfor %}{% endfor %}{% endfor %}";
    let e = quick(move || render_err(src, &ctx));
    assert_eq!(limit_name(&e), "max_iterations");
}

#[test]
fn huge_output_hits_the_output_limit() {
    let e = quick(|| {
        render_err(
            "{% for i in range(900000) %}xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx{% endfor %}",
            "{}",
        )
    });
    assert_eq!(limit_name(&e), "max_output_bytes");

    let small = Options::default().limits(Limits {
        max_output_bytes: 100,
        ..Limits::default()
    });
    let t = Template::parse(
        "{% macro m() %}{{ 'y' * 200 }}{% endmacro %}{{ m() }}",
        &small,
    )
    .unwrap();
    assert_eq!(
        limit_name(&t.render(&Value::Null).unwrap_err()),
        "max_output_bytes"
    );
}

#[test]
fn built_values_hit_the_allocation_limit() {
    // With the default limits, a huge value is refused before it is built.
    for src in [
        "{{ 'x' * 1000000000000 }}",
        "{{ range(1000000000000) | length }}",
        "{{ [1] * 1000000000000 }}",
    ] {
        let e = quick(move || render_err(src, "{}"));
        assert_eq!(limit_name(&e), "max_alloc_bytes", "{src}");
    }
    // Values grown step by step, under a smaller budget to keep the test light.
    let opts = Options::default().limits(Limits {
        max_alloc_bytes: 32 << 20,
        ..Limits::default()
    });
    for src in [
        "{% set ns = namespace(s='xy') %}{% for i in range(64) %}{% set ns.s = ns.s ~ ns.s %}{% endfor %}",
        "{% set ns = namespace(s='xy') %}{% for i in range(64) %}{% set ns.s = ns.s + ns.s %}{% endfor %}",
        "{% set s = 'x' * 1000000 %}{{ ([s] * 1000) | join }}",
        "{% set s = 'x' * 1000000 %}{{ [s] * 1000 }}",
        "{% set s = 'x' * 1000000 %}{{ ([s] * 1000) | tojson }}",
        "{% set s = 'x' * 1000000 %}{{ s | replace('', s) }}",
    ] {
        let t = Template::parse(src, &opts).unwrap();
        let e = quick(move || t.render(&Value::Null).unwrap_err());
        assert_eq!(limit_name(&e), "max_alloc_bytes", "{src}");
    }
}

#[test]
fn call_limit() {
    let opts = Options::default().limits(Limits {
        max_calls: 10,
        ..Limits::default()
    });
    let t = Template::parse(
        "{% for i in range(20) %}{{ i | string }}{% endfor %}",
        &opts,
    )
    .unwrap();
    assert_eq!(
        limit_name(&t.render(&Value::Null).unwrap_err()),
        "max_calls"
    );
}

#[test]
fn deeply_nested_values_are_refused_not_overflowed() {
    // A context nested far deeper than the engine walks.
    let mut v = Value::from(1);
    for _ in 0..5000 {
        v = Value::Array(vec![v]);
    }
    let ctx = Value::Object(vec![("x".into(), v)]);
    let e = parse("{{ x }}").unwrap().render(&ctx).unwrap_err();
    assert_eq!(limit_name(&e), "max_depth");

    // Values the template builds itself.
    let e = quick(|| {
        render_err(
            "{% set ns = namespace(v=1) %}{% for i in range(100000) %}{% set ns.v = [ns.v] %}{% endfor %}{{ ns.v }}",
            "{}",
        )
    });
    assert_eq!(limit_name(&e), "max_depth");

    // A namespace that contains itself prints as an error, not a hang.
    let e = render_err(
        "{% set ns = namespace() %}{% set ns.me = ns %}{{ ns }}",
        "{}",
    );
    assert!(
        matches!(e.kind(), TemplateErrorKind::LimitExceeded(_)),
        "{e}"
    );
}

// ---- review regressions: each ran away, panicked or overflowed before ----

/// `i128::MAX`, the largest integer literal a template can write.
const I128_MAX: &str = "170141183460469231731687303715884105727";

fn render_with(src: &str, ctx: &str, limits: Limits) -> Result<String, TemplateError> {
    let opts = Options::default().limits(limits);
    Template::parse(src, &opts)
        .unwrap_or_else(|e| panic!("{src:?} does not parse: {e}"))
        .render(&json::from_str(ctx).expect("context json"))
}

/// A list nested `depth` levels deep, built by the template itself.
fn nested_list(depth: usize) -> String {
    format!(
        "{{% set ns = namespace(l=[1]) %}}{{% for i in range({depth}) %}}{{% set ns.l = [ns.l] %}}{{% endfor %}}"
    )
}

#[test]
fn repeating_an_empty_list_or_string_is_instant() {
    // TPL-1: `[] * n` used to loop n times copying nothing.
    let src = "{{ ([] * 100000000000000000)|length }}{{ (100000000000000000 * [])|length }}\
               {{ ('' * 100000000000000000)|length }}{{ [1, 2] * 0 }}{{ [1] * -5 }}";
    let out = bounded(Duration::from_secs(5), 8 << 20, move || ok(src, "{}"));
    assert_eq!(out, "000[][]");
}

#[test]
fn sort_never_panics_on_nan_or_mixed_kinds() {
    // TPL-2: the comparator was not a total order, which std's sort may
    // detect and panic on.
    let out = quick(|| {
        ok(
            "{% set nan = 'nan'|float %}{{ [nan,21,7,16,2,8,26,18,22,nan,3,12,2,2,21,15,20,5,8,12,0]|sort }}",
            "{}",
        )
    });
    assert_eq!(
        out,
        "[0, 2, 2, 2, 3, 5, 7, 8, 8, 12, 12, 15, 16, 18, 20, 21, 21, 22, 26, nan, nan]"
    );
    assert_eq!(
        ok(
            "{% set nan = 1e999 - 1e999 %}{{ [3, nan, 1, 1e999, -1e999]|sort }}",
            "{}"
        ),
        "[-inf, 1, 3, inf, nan]"
    );
    // Integers and floats are ordered exactly, even beyond 2**53.
    assert_eq!(
        ok(
            "{{ [9007199254740993, 9007199254740992.0, 9007199254740992]|sort }}",
            "{}"
        ),
        "[9007199254740992.0, 9007199254740992, 9007199254740993]"
    );
    // A large list mixing kinds is a type error, never a panic.
    let mixed: Vec<String> = (0..300)
        .map(|i| {
            if i % 3 == 0 {
                format!("'s{i}'")
            } else {
                i.to_string()
            }
        })
        .collect();
    let src = format!("{{{{ [{}]|sort }}}}", mixed.join(","));
    let e = quick(move || render_err(&src, "{}"));
    assert!(matches!(e.kind(), TemplateErrorKind::Type(_)), "{e}");

    // Random lists of numbers (NaN and infinities included), strings and
    // nested lists, through `sort` and `dictsort`: Ok or a typed error.
    let mut rng = Rng(0x5eed);
    let atoms = [
        "nan",
        "1e999",
        "-1e999",
        "0",
        "-0.0",
        "2",
        "2.0",
        "7",
        "-3",
        "1.5",
        "'a'",
        "'B'",
        "none",
        "[1, nan]",
        "[nan, 'x']",
        "[]",
        "true",
    ];
    let mut cases = Vec::new();
    for _ in 0..400 {
        let items: Vec<&str> = (0..rng.below(40))
            .map(|_| atoms[rng.below(atoms.len())])
            .collect();
        let list = items.join(", ");
        let dict: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, v)| format!("'k{i}': {v}"))
            .collect();
        cases.push(format!(
            "{{% set nan = 'nan'|float %}}{{{{ [{list}]|sort }}}}{{{{ [{list}]|sort(reverse=true, case_sensitive=true) }}}}"
        ));
        cases.push(format!(
            "{{% set nan = 'nan'|float %}}{{{{ {{{}}}|dictsort(by='value') }}}}",
            dict.join(", ")
        ));
    }
    quick(move || {
        for src in cases {
            match render(&src, "{}") {
                Ok(_) => {}
                Err(e) => assert!(
                    matches!(
                        e.kind(),
                        TemplateErrorKind::Type(_) | TemplateErrorKind::Undefined(_)
                    ),
                    "{src}: {e}"
                ),
            }
        }
    });
}

#[test]
fn extreme_integers_neither_overflow_nor_wrap() {
    // TPL-3: each of these hit unchecked arithmetic (a panic with overflow
    // checks on, a wrong answer without).
    let m = I128_MAX;
    let cases = [
        (format!("{{{{ [1,2,3][1::{m}] }}}}"), Ok("[2]")),
        (format!("{{{{ [1,2,3][::-{m} - 1] }}}}"), Ok("[3]")),
        (format!("{{{{ range(5, 1, -{m} - 1)|list }}}}"), Ok("[5]")),
        (
            format!("{{{{ range(-{m}, {m})|length }}}}"),
            Err("max_alloc_bytes"),
        ),
        (
            format!("{{{{ range(0, {m}, 2)|length }}}}"),
            Err("max_alloc_bytes"),
        ),
        (
            "{{ 'a b'.split(' ', 18446744073709551615) }}".to_string(),
            Ok("['a', 'b']"),
        ),
        (
            "{{ 'a b'.rsplit(' ', 18446744073709551615) }}".to_string(),
            Ok("['a', 'b']"),
        ),
        (
            "{{ 'a'|center(20000000000000000000) }}".to_string(),
            Err("max_alloc_bytes"),
        ),
    ];
    quick(move || {
        for (src, want) in &cases {
            match (render(src, "{}"), want) {
                (Ok(out), Ok(w)) => assert_eq!(out, *w, "{src}"),
                (Err(e), Err(limit)) => assert_eq!(limit_name(&e), *limit, "{src}"),
                (got, _) => panic!("{src}: {got:?}"),
            }
        }
    });
    // With no allocation budget at all, a size that does not fit in a
    // `usize` is still refused, not a capacity-overflow panic.
    let unlimited = Limits {
        max_alloc_bytes: usize::MAX,
        ..Limits::default()
    };
    for src in [
        "{{ [1, 2] * 10000000000000000000 }}",
        "{{ 'ab' * 10000000000000000000 }}",
    ] {
        let e = quick(move || render_with(src, "{}", unlimited).unwrap_err());
        assert_eq!(limit_name(&e), "max_alloc_bytes", "{src}");
    }
}

#[test]
fn map_of_map_is_bounded_by_the_depth_limit() {
    // TPL-4: `map('map', 'map', ...)` recursed natively once per level of
    // the value, past `max_depth`, until the stack overflowed (the whole
    // process aborts, so this test fails loudly on a regression). It used to
    // overflow even a 2 MiB stack in a debug build; now it stays within the
    // 1 MiB that docs/template.md promises for debug builds (about 350 KiB
    // measured).
    let names = "'map',".repeat(500);
    let src = format!(
        "{}{{{{ ns.l|map({names}'string')|list|length }}}}",
        nested_list(500)
    );
    let e = bounded(Duration::from_secs(30), 1 << 20, move || {
        render_err(&src, "{}")
    });
    assert_eq!(limit_name(&e), "max_depth");
    // A few levels are fine.
    assert_eq!(
        ok("{{ [[[1, 2]], [[3]]]|map('map', 'first')|list }}", "{}"),
        "[[1], [3]]"
    );
}

#[test]
fn break_does_not_bypass_the_iteration_budget() {
    // TPL-5: every `for` copied the whole list before its first iteration,
    // so `break` kept the iteration count low while each entry cost O(n).
    let src = "{% set big = range(1000000) %}{% for i in range(3000) %}\
               {% for x in big %}{% break %}{% endfor %}{% endfor %}ok";
    let out = bounded(Duration::from_secs(10), 8 << 20, move || ok(src, "{}"));
    assert_eq!(out, "ok");
    let src = "{% set big = range(1000000) %}{% for i in range(1000000) %}\
               {% for x in big %}{% break %}{% endfor %}{% endfor %}";
    let e = bounded(Duration::from_secs(10), 8 << 20, move || {
        render_err(src, "{}")
    });
    assert_eq!(limit_name(&e), "max_iterations");
}

#[test]
fn linear_operations_are_charged_to_the_work_budget() {
    // TPL-6: operations that walk a large value were free, so a million
    // iterations of them ran for hours under every other limit.
    assert_eq!(Limits::default().max_steps, Limits::DEFAULT_MAX_STEPS);
    assert_eq!(Limits::DEFAULT_MAX_STEPS, 100_000_000);
    for src in [
        "{% set s = 'x' * 10000000 %}{% for i in range(1000000) %}{% if 'y' in s %}{% endif %}{% endfor %}",
        "{% set s = 'x' * 10000000 %}{% for i in range(1000000) %}{{ s[5] }}{{ s|length }}{{ s.find('y') }}{{ s.count('y') }}{% set t = s[1:2] %}{% endfor %}",
        "{% set s = 'x' * 10000000 %}{% set t = 'x' * 10000000 %}{% for i in range(1000000) %}{% if s == t %}{% endif %}{% endfor %}",
        "{% set a = range(100000) %}{% set b = range(100000) %}{% for i in range(1000000) %}{% if a == b %}{% endif %}{% endfor %}",
    ] {
        let e = quick(move || render_err(src, "{}"));
        assert_eq!(limit_name(&e), "max_steps", "{src}");
    }
    // Plain evaluation counts too: a long expression in a long loop.
    let src = format!(
        "{{% for i in range(1000000) %}}{{% if u{} %}}{{% endif %}}{{% endfor %}}done",
        " or u".repeat(2000)
    );
    let tight = Limits {
        max_steps: 2_000_000,
        ..Limits::default()
    };
    let e = quick(move || render_with(&src, "{}", tight).unwrap_err());
    assert_eq!(limit_name(&e), "max_steps");
    // Comparing deeply shared structure is charged per value visited, not
    // per top-level item.
    let src = "{% set a = range(1000) %}{% set b = range(1000) %}{% set x = [a] * 1000 %}{% set y = [b] * 1000 %}\
               {% for i in range(1000) %}{% if x == y %}{% endif %}{% endfor %}";
    let e = quick(move || render_err(src, "{}"));
    assert_eq!(limit_name(&e), "max_steps");
}

#[test]
fn unpacking_and_slicing_a_huge_string_build_only_what_they_return() {
    // TPL-7: `{% set a, b = s %}` built one value per character before
    // checking the count (65 times the bytes charged), and slicing collected
    // every character first.
    let limits = Limits {
        max_alloc_bytes: 24 << 20,
        ..Limits::default()
    };
    for src in [
        "{% set s = 'x' * 20000000 %}{% set a, b = s %}",
        "{% set s = 'x' * 20000000 %}{% for a, b in [s] %}{% endfor %}",
    ] {
        let e = bounded(Duration::from_secs(2), 8 << 20, move || {
            render_with(src, "{}", limits).unwrap_err()
        });
        assert!(
            matches!(e.kind(), TemplateErrorKind::Type(m) if m.contains("20000000")),
            "{src}: {e}"
        );
    }
    let src = "{% set s = 'é' * 10000000 %}{{ s[0:1] }}{{ s[-1] }}{{ s[::-5000000] }}{{ s[3:5] }}";
    let out = quick(move || render_with(src, "{}", limits).unwrap());
    assert_eq!(out, "éééééé");
    // Iterating a string charges what the characters really cost.
    let tight = Limits {
        max_alloc_bytes: 48 << 20,
        ..Limits::default()
    };
    let e = quick(move || {
        render_with(
            "{% set s = 'x' * 1000000 %}{% for c in s %}{% endfor %}",
            "{}",
            tight,
        )
        .unwrap_err()
    });
    assert_eq!(limit_name(&e), "max_alloc_bytes");
}

#[test]
fn large_maps_scopes_and_argument_lists_stay_linear() {
    // TPL-8: maps, scopes and argument lists were vectors scanned per
    // lookup, so each of these was quadratic (tens of seconds).
    let n: usize = 100_000;
    let big = Limits {
        max_source_bytes: 4 << 20,
        ..Limits::default()
    };
    let names: Vec<String> = (0..n).map(|i| format!("a{i:x}")).collect();
    let params = format!(
        "{{% macro m({}) %}}{{{{ a0 }}}}{{{{ a{:x} }}}}{{% endmacro %}}{{{{ m(1, a{:x}=2) }}}}",
        names.join(","),
        n - 1,
        n - 1
    );
    let kwargs = format!(
        "{{{{ dict({})|length }}}}",
        names
            .iter()
            .map(|k| format!("{k}=1"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let literal = format!(
        "{{{{ {{{}}}|length }}}}",
        names
            .iter()
            .map(|k| format!("'{k}':1"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let sets: String = names
        .iter()
        .map(|k| format!("{{% set {k} = 1 %}}"))
        .collect::<String>()
        + "{{ a0 }}";
    let namespace = format!(
        "{{% set ns = namespace() %}}{}{{{{ ns.a0 }}}}",
        names
            .iter()
            .map(|k| format!("{{% set ns.{k} = 1 %}}"))
            .collect::<String>()
    );
    let count = n.to_string();
    let cases = vec![
        (params, "12".to_string()),
        (kwargs, count.clone()),
        (literal, count),
        (sets, "1".to_string()),
        (namespace, "1".to_string()),
    ];
    bounded(Duration::from_secs(20), 8 << 20, move || {
        for (src, want) in &cases {
            let start = Instant::now();
            assert_eq!(render_with(src, "{}", big).unwrap(), *want);
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "took {:?}",
                start.elapsed()
            );
        }
    });
    // Data-driven: a context map with many keys, read key by key and
    // compared whole.
    let entries: Vec<String> = (0..n).map(|i| format!("\"key{i:06}\":{i}")).collect();
    let ctx = format!("{{\"m\":{{{}}}}}", entries.join(","));
    let out = bounded(Duration::from_secs(20), 8 << 20, move || {
        ok(
            "{% set m2 = dict(m) %}{% set ns = namespace(s=0) %}{% for k in m %}{% set ns.s = ns.s + m[k] %}{% endfor %}\
             {{ ns.s }}{% for i in range(20) %}{% if m != m2 %}!{% endif %}{% endfor %}{{ m == m2 }}{{ 'key099999' in m }}",
            &ctx,
        )
    });
    assert_eq!(out, format!("{}TrueTrue", (n - 1) * n / 2));
}

#[test]
fn source_limit_default_is_sized_for_real_templates() {
    // TPL-14: parsing peaks at about 100 times the source, so the default
    // is a quarter of a megabyte (real chat templates are under 20 KB).
    assert_eq!(Limits::default().max_source_bytes, 256 << 10);
    let src = "x".repeat((256 << 10) + 1);
    assert_eq!(limit_name(&parse_err(&src)), "max_source_bytes");
    assert!(parse(&"x".repeat(256 << 10)).is_ok());
}

// ---- generative: no input panics ---------------------------------------

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const PIECES: &[&str] = &[
    "{{",
    "}}",
    "{%",
    "%}",
    "{#",
    "#}",
    "{{-",
    "-}}",
    "{%-",
    "-%}",
    "{%+",
    "+%}",
    " ",
    "\n",
    "\r\n",
    "\t",
    "if",
    "elif",
    "else",
    "endif",
    "for",
    "in",
    "endfor",
    "set",
    "endset",
    "macro",
    "endmacro",
    "raw",
    "endraw",
    "break",
    "continue",
    "not",
    "and",
    "or",
    "is",
    "defined",
    "none",
    "true",
    "false",
    "loop",
    "loop.index",
    "loop.last",
    "x",
    "xs",
    "m",
    "ns",
    "namespace()",
    "range(3)",
    "range(-2, 9, 2)",
    "'s'",
    "\"t\\n\"",
    "'\\u00e9'",
    "'unterminated",
    "0",
    "1",
    "-1",
    "2.5",
    "1e309",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    ",",
    ":",
    ".",
    "|",
    "=",
    "==",
    "!=",
    "<",
    ">=",
    "+",
    "-",
    "*",
    "/",
    "//",
    "%",
    "**",
    "~",
    "trim",
    "tojson",
    "length",
    "join",
    "selectattr('a')",
    "map(attribute='a')",
    "default('d')",
    "items",
    "list",
    "first",
    "last",
    "int",
    "string",
    "upper",
    "replace('a','b')",
    "indent(2)",
    "sort",
    "unique",
    "reverse",
    "sum",
    "round",
    "is string",
    "is mapping",
    "is none",
    "is odd",
    "raise_exception('r')",
    ".strip()",
    ".split(',')",
    ".items()",
    ".get('a')",
    "[0]",
    "[1:]",
    "[::-1]",
    "[-1]",
    "x if xs else m",
    "é",
    "😀",
    // Extremes: the largest literals, NaN and infinities, empty and huge
    // repetition, sorting mixed kinds, recursion through `map`.
    "170141183460469231731687303715884105727",
    "-170141183460469231731687303715884105727 - 1",
    "18446744073709551615",
    "100000000000000000",
    "'nan'|float",
    "1e999",
    "1e999 - 1e999",
    "[] *",
    "* []",
    "[1, 'a', 2.5, none]",
    "[[1], [nan], ['x']]",
    "|sort",
    "|dictsort",
    "sort(attribute='a')",
    "map('map', 'map', 'first')",
    "map('sort')",
    "map(attribute='a.b', default='d')",
    "center(20000000000000000000)",
    "round(-400)",
    ".split(' ', 18446744073709551615)",
    ".rsplit(' ', 18446744073709551615)",
    "[1::170141183460469231731687303715884105727]",
    "[::-170141183460469231731687303715884105727]",
    "range(-170141183460469231731687303715884105727, 170141183460469231731687303715884105727)",
    "range(5, 1, -170141183460469231731687303715884105727 - 1)",
    "0 ** -1",
    "01",
    "1__0",
    "'\\N{BULLET}'",
    "{{ ([] * 100000000000000000)|length }}",
    "{{ ('nan'|float, 3, 1)|sort }}",
    "{% set a, b = 'xy' %}",
    "{% set a, b = 'x' * 1000 %}",
    "{% macro g(n) %}{{ x }}{{ g(n - 1) if n else '' }}{% endmacro %}",
    "{% for x in xs %}{% macro h() %}{{ x }}{{ loop.index }}{% endmacro %}{{ h() }}{% endfor %}",
    "{% for i in range(9) %}{% for y in range(100000) %}{% break %}{% endfor %}{% endfor %}",
];

fn random_source(rng: &mut Rng) -> String {
    let n = 1 + rng.below(40);
    (0..n).map(|_| PIECES[rng.below(PIECES.len())]).collect()
}

fn random_value(rng: &mut Rng, depth: usize) -> Value {
    match rng.below(if depth > 3 { 6 } else { 9 }) {
        0 => Value::Null,
        1 => Value::Bool(rng.below(2) == 0),
        2 => Value::Int(rng.next() as i64 >> rng.below(64)),
        3 => Value::UInt(rng.next()),
        4 => Value::Float(f64::from_bits(rng.next())),
        5 => Value::Str(["", "a", "é😀", "{{ x }}", " pad "][rng.below(5)].to_string()),
        6 => Value::Bytes(vec![1, 2, 255]),
        7 => Value::Array(
            (0..rng.below(4))
                .map(|_| random_value(rng, depth + 1))
                .collect(),
        ),
        _ => Value::Object(
            (0..rng.below(4))
                .map(|i| {
                    (
                        ["a", "role", "x", "a"][i].to_string(),
                        random_value(rng, depth + 1),
                    )
                })
                .collect(),
        ),
    }
}

fn small_limits() -> Options {
    Options::default().limits(Limits {
        max_iterations: 5_000,
        max_calls: 5_000,
        max_output_bytes: 1 << 16,
        max_alloc_bytes: 1 << 20,
        max_steps: 200_000,
        ..Limits::default()
    })
}

/// Renders, and fails if a single render takes longer than any template
/// under the small limits ever should.
fn render_bounded(t: &Template, ctx: &Value) {
    let start = Instant::now();
    let _ = t.render(ctx);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?} took {:?}",
        t.source(),
        start.elapsed()
    );
}

#[test]
fn arbitrary_templates_and_contexts_never_panic() {
    bounded(Duration::from_secs(300), 8 << 20, arbitrary_templates);
}

fn arbitrary_templates() {
    let iters: usize = std::env::var("FERROLY_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000);
    let opts = small_limits();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut parsed = 0;
    for _ in 0..iters {
        let src = random_source(&mut rng);
        let mut entries = vec![
            ("x".to_string(), random_value(&mut rng, 0)),
            ("xs".to_string(), random_value(&mut rng, 0)),
            ("m".to_string(), random_value(&mut rng, 0)),
        ];
        entries.truncate(1 + rng.below(3));
        let ctx = Value::Object(entries);
        if let Ok(t) = Template::parse(&src, &opts) {
            parsed += 1;
            render_bounded(&t, &ctx);
        }
    }
    assert!(
        parsed > iters / 50,
        "generator produced too few valid templates ({parsed})"
    );
}

#[test]
fn mutated_real_templates_never_panic() {
    bounded(Duration::from_secs(300), 8 << 20, mutated_real_templates);
}

fn mutated_real_templates() {
    let doc = json::from_str(include_str!("template_data/chat_fixtures.json")).unwrap();
    let templates: Vec<String> = doc
        .get("templates")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("template")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let ctx = doc.get("cases").and_then(Value::as_array).unwrap()[0]
        .get("context")
        .cloned()
        .unwrap();
    let opts = small_limits();
    let mut rng = Rng(42);
    for _ in 0..1500 {
        let base = &templates[rng.below(templates.len())];
        let mut bytes = base.as_bytes().to_vec();
        for _ in 0..1 + rng.below(4) {
            if bytes.is_empty() {
                break;
            }
            let at = rng.below(bytes.len());
            match rng.below(3) {
                0 => {
                    bytes.remove(at);
                }
                1 => bytes.insert(at, b"{}%#-+'\"()[]|.~\n "[rng.below(17)]),
                _ => {
                    let end = (at + rng.below(30)).min(bytes.len());
                    let chunk = bytes[at..end].to_vec();
                    let to = rng.below(bytes.len());
                    bytes.splice(to..to, chunk);
                }
            }
        }
        let src = String::from_utf8_lossy(&bytes);
        if let Ok(t) = Template::parse(&src, &opts) {
            render_bounded(&t, &ctx);
        }
    }
}

/// Replays the committed `fuzz/seeds/template_jinja` corpus under the same
/// checks the `template_jinja` fuzz target makes, so it runs on stable too.
#[test]
fn fuzz_seed_corpus_replays_cleanly() {
    bounded(Duration::from_secs(120), 8 << 20, fuzz_seed_corpus);
}

fn fuzz_seed_corpus() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fuzz")
        .join("seeds")
        .join("template_jinja");
    let contexts = [
        r#"{"messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"Hi {{ x }}"},{"role":"assistant","content":"Hello"}],"add_generation_prompt":true,"bos_token":"<s>","eos_token":"</s>"}"#,
        r#"{"messages":[{"role":"user","content":"weather?"},{"role":"assistant","content":"","tool_calls":[{"id":"call12345","function":{"name":"w","arguments":{"city":"Paris"}}}]},{"role":"tool","content":"18"}],"tools":[{"type":"function","function":{"name":"w","parameters":{"type":"object"}}}]}"#,
        r#"{"x":[1,"two",{"k":"v"},null,true,2.5,-7],"m":{"a":1,"items":[3]},"s":"é😀 {% raw %}"}"#,
        r#"{}"#,
    ];
    let opts = Options::default().limits(Limits {
        max_iterations: 20_000,
        max_calls: 20_000,
        max_output_bytes: 1 << 16,
        max_alloc_bytes: 4 << 20,
        max_steps: 2_000_000,
        ..Limits::default()
    });
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("seed directory") {
        let data = std::fs::read(entry.expect("seed").path()).expect("seed bytes");
        let Some((&selector, rest)) = data.split_first() else {
            continue;
        };
        let source = String::from_utf8_lossy(rest);
        let ctx = json::from_str(contexts[usize::from(selector) % contexts.len()]).unwrap();
        if let Ok(t) = Template::parse(&source, &opts) {
            let start = Instant::now();
            if let Ok(out) = t.render(&ctx) {
                assert!(out.len() <= 1 << 16);
                assert_eq!(t.render(&ctx).ok().as_deref(), Some(out.as_str()));
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{source:?} took {:?}",
                start.elapsed()
            );
        }
        seen += 1;
    }
    assert!(seen >= 40, "only {seen} seeds");
}
