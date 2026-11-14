#![no_main]
//! The prompt-template renderer must never panic, and must bound its output,
//! on arbitrary template text.

use ferroly::genai::template::{RenderOptions, Template};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let vars = ferroly::codec::json::from_str(
        r#"{"name":"Ada","on":true,"xs":[1,"two",{"k":"v"}],"m":{"a":1}}"#,
    )
    .unwrap();
    if let Ok(template) = Template::parse(&text) {
        let _ = template.render_with(&vars, &RenderOptions::strict().max_output_bytes(1 << 16));
        let _ = template.render_with(
            &vars,
            &RenderOptions::lenient()
                .allow_root(true)
                .max_output_bytes(1 << 16),
        );
    }
});
