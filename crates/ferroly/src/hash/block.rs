//! A fixed-size block buffer shared by the Merkle–Damgård hashers.

/// Accumulates input into `B`-byte blocks and hands each full block to a
/// compression function.
#[derive(Clone)]
pub(crate) struct BlockBuffer<const B: usize> {
    buf: [u8; B],
    len: usize,
}

impl<const B: usize> BlockBuffer<B> {
    pub(crate) fn new() -> Self {
        BlockBuffer {
            buf: [0; B],
            len: 0,
        }
    }

    /// Absorbs `data`, calling `compress` once per completed block.
    pub(crate) fn update(&mut self, mut data: &[u8], mut compress: impl FnMut(&[u8; B])) {
        if self.len > 0 {
            let take = (B - self.len).min(data.len());
            self.buf[self.len..self.len + take].copy_from_slice(&data[..take]);
            self.len += take;
            data = &data[take..];
            if self.len < B {
                return;
            }
            compress(&self.buf);
            self.len = 0;
        }
        let mut blocks = data.chunks_exact(B);
        for block in &mut blocks {
            let mut full = [0u8; B];
            full.copy_from_slice(block);
            compress(&full);
        }
        let rest = blocks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.len = rest.len();
    }

    /// Applies Merkle–Damgård padding: a `0x80` byte, zeros, then `length`
    /// (the already-encoded message bit length) at the end of the final block.
    pub(crate) fn finish(&mut self, length: &[u8], mut compress: impl FnMut(&[u8; B])) {
        self.buf[self.len] = 0x80;
        self.buf[self.len + 1..].fill(0);
        if self.len + 1 > B - length.len() {
            compress(&self.buf);
            self.buf.fill(0);
        }
        self.buf[B - length.len()..].copy_from_slice(length);
        compress(&self.buf);
        self.len = 0;
    }
}
