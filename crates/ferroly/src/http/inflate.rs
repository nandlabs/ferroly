//! A bounded, streaming INFLATE decoder (RFC 1951) with gzip (RFC 1952) and
//! zlib (RFC 1950) framing, used to decode `Content-Encoding: gzip` /
//! `deflate` response bodies when the client opts in.
//!
//! The decoder is incremental: [`Decoder::feed`] appends compressed bytes and
//! [`Decoder::decode`] returns at most a bounded amount of output per call, so
//! a small compressed input can never force a large allocation in one step
//! (the caller enforces the overall size cap). Every malformed input is an
//! error — invalid block types, over-subscribed or incomplete Huffman codes,
//! distances that reach before the start of the output, stored-block length
//! mismatches, and CRC-32 / Adler-32 / length trailer mismatches.

use super::HttpError;

/// The framing around the DEFLATE stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// gzip members (`Content-Encoding: gzip`). Concatenated members are
    /// decoded one after another.
    Gzip,
    /// `Content-Encoding: deflate`: a zlib stream per the HTTP specification,
    /// or — because some servers send it — a bare DEFLATE stream. The two are
    /// told apart by the zlib header check.
    Deflate,
}

/// Maximum size of a gzip header (which may carry a file name and comment).
const MAX_GZIP_HEADER: usize = 64 * 1024;
/// DEFLATE's history window.
const WINDOW: usize = 32 * 1024;
/// Longest Huffman code.
const MAX_BITS: usize = 15;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order in which code-length code lengths are stored in a dynamic header.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn corrupt(what: &'static str) -> HttpError {
    HttpError::Protocol(format!("corrupt compressed body: {what}"))
}

/// The bit-level read position. `Copy`, so a step that runs out of input can
/// be rolled back to where it started and retried once more bytes arrive.
#[derive(Debug, Clone, Copy, Default)]
struct Bits {
    /// Bits loaded from the input but not yet consumed (LSB first).
    buf: u64,
    /// Number of valid bits in `buf`.
    cnt: u32,
    /// Index of the next input byte to load.
    pos: usize,
}

impl Bits {
    /// Loads bytes until at least `n` bits are buffered; `false` if the input
    /// ran out first.
    fn need(&mut self, input: &[u8], n: u32) -> bool {
        while self.cnt < n {
            match input.get(self.pos) {
                Some(&b) => {
                    self.buf |= u64::from(b) << self.cnt;
                    self.cnt += 8;
                    self.pos += 1;
                }
                None => return false,
            }
        }
        true
    }

    /// Consumes `n` bits (`n <= 32`), or `None` if the input ran out.
    fn take(&mut self, input: &[u8], n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if !self.need(input, n) {
            return None;
        }
        let value = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Some(value)
    }

    /// Discards the bits up to the next byte boundary.
    fn align(&mut self) {
        let drop = self.cnt % 8;
        self.buf >>= drop;
        self.cnt -= drop;
    }
}

/// A canonical Huffman code: how many codes there are of each length, and the
/// symbols ordered by code.
#[derive(Debug, Clone)]
struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: Vec<u16>,
}

impl Huffman {
    /// Builds the code for `lengths` (one length per symbol; zero = unused).
    /// Returns the code and how many codes are left unassigned: zero for a
    /// complete code, positive for an incomplete one. An over-subscribed set
    /// of lengths is an error.
    fn new(lengths: &[u8]) -> Result<(Huffman, i32), HttpError> {
        let mut count = [0u16; MAX_BITS + 1];
        for &len in lengths {
            count[usize::from(len)] += 1;
        }
        let mut symbol = vec![0u16; lengths.len()];
        if usize::from(count[0]) == lengths.len() {
            // No codes at all: complete as far as the format is concerned, but
            // any attempt to decode with it fails.
            return Ok((Huffman { count, symbol }, 0));
        }
        let mut left: i32 = 1;
        for &n in &count[1..] {
            left <<= 1;
            left -= i32::from(n);
            if left < 0 {
                return Err(corrupt("over-subscribed Huffman code"));
            }
        }
        let mut offs = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offs[len + 1] = offs[len] + count[len];
        }
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = &mut offs[usize::from(len)];
                symbol[usize::from(*slot)] = sym as u16;
                *slot += 1;
            }
        }
        Ok((Huffman { count, symbol }, left))
    }

    /// Decodes one symbol. `Ok(None)` means the input ran out (nothing is
    /// consumed in that case).
    fn decode(&self, bits: &mut Bits, input: &[u8]) -> Result<Option<u16>, HttpError> {
        // Best effort: near the end of the stream fewer bits may remain than
        // the longest code, and a shorter code can still be decoded.
        bits.need(input, MAX_BITS as u32);
        let mut buf = bits.buf;
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            if len as u32 > bits.cnt {
                return Ok(None);
            }
            code |= (buf & 1) as i32;
            buf >>= 1;
            let count = i32::from(self.count[len]);
            if code - count < first {
                bits.buf >>= len;
                bits.cnt -= len as u32;
                return Ok(Some(self.symbol[(index + (code - first)) as usize]));
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(corrupt("invalid Huffman code"))
    }
}

