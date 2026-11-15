#![no_main]
//! The Jinja2-subset engine (`ferroly::template`) must never panic, hang or
//! exceed its limits on arbitrary template text and context. cargo-fuzz builds
//! with debug assertions and overflow checks unless run with `-O`, so
//! unchecked arithmetic on template-supplied integers is a crash, and a render
//! that outlasts `TIME_CAP` under a two-million-step budget is one too. The
//! first byte picks one of a few contexts (chat messages, tool definitions,
//! odd values); the rest is the template source.

use std::time::{Duration, Instant};

use ferroly::codec::json;
use ferroly::template::{Limits, Options, Template};
use libfuzzer_sys::fuzz_target;

const CONTEXTS: [&str; 4] = [
    r#"{"messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"Hi {{ x }}"},{"role":"assistant","content":"Hello"}],"add_generation_prompt":true,"bos_token":"<s>","eos_token":"</s>"}"#,
    r#"{"messages":[{"role":"user","content":"weather?"},{"role":"assistant","content":"","tool_calls":[{"id":"call12345","function":{"name":"w","arguments":{"city":"Paris"}}}]},{"role":"tool","content":"18"}],"tools":[{"type":"function","function":{"name":"w","parameters":{"type":"object"}}}]}"#,
    r#"{"x":[1,"two",{"k":"v"},null,true,2.5,-7],"m":{"a":1,"items":[3]},"s":"é😀 {% raw %}"}"#,
    r#"{}"#,
];

const OUTPUT_CAP: usize = 1 << 16;
const TIME_CAP: Duration = Duration::from_secs(10);

fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    let source = String::from_utf8_lossy(rest);
    let context = json::from_str(CONTEXTS[usize::from(selector) % CONTEXTS.len()]).unwrap();
    let options = Options::default().limits(Limits {
        max_iterations: 20_000,
        max_calls: 20_000,
        max_output_bytes: OUTPUT_CAP,
        max_alloc_bytes: 4 << 20,
        max_steps: 2_000_000,
        ..Limits::default()
    });
    if let Ok(template) = Template::parse(&source, &options) {
        let start = Instant::now();
        if let Ok(out) = template.render(&context) {
            assert!(out.len() <= OUTPUT_CAP, "output cap exceeded: {}", out.len());
            // Rendering is deterministic.
            assert_eq!(template.render(&context).ok().as_deref(), Some(out.as_str()));
        }
        // The limits bound the work, not only its counts: two million steps
        // take milliseconds, so a render that runs for seconds has found
        // work no budget charges.
        assert!(
            start.elapsed() < TIME_CAP,
            "render took {:?}",
            start.elapsed()
        );
    }
});
