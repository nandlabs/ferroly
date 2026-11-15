//! HPACK header compression (RFC 7541).
//!
//! The [`Decoder`] implements the full format: the static table, a dynamic
//! table bounded by the size this endpoint advertised, Huffman-coded strings
//! and dynamic-table size updates. Decoding is bounded: string lengths are
//! checked against the input, the decoded header list is capped (a block that
//! expands past the cap — an "HPACK bomb" — is decoded to keep the table in
//! sync but its fields are dropped), and integers are limited in size.
//!
//! The [`Encoder`] never inserts into the dynamic table (which is always
//! valid HPACK): it emits indexed fields for exact static-table matches and
//! literals otherwise, Huffman-coding a string whenever that is shorter, and
//! marks credentials as never-indexed.

use std::collections::VecDeque;

use super::huffman;

/// RFC 7541 Appendix A.
pub(crate) const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// Per-entry overhead counted by the table size and the header-list size
/// (RFC 7541 §4.1, RFC 9113 §6.5.2).
pub(crate) const ENTRY_OVERHEAD: usize = 32;
/// Largest integer accepted in a representation; anything bigger is an error
/// (every legitimate value — an index, a length, a table size — is far below).
const MAX_INTEGER: u64 = 1 << 28;
/// Most dynamic-table size updates accepted at the start of one block.
const MAX_SIZE_UPDATES: usize = 2;

/// Why a header block could not be decoded. Every variant is a connection
/// error of type `COMPRESSION_ERROR`, because the decoder's state can no
/// longer be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HpackError {
    /// The block ended in the middle of a representation.
    Truncated,
    /// An index of 0, or one past the end of the tables.
    BadIndex,
    /// An integer larger than any value HPACK legitimately carries.
    IntegerOverflow,
    /// Invalid Huffman data.
    Huffman,
    /// A dynamic-table size update above the advertised limit, after a field,
    /// or repeated too often.
    BadSizeUpdate,
}

impl HpackError {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HpackError::Truncated => "truncated header block",
            HpackError::BadIndex => "invalid header table index",
            HpackError::IntegerOverflow => "oversized integer in header block",
            HpackError::Huffman => "invalid Huffman string in header block",
            HpackError::BadSizeUpdate => "invalid dynamic table size update",
        }
    }
}

/// A decoded header field (names and values are raw octets).
pub(crate) type Field = (Vec<u8>, Vec<u8>);

/// Caps applied while decoding one header block.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DecodeLimits {
    /// Cap on the decoded list size: name + value + 32 per field.
    pub(crate) max_list_size: usize,
    /// Cap on the number of fields.
    pub(crate) max_fields: usize,
}

/// The dynamic table: newest entry first.
#[derive(Debug, Default)]
struct DynTable {
    entries: VecDeque<Field>,
    size: usize,
    max_size: usize,
}

impl DynTable {
    fn new(max_size: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            size: 0,
            max_size,
        }
    }

    fn evict_to(&mut self, limit: usize) {
        while self.size > limit {
            match self.entries.pop_back() {
                Some((n, v)) => self.size -= n.len() + v.len() + ENTRY_OVERHEAD,
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }

    fn set_max(&mut self, max: usize) {
        self.max_size = max;
        self.evict_to(max);
    }

    fn insert(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let size = name.len() + value.len() + ENTRY_OVERHEAD;
        if size > self.max_size {
            // An entry larger than the table empties it (RFC 7541 §4.4).
            self.entries.clear();
            self.size = 0;
            return;
        }
        self.evict_to(self.max_size - size);
        self.entries.push_front((name, value));
        self.size += size;
    }
}

/// Looks up a (1-based) index in the static table, then the dynamic table.
fn lookup(table: &DynTable, index: u64) -> Result<(&[u8], &[u8]), HpackError> {
    let index = usize::try_from(index).map_err(|_| HpackError::BadIndex)?;
    if index == 0 {
        return Err(HpackError::BadIndex);
    }
    if index <= STATIC_TABLE.len() {
        let (n, v) = STATIC_TABLE[index - 1];
        return Ok((n.as_bytes(), v.as_bytes()));
    }
    table
        .entries
        .get(index - STATIC_TABLE.len() - 1)
        .map(|(n, v)| (n.as_slice(), v.as_slice()))
        .ok_or(HpackError::BadIndex)
}

/// Reads an HPACK integer with an `n`-bit prefix (RFC 7541 §5.1).
fn read_int(buf: &[u8], pos: &mut usize, n: u8) -> Result<u64, HpackError> {
    let first = *buf.get(*pos).ok_or(HpackError::Truncated)?;
    *pos += 1;
    let mask = (1u16 << n) - 1;
    let mut value = u64::from(first) & u64::from(mask);
    if value < u64::from(mask) {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos).ok_or(HpackError::Truncated)?;
        *pos += 1;
        value += u64::from(b & 0x7f) << shift;
        if value > MAX_INTEGER {
            return Err(HpackError::IntegerOverflow);
        }
        if b & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 28 {
            return Err(HpackError::IntegerOverflow);
        }
    }
}

