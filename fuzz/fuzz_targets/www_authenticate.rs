#![no_main]
//! Fuzz the OCI adapter's `WWW-Authenticate` challenge parser: arbitrary
//! header text (quoted-pairs before multi-byte characters, unterminated
//! quotes, stray `=` and `,`) must never panic, must stay within the
//! challenge and parameter caps, and must never produce more text than it
//! was given.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ferroly::store::oci::__fuzz_challenges(data);
});