/// The fixed literal/length and distance codes of a BTYPE=01 block.
fn fixed_codes() -> (Huffman, Huffman) {
    let mut lengths = [0u8; 288];
    for (sym, len) in lengths.iter_mut().enumerate() {
        *len = match sym {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let (lit, _) = Huffman::new(&lengths).expect("fixed literal code is valid");
    let (dist, _) = Huffman::new(&[5u8; 30]).expect("fixed distance code is valid");
    (lit, dist)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrapper {
    Gzip,
    Zlib,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the gzip / zlib header of a member.
    Header,
    /// Before a block header.
    Block,
    /// Inside a stored block with this many bytes left.
    Stored(usize),
    /// Inside a Huffman-coded block.
    Codes,
    /// After the final block, before the checksum trailer.
    Trailer,
    /// After a complete member.
    Done,
    /// After the stream: any further input is ignored.
    Ignore,
}

/// A streaming gzip / zlib / raw-DEFLATE decoder.
#[derive(Debug)]
pub(crate) struct Decoder {
    format: Format,
    wrapper: Wrapper,
    state: State,
    /// Compressed bytes not yet consumed (consumed ones are dropped on the
    /// next [`feed`](Decoder::feed)).
    input: Vec<u8>,
    bits: Bits,
    last_block: bool,
    lit: Huffman,
    dist: Huffman,
    /// Recent output: the history window followed by bytes not yet returned.
    window: Vec<u8>,
    /// Start of the bytes in `window` not yet returned to the caller.
    emitted: usize,
    /// Start of the bytes in `window` not yet folded into the checksum.
    hashed: usize,
    crc: u32,
    adler: (u32, u32),
    /// Output length of the current member, modulo 2^32 (gzip ISIZE).
    member_len: u32,
    members: usize,
    fed: bool,
}

impl Decoder {
    /// A decoder for `format`.
    pub(crate) fn new(format: Format) -> Self {
        let (lit, dist) = fixed_codes();
        Self {
            format,
            wrapper: Wrapper::Raw,
            state: State::Header,
            input: Vec::new(),
            bits: Bits::default(),
            last_block: false,
            lit,
            dist,
            window: Vec::new(),
            emitted: 0,
            hashed: 0,
            crc: !0,
            adler: (1, 0),
            member_len: 0,
            members: 0,
            fed: false,
        }
    }

    /// The decoder for a `Content-Encoding` value, if it is one this module
    /// handles (`gzip`, `x-gzip`, `deflate`).
    pub(crate) fn for_content_encoding(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("gzip") || value.eq_ignore_ascii_case("x-gzip") {
            Some(Self::new(Format::Gzip))
        } else if value.eq_ignore_ascii_case("deflate") {
            Some(Self::new(Format::Deflate))
        } else {
            None
        }
    }

    /// Appends compressed input.
    pub(crate) fn feed(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.fed = true;
        if self.state == State::Ignore {
            return;
        }
        // Everything before the read position has been consumed for good:
        // steps that run out of input roll the position back first.
        self.input.drain(..self.bits.pos);
        self.bits.pos = 0;
        self.input.extend_from_slice(data);
    }

    /// Whether the input so far forms a complete stream: at least one whole
    /// member with a verified trailer and no partial member after it. An
    /// entirely empty input also counts as complete (an empty body).
    pub(crate) fn is_complete(&self) -> bool {
        if !self.fed {
            return true;
        }
        match self.state {
            State::Done | State::Ignore => true,
            State::Header => self.members > 0 && self.bits.pos == self.input.len(),
            _ => false,
        }
    }

    /// Decodes as much as the buffered input allows, returning roughly
    /// `limit` bytes at most (never more than `limit + 258`). An empty result
    /// means more input is needed, or the stream is finished.
    pub(crate) fn decode(&mut self, limit: usize) -> Result<Vec<u8>, HttpError> {
        let limit = limit.max(1);
        while self.window.len() - self.emitted < limit {
            let progressed = match self.state {
                State::Header => self.read_header()?,
                State::Block => self.read_block_header()?,
                State::Stored(left) => self.copy_stored(left, limit),
                State::Codes => self.inflate_codes(limit)?,
                State::Trailer => self.read_trailer()?,
                State::Done => self.after_member(),
                State::Ignore => false,
            };
            if !progressed {
                break;
            }
        }
        self.checksum();
        let out = self.window[self.emitted..].to_vec();
        if self.window.len() > 2 * WINDOW {
            let cut = self.window.len() - WINDOW;
            self.window.drain(..cut);
        }
        self.emitted = self.window.len();
        self.hashed = self.window.len();
        Ok(out)
    }

    /// Folds the output produced since the last call into the checksum.
    fn checksum(&mut self) {
        let fresh = &self.window[self.hashed..];
        match self.wrapper {
            Wrapper::Gzip => self.crc = crc32_update(self.crc, fresh),
            Wrapper::Zlib => self.adler = adler32_update(self.adler, fresh),
            Wrapper::Raw => {}
        }
        self.member_len = self.member_len.wrapping_add(fresh.len() as u32);
        self.hashed = self.window.len();
    }

    fn read_header(&mut self) -> Result<bool, HttpError> {
        let data = &self.input[self.bits.pos..];
        match self.format {
            Format::Gzip => {
                let Some(len) = gzip_header_len(data)? else {
                    if data.len() > MAX_GZIP_HEADER {
                        return Err(corrupt("gzip header too large"));
                    }
                    return Ok(false);
                };
                self.bits.pos += len;
                self.wrapper = Wrapper::Gzip;
            }
            Format::Deflate => {
                if data.len() < 2 {
                    return Ok(false);
                }
                let (cmf, flg) = (data[0], data[1]);
                let is_zlib = cmf & 0x0f == 8
                    && cmf >> 4 <= 7
                    && (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0;
                if is_zlib {
                    if flg & 0x20 != 0 {
                        return Err(corrupt("zlib preset dictionary is not supported"));
                    }
                    self.bits.pos += 2;
                    self.wrapper = Wrapper::Zlib;
                } else {
                    self.wrapper = Wrapper::Raw;
                }
            }
        }
        self.crc = !0;
        self.adler = (1, 0);
        self.member_len = 0;
        self.last_block = false;
        self.state = State::Block;
        Ok(true)
    }

    fn read_block_header(&mut self) -> Result<bool, HttpError> {
        // The whole block header is one step: if the input runs out anywhere
        // inside it, roll back and parse it again when more arrives.
        let start = self.bits;
        match self.parse_block_header() {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.bits = start;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn parse_block_header(&mut self) -> Result<bool, HttpError> {
        let input = &self.input[..];
        let bits = &mut self.bits;
        let Some(last) = bits.take(input, 1) else {
            return Ok(false);
        };
        let Some(kind) = bits.take(input, 2) else {
            return Ok(false);
        };
        match kind {
            0 => {
                bits.align();
                let Some(len) = bits.take(input, 16) else {
                    return Ok(false);
                };
                let Some(nlen) = bits.take(input, 16) else {
                    return Ok(false);
                };
                if len != !nlen & 0xffff {
                    return Err(corrupt("stored block length check failed"));
                }
                self.state = State::Stored(len as usize);
            }
            1 => {
                let (lit, dist) = fixed_codes();
                self.lit = lit;
                self.dist = dist;
                self.state = State::Codes;
            }
            2 => {
                let Some(nlen) = bits.take(input, 5) else {
                    return Ok(false);
                };
                let Some(ndist) = bits.take(input, 5) else {
                    return Ok(false);
                };
                let Some(ncode) = bits.take(input, 4) else {
                    return Ok(false);
                };
                let (nlen, ndist, ncode) =
                    (nlen as usize + 257, ndist as usize + 1, ncode as usize + 4);
                if nlen > 286 || ndist > 30 {
                    return Err(corrupt("too many length or distance codes"));
                }
                let mut lengths = [0u8; 286 + 30];
                for &slot in CLEN_ORDER.iter().take(ncode) {
                    let Some(len) = bits.take(input, 3) else {
                        return Ok(false);
                    };
                    lengths[slot] = len as u8;
                }
                let (clen, left) = Huffman::new(&lengths[..19])?;
                if left != 0 {
                    return Err(corrupt("incomplete code-length code"));
                }
                lengths = [0u8; 286 + 30];
                let mut index = 0usize;
                while index < nlen + ndist {
                    let Some(sym) = clen.decode(bits, input)? else {
                        return Ok(false);
                    };
                    if sym < 16 {
                        lengths[index] = sym as u8;
                        index += 1;
                        continue;
                    }
                    let (value, base, extra) = match sym {
                        16 => {
                            if index == 0 {
                                return Err(corrupt("length repeat with no previous length"));
                            }
                            (lengths[index - 1], 3, 2)
                        }
                        17 => (0, 3, 3),
                        _ => (0, 11, 7),
                    };
                    let Some(more) = bits.take(input, extra) else {
                        return Ok(false);
                    };
                    let repeat = base + more as usize;
                    if index + repeat > nlen + ndist {
                        return Err(corrupt("code lengths overflow the table"));
                    }
                    for slot in &mut lengths[index..index + repeat] {
                        *slot = value;
                    }
                    index += repeat;
                }
                if lengths[256] == 0 {
                    return Err(corrupt("no end-of-block code"));
                }
                // An incomplete code is only valid when it has a single code.
                let (lit, left) = Huffman::new(&lengths[..nlen])?;
                if left != 0 && nlen != usize::from(lit.count[0] + lit.count[1]) {
                    return Err(corrupt("incomplete literal/length code"));
                }
                let (dist, left) = Huffman::new(&lengths[nlen..nlen + ndist])?;
                if left != 0 && ndist != usize::from(dist.count[0] + dist.count[1]) {
                    return Err(corrupt("incomplete distance code"));
                }
                self.lit = lit;
                self.dist = dist;
                self.state = State::Codes;
            }
            _ => return Err(corrupt("invalid block type")),
        }
        self.last_block = last == 1;
        Ok(true)
    }

    fn end_of_block(&mut self) {
        self.state = if self.last_block {
            State::Trailer
        } else {
            State::Block
        };
    }

    fn copy_stored(&mut self, left: usize, limit: usize) -> bool {
        if left == 0 {
            self.end_of_block();
            return true;
        }
        // After the LEN/NLEN words the bit buffer is empty, so stored bytes
        // come straight from the input.
        debug_assert_eq!(self.bits.cnt, 0);
        let room = limit - (self.window.len() - self.emitted);
        let available = self.input.len() - self.bits.pos;
        let n = left.min(available).min(room);
        if n == 0 {
            return false;
        }
        let from = self.bits.pos;
        self.window.extend_from_slice(&self.input[from..from + n]);
        self.bits.pos += n;
        self.state = State::Stored(left - n);
        true
    }

    fn inflate_codes(&mut self, limit: usize) -> Result<bool, HttpError> {
        let input = &self.input[..];
        loop {
            if self.window.len() - self.emitted >= limit {
                return Ok(true);
            }
            // One symbol (a literal, or a whole length/distance pair) is one
            // step, rolled back if the input runs out part-way through.
            let start = self.bits;
            let Some(sym) = self.lit.decode(&mut self.bits, input)? else {
                self.bits = start;
                return Ok(false);
            };
            match sym {
                0..=255 => self.window.push(sym as u8),
                256 => {
                    self.end_of_block();
                    return Ok(true);
                }
                257..=285 => {
                    let i = usize::from(sym) - 257;
                    let Some(extra) = self.bits.take(input, u32::from(LENGTH_EXTRA[i])) else {
                        self.bits = start;
                        return Ok(false);
                    };
                    let len = usize::from(LENGTH_BASE[i]) + extra as usize;
                    let Some(dsym) = self.dist.decode(&mut self.bits, input)? else {
                        self.bits = start;
                        return Ok(false);
                    };
                    let d = usize::from(dsym);
                    if d >= 30 {
                        return Err(corrupt("invalid distance code"));
                    }
                    let Some(extra) = self.bits.take(input, u32::from(DIST_EXTRA[d])) else {
                        self.bits = start;
                        return Ok(false);
                    };
                    let dist = usize::from(DIST_BASE[d]) + extra as usize;
                    if dist > self.window.len() {
                        return Err(corrupt("distance reaches before the start of the output"));
                    }
                    // Byte by byte: the source and destination may overlap.
                    let from = self.window.len() - dist;
                    for k in 0..len {
                        let b = self.window[from + k];
                        self.window.push(b);
                    }
                }
                _ => return Err(corrupt("invalid literal/length code")),
            }
        }
    }

    fn read_trailer(&mut self) -> Result<bool, HttpError> {
        self.checksum();
        let input = &self.input[..];
        let start = self.bits;
        self.bits.align();
        match self.wrapper {
            Wrapper::Gzip => {
                let (Some(crc), Some(len)) = (self.bits.take(input, 32), self.bits.take(input, 32))
                else {
                    self.bits = start;
                    return Ok(false);
                };
                if crc != !self.crc {
                    return Err(corrupt("gzip CRC-32 mismatch"));
                }
                if len != self.member_len {
                    return Err(corrupt("gzip length mismatch"));
                }
            }
            Wrapper::Zlib => {
                let mut expected = 0u32;
                for _ in 0..4 {
                    let Some(byte) = self.bits.take(input, 8) else {
                        self.bits = start;
                        return Ok(false);
                    };
                    expected = expected << 8 | byte;
                }
                if expected != (self.adler.1 << 16 | self.adler.0) {
                    return Err(corrupt("zlib Adler-32 mismatch"));
                }
            }
            Wrapper::Raw => {}
        }
        self.members += 1;
        self.state = State::Done;
        Ok(true)
    }

    /// After a member: gzip allows another member to follow; anything else
    /// (and anything that is not a gzip member) is ignored.
    fn after_member(&mut self) -> bool {
        let rest = &self.input[self.bits.pos..];
        if self.wrapper == Wrapper::Gzip && self.bits.cnt == 0 {
            match rest {
                [] | [0x1f] => return false,
                [0x1f, 0x8b, ..] => {
                    self.state = State::Header;
                    return true;
                }
                _ => {}
            }
        }
        self.bits.pos = self.input.len();
        self.state = State::Ignore;
        false
    }
}

/// The length of the gzip member header at the start of `data`, or `None`
/// when more bytes are needed to tell.
fn gzip_header_len(data: &[u8]) -> Result<Option<usize>, HttpError> {
    if data.len() < 10 {
        return Ok(None);
    }
    if data[0] != 0x1f || data[1] != 0x8b {
        return Err(corrupt("not a gzip stream"));
    }
    if data[2] != 8 {
        return Err(corrupt("unsupported gzip compression method"));
    }
    let flags = data[3];
    if flags & 0xe0 != 0 {
        return Err(corrupt("reserved gzip flags set"));
    }
    let mut at = 10usize;
    if flags & 0x04 != 0 {
        // FEXTRA: a two-byte length, then that many bytes.
        let Some(len) = data.get(at..at + 2) else {
            return Ok(None);
        };
        at += 2 + usize::from(u16::from_le_bytes([len[0], len[1]]));
        if data.len() < at {
            return Ok(None);
        }
    }
    for flag in [0x08u8, 0x10] {
        // FNAME, FCOMMENT: zero-terminated strings.
        if flags & flag != 0 {
            match data[at..].iter().position(|&b| b == 0) {
                Some(end) => at += end + 1,
                None => return Ok(None),
            }
        }
    }
    if flags & 0x02 != 0 {
        // FHCRC: a two-byte header checksum (not verified).
        at += 2;
        if data.len() < at {
            return Ok(None);
        }
    }
    Ok(Some(at))
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// Updates a (pre-inverted) CRC-32 register with `data`.
fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// Updates an Adler-32 `(a, b)` pair with `data`.
fn adler32_update((mut a, mut b): (u32, u32), data: &[u8]) -> (u32, u32) {
    const MODULUS: u32 = 65_521;
    // 5552 is the most bytes that can be summed before `b` could overflow.
    for block in data.chunks(5552) {
        for &byte in block {
            a += u32::from(byte);
            b += a;
        }
        a %= MODULUS;
        b %= MODULUS;
    }
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Decodes `data` fed `step` bytes at a time.
    fn run(format: Format, data: &[u8], step: usize) -> Result<(Vec<u8>, bool), HttpError> {
        let mut dec = Decoder::new(format);
        let mut out = Vec::new();
        for piece in data.chunks(step.max(1)) {
            dec.feed(piece);
            loop {
                let got = dec.decode(4096)?;
                if got.is_empty() {
                    break;
                }
                assert!(got.len() <= 4096 + 258, "piece of {} bytes", got.len());
                out.extend_from_slice(&got);
            }
        }
        Ok((out, dec.is_complete()))
    }

    /// Decodes all at once and byte by byte, requiring both to agree.
    fn inflate(format: Format, data: &[u8]) -> Result<Vec<u8>, HttpError> {
        let (whole, complete) = run(format, data, data.len())?;
        assert!(complete, "stream not complete");
        for step in [1, 2, 3, 7] {
            let (pieces, complete) = run(format, data, step)?;
            assert!(complete, "stream not complete at step {step}");
            assert_eq!(pieces, whole, "step {step}");
        }
        Ok(whole)
    }

    /// Deterministic compressible text used by the larger vectors (the same
    /// generator produced the embedded compressed data).
    fn sample_text(n: usize) -> Vec<u8> {
        const WORDS: [&str; 8] = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        ];
        let mut out = Vec::new();
        let mut x: u32 = 12345;
        while out.len() < n {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345) & 0x7fff_ffff;
            out.extend_from_slice(WORDS[((x >> 16) & 7) as usize].as_bytes());
            out.push(if (x >> 8) & 15 == 0 { b'\n' } else { b' ' });
        }
        out.truncate(n);
        out
    }

    // Vectors produced with CPython's zlib module (see the comment on each).

    /// `zlib.compressobj(0, wbits=-15)` over b"hello stored block".
    const RAW_STORED: &str = "011200edff68656c6c6f2073746f72656420626c6f636b";
    /// `zlib.compressobj(9, strategy=Z_FIXED, wbits=-15)` over
    /// b"abcabcabcabc hello hello hello".
    const RAW_FIXED: &str = "4b4c4a4e8421858cd49c9c7c641200";
    /// `zlib.compressobj(9, strategy=Z_FIXED, wbits=15)` over
    /// b"hello hello hello hello" (zlib wrapper).
    const ZLIB_FIXED: &str = "7801cb48cdc9c957c8402701680308b1";
    /// `gzip.compress(b"hello gzip world", mtime=0)`.
    const GZIP_SMALL: &str =
        "1f8b08000000000002ffcb48cdc9c95748afca2c5028cf2fca4901006b7de8b710000000";
    /// A gzip member with FEXTRA, FNAME, FCOMMENT and FHCRC set, wrapping
    /// b"hi".
    const GZIP_ALL_FLAGS: &str =
        "1f8b081e000000000003 0400 61626364 6e616d6500 636f6d6d656e7400 0000 cbc80400 ac2a93d8 02000000";

    #[test]
    fn crc_and_adler_known_values() {
        assert_eq!(!crc32_update(!0, b"123456789"), 0xcbf4_3926);
        let (a, b) = adler32_update((1, 0), b"Wikipedia");
        assert_eq!(b << 16 | a, 0x11e6_0398);
    }

    #[test]
    fn stored_block() {
        assert_eq!(
            inflate(Format::Deflate, &hex(RAW_STORED)).unwrap(),
            b"hello stored block"
        );
    }

    #[test]
    fn fixed_huffman_block() {
        assert_eq!(
            inflate(Format::Deflate, &hex(RAW_FIXED)).unwrap(),
            b"abcabcabcabc hello hello hello"
        );
        assert_eq!(
            inflate(Format::Deflate, &hex(ZLIB_FIXED)).unwrap(),
            b"hello hello hello hello"
        );
    }

    #[test]
    fn gzip_members() {
        assert_eq!(
            inflate(Format::Gzip, &hex(GZIP_SMALL)).unwrap(),
            b"hello gzip world"
        );
        assert_eq!(inflate(Format::Gzip, &hex(GZIP_ALL_FLAGS)).unwrap(), b"hi");
        // Two concatenated members decode to the concatenated payloads.
        let mut two = hex(GZIP_SMALL);
        two.extend(hex(GZIP_ALL_FLAGS));
        assert_eq!(inflate(Format::Gzip, &two).unwrap(), b"hello gzip worldhi");
        // Trailing bytes that are not another member are ignored.
        let mut padded = hex(GZIP_SMALL);
        padded.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(inflate(Format::Gzip, &padded).unwrap(), b"hello gzip world");
    }

    #[test]
    fn dynamic_huffman_blocks() {
        let text = sample_text(6000);
        let data = hex(include_str!("testdata/inflate_dynamic_gzip.hex"));
        assert_eq!(inflate(Format::Gzip, &data).unwrap(), text);
        let data = hex(include_str!("testdata/inflate_dynamic_zlib.hex"));
        assert_eq!(inflate(Format::Deflate, &data).unwrap(), text);
    }

    #[test]
    fn long_matches_across_the_window() {
        // 100 000 bytes with a period of 30 000: a pseudo-random prefix that
        // then repeats, which needs match distances near the 32 KiB window
        // limit and spans several blocks.
        let data = hex(include_str!("testdata/inflate_window_raw.hex"));
        let out = inflate(Format::Deflate, &data).unwrap();
        assert_eq!(out.len(), 100_000);
        let mut x: u32 = 99;
        let mut expect = Vec::with_capacity(100_000);
        for _ in 0..30_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345) & 0x7fff_ffff;
            expect.push((x >> 16) as u8);
        }
        while expect.len() < 100_000 {
            let b = expect[expect.len() - 30_000];
            expect.push(b);
        }
        assert_eq!(out, expect);
    }

    #[test]
    fn truncated_input_is_never_complete() {
        for (format, vector) in [
            (Format::Deflate, RAW_STORED),
            (Format::Deflate, RAW_FIXED),
            (Format::Deflate, ZLIB_FIXED),
            (Format::Gzip, GZIP_SMALL),
            (Format::Gzip, GZIP_ALL_FLAGS),
        ] {
            let data = hex(vector);
            for cut in 1..data.len() {
                match run(format, &data[..cut], cut) {
                    Ok((_, complete)) => assert!(!complete, "{vector} cut at {cut}"),
                    Err(e) => panic!("{vector} cut at {cut}: {e}"),
                }
            }
        }
        let data = hex(include_str!("testdata/inflate_dynamic_gzip.hex"));
        for cut in (1..data.len()).step_by(7) {
            let (_, complete) = run(Format::Gzip, &data[..cut], 64).unwrap();
            assert!(!complete, "cut at {cut}");
        }
    }

    #[test]
    fn corrupt_input_is_an_error() {
        // Reserved block type 3.
        assert!(run(Format::Deflate, &[0x07, 0x00], 2).is_err());
        // Stored block whose NLEN is not the complement of LEN.
        assert!(run(Format::Deflate, &hex("010500faef68656c6c6f"), 16).is_err());
        // Fixed block starting with a match: nothing to copy from.
        // bits: BFINAL=1, BTYPE=01, length code 257 (0000001), distance code 0 (00000).
        assert!(run(Format::Deflate, &[0x03, 0x02, 0x00], 3).is_err());
        // Wrong gzip magic and method.
        assert!(run(Format::Gzip, &hex("1f8c08000000000002ff0300"), 16).is_err());
        assert!(run(Format::Gzip, &hex("1f8b07000000000002ff0300"), 16).is_err());

        // A flipped bit in the CRC, in the length, and in the data.
        let good = hex(GZIP_SMALL);
        let n = good.len();
        for at in [n - 8, n - 4, 12] {
            let mut bad = good.clone();
            bad[at] ^= 0x01;
            assert!(
                !matches!(run(Format::Gzip, &bad, n), Ok((_, true))),
                "flip at {at}"
            );
        }
        // A flipped bit in the Adler-32.
        let mut bad = hex(ZLIB_FIXED);
        let n = bad.len();
        bad[n - 1] ^= 0x01;
        assert!(run(Format::Deflate, &bad, n).is_err());

        // Dynamic block with an over-subscribed code-length code: HLIT=0,
        // HDIST=0, HCLEN=15 (19 lengths), every length 1.
        // BFINAL=1 BTYPE=10 | 00000 | 00000 | 1111 | 19 x 001
        let mut bitstream: Vec<u8> = Vec::new();
        let mut acc = 0u64;
        let mut n = 0;
        let mut put = |value: u64, width: u32, out: &mut Vec<u8>| {
            acc |= value << n;
            n += width;
            while n >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                n -= 8;
            }
        };
        put(1, 1, &mut bitstream);
        put(2, 2, &mut bitstream);
        put(0, 5, &mut bitstream);
        put(0, 5, &mut bitstream);
        put(15, 4, &mut bitstream);
        for _ in 0..19 {
            put(1, 3, &mut bitstream);
        }
        put(0, 7, &mut bitstream);
        assert!(run(Format::Deflate, &bitstream, 64).is_err());
    }

    #[test]
    fn every_single_bit_flip_is_detected_or_changes_nothing_silently() {
        // For a gzip member, any one-bit corruption of the DEFLATE data or
        // trailer must surface as an error or an incomplete stream — never as
        // a "complete" stream with different bytes.
        let good = hex(include_str!("testdata/inflate_dynamic_gzip.hex"));
        let expect = sample_text(6000);
        for at in (10..good.len()).step_by(3) {
            for bit in [0x01u8, 0x10, 0x80] {
                let mut bad = good.clone();
                bad[at] ^= bit;
                if let Ok((out, true)) = run(Format::Gzip, &bad, 512) {
                    assert_eq!(out, expect, "silent corruption at byte {at} bit {bit:#x}");
                }
            }
        }
    }

    #[test]
    fn output_is_bounded_per_call() {
        // 4 MiB of zeros compresses to a few KiB; each decode call must still
        // hand back a bounded piece, so the caller can stop at its size cap.
        let data = hex(include_str!("testdata/inflate_bomb_gzip.hex"));
        assert!(data.len() < 8192);
        let mut dec = Decoder::new(Format::Gzip);
        dec.feed(&data);
        let mut total = 0usize;
        let mut calls = 0usize;
        loop {
            let piece = dec.decode(1024).unwrap();
            if piece.is_empty() {
                break;
            }
            assert!(piece.len() <= 1024 + 258);
            assert!(piece.iter().all(|&b| b == 0));
            total += piece.len();
            calls += 1;
        }
        assert_eq!(total, 4 * 1024 * 1024);
        assert!(calls >= 4 * 1024 * 1024 / (1024 + 258));
        assert!(dec.is_complete());
    }

    #[test]
    fn empty_input_counts_as_complete() {
        let mut dec = Decoder::new(Format::Gzip);
        assert!(dec.decode(64).unwrap().is_empty());
        assert!(dec.is_complete());
        dec.feed(&[0x1f]);
        assert!(dec.decode(64).unwrap().is_empty());
        assert!(!dec.is_complete());
    }

    #[test]
    fn content_encoding_selection() {
        assert!(Decoder::for_content_encoding("gzip").is_some());
        assert!(Decoder::for_content_encoding(" GZIP ").is_some());
        assert!(Decoder::for_content_encoding("x-gzip").is_some());
        assert!(Decoder::for_content_encoding("deflate").is_some());
        assert!(Decoder::for_content_encoding("br").is_none());
        assert!(Decoder::for_content_encoding("gzip, br").is_none());
        assert!(Decoder::for_content_encoding("identity").is_none());
    }
}
