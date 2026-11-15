#![cfg(feature = "grpc")]
//! Fuzz-lite for gRPC's wire decoding: seeded random and mutated inputs run
//! through the same entry point as `fuzz/fuzz_targets/grpc_frames.rs`
//! (framing, status trailers, `grpc-timeout`, metadata), plus a decoder
//! invariant: a valid stream decodes identically at every chunking.
//! `FERROLY_FUZZ_ITERS` raises the iteration count for a longer soak.

use ferroly::grpc::FrameDecoder;

/// xorshift64*: deterministic, so a failure reproduces from the seed.
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

fn iters() -> usize {
    std::env::var("FERROLY_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000)
}

const SEEDS: &[&[u8]] = &[
    b"\x03\x00\x00\x00\x02hi\x00\x00\x00\x00\x00",
    b"\x00\xff\xff\xff\xff",
    b"\x01\x00\x00\x00\x01\x08",
    b"grpc-message: bad%20news%0A%zz\nx-trace-bin: AAEC/w\ny-bin: aGk=, aGVsbG8\n",
    b"99999999H",
    b"100m",
    b"",
];

#[test]
fn random_and_mutated_inputs_never_panic() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for seed in SEEDS {
        ferroly::grpc::__fuzz(seed);
    }
    for _ in 0..iters() {
        let mut input = SEEDS[rng.below(SEEDS.len())].to_vec();
        for _ in 0..rng.below(8) {
            match rng.below(4) {
                0 if !input.is_empty() => {
                    let i = rng.below(input.len());
                    input[i] = rng.next() as u8;
                }
                1 => {
                    let i = rng.below(input.len() + 1);
                    input.insert(i, rng.next() as u8);
                }
                2 if !input.is_empty() => {
                    let i = rng.below(input.len());
                    input.remove(i);
                }
                _ => {
                    for _ in 0..rng.below(32) {
                        input.push(rng.next() as u8);
                    }
                }
            }
        }
        ferroly::grpc::__fuzz(&input);
    }
}

#[test]
fn valid_streams_decode_the_same_at_any_chunking() {
    let mut rng = Rng(42);
    for _ in 0..iters() / 10 {
        let bodies: Vec<Vec<u8>> = (0..rng.below(5))
            .map(|_| (0..rng.below(200)).map(|_| rng.next() as u8).collect())
            .collect();
        let mut stream = Vec::new();
        for b in &bodies {
            stream.push(0);
            stream.extend_from_slice(&(b.len() as u32).to_be_bytes());
            stream.extend_from_slice(b);
        }
        let mut decoder = FrameDecoder::new(256);
        let mut got = Vec::new();
        let mut rest = &stream[..];
        while !rest.is_empty() {
            let n = 1 + rng.below(rest.len());
            decoder.push(&rest[..n]);
            rest = &rest[n..];
            while let Some(frame) = decoder.next_frame().unwrap() {
                got.push(frame);
            }
        }
        decoder.finish().unwrap();
        assert_eq!(got, bodies);
    }
}