/// Writes an HPACK integer with an `n`-bit prefix, OR-ing `flags` into the
/// first byte.
fn write_int(out: &mut Vec<u8>, flags: u8, n: u8, mut value: usize) {
    let mask = (1usize << n) - 1;
    if value < mask {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | mask as u8);
    value -= mask;
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Reads a string literal (RFC 7541 §5.2). The result is capped at `max`
/// bytes: a longer string yields `Ok(None)` (consumed, but not returned).
fn read_string(buf: &[u8], pos: &mut usize, max: usize) -> Result<Option<Vec<u8>>, HpackError> {
    let huffman = *buf.get(*pos).ok_or(HpackError::Truncated)? & 0x80 != 0;
    let len = read_int(buf, pos, 7)?;
    let len = usize::try_from(len).map_err(|_| HpackError::IntegerOverflow)?;
    let end = pos.checked_add(len).ok_or(HpackError::Truncated)?;
    let raw = buf.get(*pos..end).ok_or(HpackError::Truncated)?;
    *pos = end;
    if huffman {
        // A Huffman string decodes to at most 8/5 of its length.
        let mut out = Vec::with_capacity((len.saturating_mul(8) / 5).min(max));
        match huffman::decode(raw, max, &mut out) {
            Ok(()) => Ok(Some(out)),
            Err(huffman::HuffmanError::TooLong) => {
                // Still validate the rest of the string so a malformed block
                // is always an error, whatever the cap.
                let mut sink = Vec::new();
                match huffman::decode(raw, usize::MAX, &mut sink) {
                    Ok(()) => Ok(None),
                    Err(_) => Err(HpackError::Huffman),
                }
            }
            Err(huffman::HuffmanError::Invalid) => Err(HpackError::Huffman),
        }
    } else if raw.len() > max {
        Ok(None)
    } else {
        Ok(Some(raw.to_vec()))
    }
}

/// The receiving side of HPACK for one connection.
#[derive(Debug)]
pub(crate) struct Decoder {
    table: DynTable,
    /// The `SETTINGS_HEADER_TABLE_SIZE` this endpoint advertised: the most
    /// the peer's encoder may size the table to.
    limit: usize,
}

/// What [`Decoder::decode`] produced.
#[derive(Debug, Default)]
pub(crate) struct Decoded {
    /// The fields, in order (empty when `too_large`).
    pub(crate) fields: Vec<Field>,
    /// The block exceeded the list-size or field-count cap; its fields were
    /// dropped (the table was still updated).
    pub(crate) too_large: bool,
}

impl Decoder {
    /// A decoder whose dynamic table may grow to `limit` bytes.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            table: DynTable::new(limit),
            limit,
        }
    }

    /// The current dynamic table size in bytes (for tests).
    #[cfg(test)]
    pub(crate) fn table_size(&self) -> usize {
        self.table.size
    }

    /// Decodes one complete header block.
    pub(crate) fn decode(
        &mut self,
        block: &[u8],
        limits: &DecodeLimits,
    ) -> Result<Decoded, HpackError> {
        let mut out = Decoded::default();
        let mut list_size = 0usize;
        let mut pos = 0usize;
        let mut seen_field = false;
        let mut size_updates = 0usize;
        // A string longer than both the list cap and the table can never be
        // kept or indexed, so it is skipped rather than copied.
        let string_cap = limits.max_list_size.max(self.limit);
        while pos < block.len() {
            let b = block[pos];
            if b & 0x80 != 0 {
                // Indexed header field.
                let index = read_int(block, &mut pos, 7)?;
                let (n, v) = lookup(&self.table, index)?;
                seen_field = true;
                push_field(&mut out, &mut list_size, limits, n, v);
            } else if b & 0xe0 == 0x20 {
                // Dynamic table size update: only at the start of a block.
                if seen_field || size_updates >= MAX_SIZE_UPDATES {
                    return Err(HpackError::BadSizeUpdate);
                }
                size_updates += 1;
                let size = read_int(block, &mut pos, 5)?;
                let size = usize::try_from(size).map_err(|_| HpackError::BadSizeUpdate)?;
                if size > self.limit {
                    return Err(HpackError::BadSizeUpdate);
                }
                self.table.set_max(size);
            } else {
                // A literal: with incremental indexing (01), without (0000)
                // or never indexed (0001).
                let (prefix, index_it) = if b & 0xc0 == 0x40 {
                    (6, true)
                } else {
                    (4, false)
                };
                let name_index = read_int(block, &mut pos, prefix)?;
                let name = if name_index == 0 {
                    read_string(block, &mut pos, string_cap)?
                } else {
                    Some(lookup(&self.table, name_index)?.0.to_vec())
                };
                let value = read_string(block, &mut pos, string_cap)?;
                seen_field = true;
                match (name, value) {
                    (Some(name), Some(value)) => {
                        push_field(&mut out, &mut list_size, limits, &name, &value);
                        if index_it {
                            self.table.insert(name, value);
                        }
                    }
                    (name, value) => {
                        // Over the cap: the field is dropped. An indexed
                        // literal this large is also larger than any
                        // dynamic table we allow, which empties the table —
                        // exactly what inserting it would have done.
                        out.too_large = true;
                        out.fields.clear();
                        if index_it {
                            let n = name.map_or(string_cap + 1, |n| n.len());
                            let v = value.map_or(string_cap + 1, |v| v.len());
                            if n + v + ENTRY_OVERHEAD > self.table.max_size {
                                self.table.entries.clear();
                                self.table.size = 0;
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

fn push_field(out: &mut Decoded, list_size: &mut usize, limits: &DecodeLimits, n: &[u8], v: &[u8]) {
    if out.too_large {
        return;
    }
    *list_size = list_size.saturating_add(n.len() + v.len() + ENTRY_OVERHEAD);
    if *list_size > limits.max_list_size || out.fields.len() >= limits.max_fields {
        out.too_large = true;
        out.fields = Vec::new();
        return;
    }
    out.fields.push((n.to_vec(), v.to_vec()));
}

/// Header names whose values are credentials: sent as never-indexed
/// literals so no intermediary may compress them (RFC 7541 §7.1.3).
fn is_sensitive(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    )
}

/// The sending side of HPACK for one connection.
#[derive(Debug)]
pub(crate) struct Encoder {
    /// Emit a dynamic-table size update of 0 at the start of the next block:
    /// this encoder never uses the table, and saying so up front satisfies
    /// peers that require an update after a `SETTINGS_HEADER_TABLE_SIZE`
    /// change.
    announce_size: bool,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self {
            announce_size: true,
        }
    }

    /// Notes that the peer changed `SETTINGS_HEADER_TABLE_SIZE`.
    pub(crate) fn table_size_changed(&mut self) {
        self.announce_size = true;
    }

    /// Encodes `fields` (names already lower-case) as one header block.
    pub(crate) fn encode<'a, I>(&mut self, fields: I, out: &mut Vec<u8>)
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        if self.announce_size {
            write_int(out, 0x20, 5, 0);
            self.announce_size = false;
        }
        for (name, value) in fields {
            let mut name_index = 0usize;
            let mut exact = 0usize;
            for (i, (n, v)) in STATIC_TABLE.iter().enumerate() {
                if *n == name {
                    if name_index == 0 {
                        name_index = i + 1;
                    }
                    if *v == value {
                        exact = i + 1;
                        break;
                    }
                }
            }
            if exact != 0 && !is_sensitive(name) {
                write_int(out, 0x80, 7, exact);
                continue;
            }
            // Literal without indexing (0000) or never indexed (0001).
            let flags = if is_sensitive(name) { 0x10 } else { 0x00 };
            write_int(out, flags, 4, name_index);
            if name_index == 0 {
                write_string(out, name.as_bytes());
            }
            write_string(out, value.as_bytes());
        }
    }
}

fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    let huff = huffman::encoded_len(s);
    if huff < s.len() {
        write_int(out, 0x80, 7, huff);
        huffman::encode(s, out);
    } else {
        write_int(out, 0x00, 7, s.len());
        out.extend_from_slice(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const LIMITS: DecodeLimits = DecodeLimits {
        max_list_size: 1 << 20,
        max_fields: 1000,
    };

    fn fields(d: &Decoded) -> Vec<(String, String)> {
        d.fields
            .iter()
            .map(|(n, v)| {
                (
                    String::from_utf8(n.clone()).unwrap(),
                    String::from_utf8(v.clone()).unwrap(),
                )
            })
            .collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn integers_rfc_c1() {
        // C.1.1: 10 with a 5-bit prefix.
        let mut out = Vec::new();
        write_int(&mut out, 0, 5, 10);
        assert_eq!(out, [0x0a]);
        // C.1.2: 1337 with a 5-bit prefix.
        out.clear();
        write_int(&mut out, 0, 5, 1337);
        assert_eq!(out, [0x1f, 0x9a, 0x0a]);
        let mut pos = 0;
        assert_eq!(read_int(&out, &mut pos, 5).unwrap(), 1337);
        // C.1.3: 42 starting at an octet boundary.
        out.clear();
        write_int(&mut out, 0, 8, 42);
        assert_eq!(out, [0x2a]);
        // Overflow and truncation.
        let mut pos = 0;
        let huge = [0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(
            read_int(&huge, &mut pos, 5),
            Err(HpackError::IntegerOverflow)
        );
        let mut pos = 0;
        assert_eq!(
            read_int(&[0x1f, 0x9a], &mut pos, 5),
            Err(HpackError::Truncated)
        );
    }

    #[test]
    fn literal_examples_rfc_c2() {
        // C.2.1: literal with indexing.
        let mut d = Decoder::new(4096);
        let block = unhex("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572");
        let out = d.decode(&block, &LIMITS).unwrap();
        assert_eq!(fields(&out), pairs(&[("custom-key", "custom-header")]));
        assert_eq!(d.table_size(), 55);
        // C.2.2: literal without indexing.
        let mut d = Decoder::new(4096);
        let block = unhex("040c 2f73 616d 706c 652f 7061 7468");
        let out = d.decode(&block, &LIMITS).unwrap();
        assert_eq!(fields(&out), pairs(&[(":path", "/sample/path")]));
        assert_eq!(d.table_size(), 0);
        // C.2.3: literal never indexed.
        let block = unhex("1008 7061 7373 776f 7264 0673 6563 7265 74");
        let out = d.decode(&block, &LIMITS).unwrap();
        assert_eq!(fields(&out), pairs(&[("password", "secret")]));
        assert_eq!(d.table_size(), 0);
        // C.2.4: indexed field.
        let out = d.decode(&[0x82], &LIMITS).unwrap();
        assert_eq!(fields(&out), pairs(&[(":method", "GET")]));
    }

    /// Runs a sequence of requests through one decoder, checking each
    /// decoded list and the table size afterwards.
    /// One step: the block (hex), the expected fields, the table size after.
    type Step<'a> = (&'a str, &'a [(&'a str, &'a str)], usize);

    fn run_sequence(table: usize, steps: &[Step<'_>]) {
        let mut d = Decoder::new(table);
        for (hex, expect, size) in steps {
            let out = d.decode(&unhex(hex), &LIMITS).unwrap();
            assert_eq!(fields(&out), pairs(expect));
            assert_eq!(d.table_size(), *size);
        }
    }

    const REQ1: &[(&str, &str)] = &[
        (":method", "GET"),
        (":scheme", "http"),
        (":path", "/"),
        (":authority", "www.example.com"),
    ];
    const REQ2: &[(&str, &str)] = &[
        (":method", "GET"),
        (":scheme", "http"),
        (":path", "/"),
        (":authority", "www.example.com"),
        ("cache-control", "no-cache"),
    ];
    const REQ3: &[(&str, &str)] = &[
        (":method", "GET"),
        (":scheme", "https"),
        (":path", "/index.html"),
        (":authority", "www.example.com"),
        ("custom-key", "custom-value"),
    ];

    #[test]
    fn requests_without_huffman_rfc_c3() {
        run_sequence(
            4096,
            &[
                (
                    "8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d",
                    REQ1,
                    57,
                ),
                ("8286 84be 5808 6e6f 2d63 6163 6865", REQ2, 110),
                (
                    "8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65",
                    REQ3,
                    164,
                ),
            ],
        );
    }

    #[test]
    fn requests_with_huffman_rfc_c4() {
        run_sequence(
            4096,
            &[
                ("8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff", REQ1, 57),
                ("8286 84be 5886 a8eb 1064 9cbf", REQ2, 110),
                (
                    "8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf",
                    REQ3,
                    164,
                ),
            ],
        );
    }

    const RESP1: &[(&str, &str)] = &[
        (":status", "302"),
        ("cache-control", "private"),
        ("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
        ("location", "https://www.example.com"),
    ];
    const RESP2: &[(&str, &str)] = &[
        (":status", "307"),
        ("cache-control", "private"),
        ("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
        ("location", "https://www.example.com"),
    ];
    const RESP3: &[(&str, &str)] = &[
        (":status", "200"),
        ("cache-control", "private"),
        ("date", "Mon, 21 Oct 2013 20:13:22 GMT"),
        ("location", "https://www.example.com"),
        ("content-encoding", "gzip"),
        (
            "set-cookie",
            "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
        ),
    ];

    #[test]
    fn responses_without_huffman_rfc_c5() {
        // A 256-byte table forces evictions.
        run_sequence(
            256,
            &[
                (
                    "4803 3330 3258 0770 7269 7661 7465 611d 4d6f 6e2c 2032 3120 4f63 7420
                     3230 3133 2032 303a 3133 3a32 3120 474d 546e 1768 7474 7073 3a2f 2f77
                     7777 2e65 7861 6d70 6c65 2e63 6f6d",
                    RESP1,
                    222,
                ),
                ("4803 3330 37c1 c0bf", RESP2, 222),
                (
                    "88c1 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 2032 303a 3133 3a32
                     3220 474d 54c0 5a04 677a 6970 7738 666f 6f3d 4153 444a 4b48 514b 425a
                     584f 5157 454f 5049 5541 5851 5745 4f49 553b 206d 6178 2d61 6765 3d33
                     3630 303b 2076 6572 7369 6f6e 3d31",
                    RESP3,
                    215,
                ),
            ],
        );
    }

    #[test]
    fn responses_with_huffman_rfc_c6() {
        run_sequence(
            256,
            &[
                (
                    "4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81
                     66e0 82a6 2d1b ff6e 919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3",
                    RESP1,
                    222,
                ),
                ("4883 640e ffc1 c0bf", RESP2, 222),
                (
                    "88c1 6196 d07a be94 1054 d444 a820 0595 040b 8166 e084 a62d 1bff c05a
                     839b d9ab 77ad 94e7 821d d7f2 e6c7 b335 dfdf cd5b 3960 d5af 2708 7f36
                     72c1 ab27 0fb5 291f 9587 3160 65c0 03ed 4ee5 b106 3d50 07",
                    RESP3,
                    215,
                ),
            ],
        );
    }

    #[test]
    fn encoder_round_trips_through_decoder() {
        let mut e = Encoder::new();
        let mut d = Decoder::new(4096);
        let list = [
            (":status", "200"),
            ("content-type", "application/grpc"),
            ("x-custom", "value with spaces"),
            ("authorization", "Bearer secret"),
            ("grpc-status", "0"),
            (":method", "GET"),
        ];
        for _ in 0..3 {
            let mut block = Vec::new();
            e.encode(list.iter().copied(), &mut block);
            let out = d.decode(&block, &LIMITS).unwrap();
            assert_eq!(fields(&out), pairs(&list));
        }
        // The first block announces a zero-size table.
        let mut e = Encoder::new();
        let mut block = Vec::new();
        e.encode([(":status", "200")], &mut block);
        assert_eq!(block, [0x20, 0x88]);
        block.clear();
        e.encode([(":status", "200")], &mut block);
        assert_eq!(block, [0x88]);
        // Credentials are never indexed.
        block.clear();
        e.encode([("authorization", "x")], &mut block);
        assert_eq!(block[0] & 0xf0, 0x10);
    }

    #[test]
    fn rejects_malformed_blocks() {
        let mut d = Decoder::new(4096);
        // Index 0, and past the end of both tables.
        assert_eq!(
            d.decode(&[0x80], &LIMITS).unwrap_err(),
            HpackError::BadIndex
        );
        assert_eq!(
            d.decode(&[0xbe], &LIMITS).unwrap_err(),
            HpackError::BadIndex
        );
        // A string longer than the block.
        assert_eq!(
            d.decode(&[0x40, 0x05, b'a'], &LIMITS).unwrap_err(),
            HpackError::Truncated
        );
        // Size update above the advertised limit, and after a field.
        assert_eq!(
            d.decode(&[0x3f, 0xe2, 0x1f], &LIMITS).unwrap_err(),
            HpackError::BadSizeUpdate
        );
        assert_eq!(
            d.decode(&[0x82, 0x20], &LIMITS).unwrap_err(),
            HpackError::BadSizeUpdate
        );
        // Three size updates in a row.
        assert_eq!(
            d.decode(&[0x20, 0x20, 0x20], &LIMITS).unwrap_err(),
            HpackError::BadSizeUpdate
        );
        // A valid size update shrinks (and evicts) the table.
        let mut d = Decoder::new(4096);
        d.decode(
            &unhex("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572"),
            &LIMITS,
        )
        .unwrap();
        assert_eq!(d.table_size(), 55);
        d.decode(&[0x20], &LIMITS).unwrap();
        assert_eq!(d.table_size(), 0);
        assert_eq!(
            d.decode(&[0xbe], &LIMITS).unwrap_err(),
            HpackError::BadIndex
        );
        // Bad Huffman data.
        assert_eq!(
            d.decode(&[0x40, 0x81, 0xff, 0x00], &LIMITS).unwrap_err(),
            HpackError::Huffman
        );
    }

    #[test]
    fn header_bomb_is_capped() {
        // One 4000-byte entry inserted, then referenced 1000 times: a 4 KiB
        // block that would expand to 4 MB.
        let mut d = Decoder::new(4096);
        let mut block = vec![0x40, 0x01, b'x'];
        write_int(&mut block, 0x00, 7, 3900);
        block.extend(std::iter::repeat(b'v').take(3900));
        block.extend(std::iter::repeat(0xbe).take(1000));
        let limits = DecodeLimits {
            max_list_size: 16 * 1024,
            max_fields: 100,
        };
        let out = d.decode(&block, &limits).unwrap();
        assert!(out.too_large);
        assert!(out.fields.is_empty());
        // The table stays in sync: the entry is still addressable.
        let out = d.decode(&[0xbe], &LIMITS).unwrap();
        assert_eq!(out.fields[0].1.len(), 3900);
        // A field count cap.
        let many = vec![0x82; 200];
        let out = d.decode(&many, &limits).unwrap();
        assert!(out.too_large);
        // A huge literal is dropped without being kept in memory.
        let mut block = vec![0x00, 0x01, b'y'];
        write_int(&mut block, 0x00, 7, 20_000);
        block.extend(std::iter::repeat(b'z').take(20_000));
        let out = d.decode(&block, &limits).unwrap();
        assert!(out.too_large && out.fields.is_empty());
    }
}
