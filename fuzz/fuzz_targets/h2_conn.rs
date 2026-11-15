#![no_main]
//! Fuzz the HTTP/2 connection state machine: the server engine runs over an
//! in-memory pipe and the input is everything the client sends after the
//! preface (frames in any order, on streams in any state, with buffered and
//! streaming handlers behind them). The connection must end with a clean
//! close, a stream error or a GOAWAY — never a panic, a hang, or a task that
//! outlives it.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ferroly::http::__fuzz_h2_connection(data);
});
