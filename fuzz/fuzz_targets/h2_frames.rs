#![no_main]
//! Fuzz the HTTP/2 frame decoder and the HPACK decoder: arbitrary bytes are
//! split into frames (9-byte header + payload) and every header block is fed
//! through one HPACK decoder, then the whole input is decoded as a single
//! header block. Every frame-level rule violation and every malformed block
//! must be a plain error; the dynamic table, the decoded header list and the
//! per-frame allocations stay within their caps.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ferroly::http::__fuzz_h2_frames(data);
});
