#![no_main]
//! Fuzz gRPC's wire decoding: arbitrary bytes are split into length-prefixed
//! messages (at a data-dependent chunk boundary, under a 1 KiB limit), and
//! read as header text for the `grpc-status` / `grpc-message` /
//! `grpc-status-details-bin` trailers, `grpc-timeout` and metadata
//! (including base64 `-bin` values). Nothing may panic; a decoded frame never
//! exceeds the limit; a written status and a formatted timeout read back.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ferroly::grpc::__fuzz(data);
});
