//! Low-level protobuf wire-format primitives: varints, zig-zag, field keys,
//! and the bounded [`Reader`] every decoder pulls from.
//!
//! These are the building blocks the [`Message`](super::Message) derive
//! expands to. They are public so that a hand-written message, or a transport
//! such as gRPC, can read and write raw fields without going through a
//! derived type.
//!
//! ```
//! use ferroly::proto::wire::{self, Reader, WireType};
//!
//! // Field 1, varint 150 — the example from the protobuf encoding guide.
//! let mut buf = Vec::new();
//! wire::encode_key(1, WireType::Varint, &mut buf);
//! wire::encode_varint(150, &mut buf);
//! assert_eq!(buf, [0x08, 0x96, 0x01]);
//!
//! let mut r = Reader::new(&buf);
//! assert_eq!(r.read_key().unwrap(), Some((1, WireType::Varint)));
//! assert_eq!(r.read_varint().unwrap(), 150);
//! assert!(r.is_empty());
//! ```

use std::collections::BTreeMap;

use super::error::ProtoError;
use super::limits::Limits;

/// The largest valid field number (2²⁹ − 1).
pub const MAX_TAG: u32 = (1 << 29) - 1;

/// The first field number reserved for the protobuf implementation.
pub const RESERVED_TAGS_START: u32 = 19_000;

/// The last field number reserved for the protobuf implementation.
pub const RESERVED_TAGS_END: u32 = 19_999;

/// The longest varint: ten bytes carry 64 bits.
pub const MAX_VARINT_LEN: usize = 10;

/// The six protobuf wire types (the low three bits of a field key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireType {
    /// `0` — int32, int64, uint32, uint64, sint32, sint64, bool, enum.
    Varint,
    /// `1` — fixed64, sfixed64, double.
    I64,
    /// `2` — string, bytes, embedded messages, packed repeated fields.
    Len,
    /// `3` — group start (deprecated; only skipped and preserved).
    StartGroup,
    /// `4` — group end (deprecated; only skipped and preserved).
    EndGroup,
    /// `5` — fixed32, sfixed32, float.
    I32,
}

impl WireType {
    /// The wire type for the low three bits of a key, or `None` for the
    /// unassigned values 6 and 7.
    pub const fn from_bits(bits: u8) -> Option<WireType> {
        match bits {
            0 => Some(WireType::Varint),
            1 => Some(WireType::I64),
            2 => Some(WireType::Len),
            3 => Some(WireType::StartGroup),
            4 => Some(WireType::EndGroup),
            5 => Some(WireType::I32),
            _ => None,
        }
    }

    /// The three-bit value written into a key.
    pub const fn bits(self) -> u8 {
        match self {
            WireType::Varint => 0,
            WireType::I64 => 1,
            WireType::Len => 2,
            WireType::StartGroup => 3,
            WireType::EndGroup => 4,
            WireType::I32 => 5,
        }
    }
}

/// Whether `tag` may be used as a field number: `1..=MAX_TAG`, outside the
/// reserved `19000..=19999` range.
pub const fn is_valid_tag(tag: u32) -> bool {
    tag >= 1 && tag <= MAX_TAG && !(tag >= RESERVED_TAGS_START && tag <= RESERVED_TAGS_END)
}

/// Appends `value` as a base-128 varint (1 to 10 bytes).
pub fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// The number of bytes [`encode_varint`] writes for `value`.
pub const fn varint_len(value: u64) -> usize {
    // Bits needed, rounded up to groups of seven (at least one byte).
    let bits = 64 - (value | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// Decodes one varint from the start of `bytes`, returning the value and the
/// number of bytes it occupied.
///
/// Non-canonical (padded) encodings up to ten bytes are accepted, as every
/// protobuf implementation does; an eleventh byte or bits beyond 64 are
/// [`ProtoError::VarintOverflow`].
pub fn decode_varint(bytes: &[u8]) -> Result<(u64, usize), ProtoError> {
    let mut value: u64 = 0;
    for (i, &b) in bytes.iter().enumerate().take(MAX_VARINT_LEN) {
        if i == MAX_VARINT_LEN - 1 && b > 0x01 {
            return Err(ProtoError::VarintOverflow);
        }
        value |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            return Ok((value, i + 1));
        }
    }
    if bytes.len() >= MAX_VARINT_LEN {
        Err(ProtoError::VarintOverflow)
    } else {
        Err(ProtoError::Truncated)
    }
}

/// Appends a field key (`tag << 3 | wire type`).
pub fn encode_key(tag: u32, wire_type: WireType, buf: &mut Vec<u8>) {
    encode_varint(u64::from(tag) << 3 | u64::from(wire_type.bits()), buf);
}

/// The number of bytes [`encode_key`] writes for `tag`.
pub const fn key_len(tag: u32) -> usize {
    varint_len((tag as u64) << 3)
}

/// Zig-zag maps a signed 32-bit value onto an unsigned one (`sint32`):
/// 0 → 0, −1 → 1, 1 → 2, −2 → 3, …
pub const fn zigzag_encode32(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

/// Inverts [`zigzag_encode32`].
pub const fn zigzag_decode32(value: u32) -> i32 {
    ((value >> 1) as i32) ^ -((value & 1) as i32)
}

/// Zig-zag maps a signed 64-bit value onto an unsigned one (`sint64`).
pub const fn zigzag_encode64(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Inverts [`zigzag_encode64`].
pub const fn zigzag_decode64(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// A bounded cursor over an encoded message.
///
/// Every read checks the remaining input first, so a hostile length or a
/// truncated value is an error, never a panic or an allocation sized by the
/// input. The reader also carries the decode budget from [`Limits`]: nested
/// messages and groups consume depth, every field (and every element of a
/// packed field) consumes one entry, and what decoding creates is charged
/// against [`Limits::max_decoded_bytes`] through [`Reader::charge`].
///
/// It also collects the proto2 `required` fields derived messages report as
/// missing, so a sub-message split across several occurrences is checked
/// once all of them are merged; [`Message::decode`](super::Message::decode)
/// reports what is still missing at the end through
/// [`Reader::finish_required`].
#[derive(Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
    depth_left: usize,
    max_depth: usize,
    entries_left: usize,
    decoded_left: usize,
    max_decoded: usize,
    /// One record per live value with required fields, in creation order
    /// (so a repeated element's records are a suffix while it is decoded).
    required: Vec<PendingRequired>,
    /// `(address, type)` to the position of its live record in `required`:
    /// every occurrence of a value merges into one record, and a value can be
    /// forgotten without a scan.
    required_index: BTreeMap<(usize, &'static str), usize>,
}

/// The required fields no occurrence of one value carried so far.
#[derive(Debug, Clone, Copy)]
struct PendingRequired {
    /// The address of the value merged into: every occurrence of a singular
    /// sub-message merges into the same value.
    addr: usize,
    /// The full Rust type name: two values can share an address (a struct
    /// and its first field).
    key: &'static str,
    /// The short type name, for the error.
    message: &'static str,
    /// One bit per required field, set while no occurrence carried it; 0 once
    /// the value is forgotten.
    missing: u64,
    fields: &'static [&'static str],
}

/// What one required-field record costs against the decode budget: the
/// record and its index entry (a B-tree node holds up to 11 of them).
const REQUIRED_RECORD_BYTES: usize = std::mem::size_of::<PendingRequired>()
    + 2 * std::mem::size_of::<((usize, &'static str), usize)>();

impl<'a> Reader<'a> {
    /// A reader over `data` with the [default limits](Limits::DEFAULT).
    ///
    /// The input-size cap is not checked here; use [`Reader::with_limits`] for
    /// untrusted input of unknown size.
    pub fn new(data: &'a [u8]) -> Self {
        Self::unchecked(data, Limits::DEFAULT)
    }

    /// A reader over `data` with an explicit budget. Fails with
    /// [`ProtoError::LimitExceeded`] when `data` is larger than
    /// `limits.max_input_bytes`.
    pub fn with_limits(data: &'a [u8], limits: Limits) -> Result<Self, ProtoError> {
        if data.len() > limits.max_input_bytes {
            return Err(ProtoError::LimitExceeded(format!(
                "input is {} bytes, over the limit of {}",
                data.len(),
                limits.max_input_bytes
            )));
        }
        Ok(Self::unchecked(data, limits))
    }

    fn unchecked(data: &'a [u8], limits: Limits) -> Self {
        Reader {
            data,
            pos: 0,
            end: data.len(),
            depth_left: limits.max_depth,
            max_depth: limits.max_depth,
            entries_left: limits.max_entries,
            decoded_left: limits.max_decoded_bytes,
            max_decoded: limits.max_decoded_bytes,
            required: Vec::new(),
            required_index: BTreeMap::new(),
        }
    }

    /// Charges `bytes` of decoded memory against
    /// [`Limits::max_decoded_bytes`], failing with
    /// [`ProtoError::LimitExceeded`] once the budget is spent. The field
    /// helpers call it before each allocation decoding makes; a hand-written
    /// [`Message`](super::Message) should too.
    pub fn charge(&mut self, bytes: usize) -> Result<(), ProtoError> {
        if bytes > self.decoded_left {
            return Err(ProtoError::LimitExceeded(format!(
                "the decoded message would exceed max_decoded_bytes ({} bytes)",
                self.max_decoded
            )));
        }
        self.decoded_left -= bytes;
        Ok(())
    }

    /// Charges for pushing one element (owning `heap` further bytes) onto
    /// `vec`: when the push grows the vector, the slots of its new capacity
    /// are charged, so the charge follows the real allocation.
    pub fn charge_push<T>(&mut self, vec: &Vec<T>, heap: usize) -> Result<(), ProtoError> {
        let elem = std::mem::size_of::<T>();
        let mut cost = heap;
        if vec.len() == vec.capacity() && elem != 0 {
            // `Vec`'s amortized growth: double, starting at 4 (8 for bytes).
            let min = if elem == 1 { 8 } else { 4 };
            let new_cap = vec.capacity().saturating_mul(2).max(min);
            cost = cost.saturating_add((new_cap - vec.capacity()).saturating_mul(elem));
        }
        self.charge(cost)
    }

    /// Records that one occurrence of the message at `addr` (of the Rust type
    /// `key`, named `message` in errors) lacked the required fields whose bits are set in
    /// `missing`. Derived code calls it at the end of each `merge_from` of a
    /// message with required fields; occurrences of the same value merge into
    /// one record.
    #[doc(hidden)]
    pub fn note_required(
        &mut self,
        addr: *const (),
        key: &'static str,
        message: &'static str,
        missing: u64,
        fields: &'static [&'static str],
    ) -> Result<(), ProtoError> {
        let addr = addr as usize;
        if let Some(&i) = self.required_index.get(&(addr, key)) {
            self.required[i].missing &= missing;
            return Ok(());
        }
        self.charge(REQUIRED_RECORD_BYTES)?;
        self.required_index.insert((addr, key), self.required.len());
        self.required.push(PendingRequired {
            addr,
            key,
            message,
            missing,
            fields,
        });
        Ok(())
    }

    /// Whether any required-field record is pending (derived code skips
    /// forgetting a replaced value when none is).
    #[doc(hidden)]
    pub fn has_required(&self) -> bool {
        !self.required_index.is_empty()
    }

    /// A marker for [`settle_required`](Self::settle_required).
    #[doc(hidden)]
    pub fn required_mark(&self) -> usize {
        self.required.len()
    }

    /// Checks the required-field records made since `mark` — a value that is
    /// now complete, such as a repeated element about to be stored — and
    /// drops them. A field is missing when no occurrence of its message
    /// carried it.
    #[doc(hidden)]
    pub fn settle_required(&mut self, mark: usize) -> Result<(), ProtoError> {
        if self.required.len() <= mark {
            return Ok(());
        }
        let mut result = Ok(());
        for (i, p) in self.required.iter().enumerate().skip(mark) {
            if p.missing != 0 && result.is_ok() {
                let bit = p.missing.trailing_zeros() as usize;
                let field = p.fields.get(bit).copied().unwrap_or("?");
                result = Err(ProtoError::missing_required(p.message, field));
            }
            // A forgotten record's key may since belong to a newer record.
            if self.required_index.get(&(p.addr, p.key)) == Some(&i) {
                self.required_index.remove(&(p.addr, p.key));
            }
        }
        self.required.truncate(mark);
        result
    }

    /// Drops the record of the value at `addr` of the Rust type `key`, if
    /// any: the value is being dropped (a oneof variant being replaced), and
    /// a later value at the same address must not inherit what it carried.
    #[doc(hidden)]
    pub fn forget_required(&mut self, addr: *const (), key: &'static str) {
        if let Some(i) = self.required_index.remove(&(addr as usize, key)) {
            self.required[i].missing = 0;
        }
    }

    /// Fails with [`ProtoError::MissingRequired`] when a proto2 `required`
    /// field was absent from every occurrence of its message read so far,
    /// then forgets the records. [`Message::decode`](super::Message::decode)
    /// calls it after the top-level `merge_from`; call it yourself when
    /// driving `merge_from` on a reader directly.
    pub fn finish_required(&mut self) -> Result<(), ProtoError> {
        self.settle_required(0)
    }

    /// Forgets the required-field records without checking them.
    /// [`Message::merge`](super::Message::merge) does this: the value merged
    /// into may already hold the fields.
    pub fn discard_required(&mut self) {
        self.required.clear();
        self.required_index.clear();
    }

    /// Whether the current message (or packed run) has been fully read.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.end
    }

    /// Bytes left in the current message (or packed run).
    pub fn remaining(&self) -> usize {
        self.end - self.pos
    }

    /// The offset of the cursor from the start of the whole input.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Reads one varint.
    pub fn read_varint(&mut self) -> Result<u64, ProtoError> {
        let (value, used) = decode_varint(&self.data[self.pos..self.end])?;
        self.pos += used;
        Ok(value)
    }

    /// Reads four little-endian bytes.
    pub fn read_fixed32(&mut self) -> Result<u32, ProtoError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads eight little-endian bytes.
    pub fn read_fixed64(&mut self) -> Result<u64, ProtoError> {
        let bytes = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(a))
    }

    /// Reads the next field key, or `None` at the end of the current message.
    ///
    /// Each key consumes one entry of the budget. Field number 0, numbers above
    /// [`MAX_TAG`], and the unassigned wire types 6 and 7 are errors.
    pub fn read_key(&mut self) -> Result<Option<(u32, WireType)>, ProtoError> {
        if self.is_empty() {
            return Ok(None);
        }
        let key = self.read_varint()?;
        let tag = key >> 3;
        if tag == 0 || tag > u64::from(MAX_TAG) {
            return Err(ProtoError::InvalidTag(tag));
        }
        let tag = tag as u32;
        let bits = (key & 7) as u8;
        let wire_type =
            WireType::from_bits(bits).ok_or(ProtoError::InvalidWireType { tag, bits })?;
        self.take_entry()?;
        Ok(Some((tag, wire_type)))
    }

    /// Consumes one entry of the budget (the derive calls this for each
    /// element of a packed field).
    pub fn take_entry(&mut self) -> Result<(), ProtoError> {
        if self.entries_left == 0 {
            return Err(ProtoError::LimitExceeded(
                "too many fields or elements".to_owned(),
            ));
        }
        self.entries_left -= 1;
        Ok(())
    }

    /// Reads a length prefix and checks it against the remaining input.
    pub fn read_len(&mut self) -> Result<usize, ProtoError> {
        let len = self.read_varint()?;
        let remaining = self.remaining();
        if len > remaining as u64 {
            return Err(ProtoError::LengthOutOfBounds { len, remaining });
        }
        Ok(len as usize)
    }

    /// Reads a length-delimited value and returns its bytes, borrowed from the
    /// input.
    pub fn read_bytes(&mut self) -> Result<&'a [u8], ProtoError> {
        let len = self.read_len()?;
        self.take(len)
    }

    /// Reads a length-delimited embedded message: narrows the reader to the
    /// message's bytes, consumes one level of depth, runs `f`, and checks that
    /// `f` read exactly those bytes.
    pub fn read_nested<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, ProtoError>,
    ) -> Result<T, ProtoError> {
        self.enter()?;
        let out = self.read_delimited(f);
        self.depth_left += 1;
        out
    }

    /// Reads a packed repeated run: like [`read_nested`](Self::read_nested) but
    /// without consuming depth.
    pub fn read_packed(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<(), ProtoError>,
    ) -> Result<(), ProtoError> {
        self.read_delimited(f)
    }

    fn read_delimited<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, ProtoError>,
    ) -> Result<T, ProtoError> {
        let len = self.read_len()?;
        let outer_end = self.end;
        self.end = self.pos + len;
        let out = f(self);
        let finished = self.pos == self.end;
        self.end = outer_end;
        let out = out?;
        if finished {
            Ok(out)
        } else {
            Err(ProtoError::Malformed("embedded value was not fully read"))
        }
    }

    fn enter(&mut self) -> Result<(), ProtoError> {
        if self.depth_left == 0 {
            return Err(ProtoError::DepthExceeded(self.max_depth));
        }
        self.depth_left -= 1;
        Ok(())
    }

    /// Skips the value of a field whose key was just read.
    pub fn skip_field(&mut self, tag: u32, wire_type: WireType) -> Result<(), ProtoError> {
        match wire_type {
            WireType::Varint => self.read_varint().map(drop),
            WireType::I64 => self.take(8).map(drop),
            WireType::I32 => self.take(4).map(drop),
            WireType::Len => self.read_bytes().map(drop),
            WireType::StartGroup => self.skip_group(tag).map(drop),
            WireType::EndGroup => Err(ProtoError::UnexpectedEndGroup(tag)),
        }
    }

    /// Skips a group whose start key (for `tag`) was just read, returning the
    /// bytes between the start and end keys.
    pub fn skip_group(&mut self, tag: u32) -> Result<&'a [u8], ProtoError> {
        self.enter()?;
        let out = self.skip_group_body(tag);
        self.depth_left += 1;
        out
    }

    fn skip_group_body(&mut self, tag: u32) -> Result<&'a [u8], ProtoError> {
        let start = self.pos;
        loop {
            let before = self.pos;
            match self.read_key()? {
                None => return Err(ProtoError::Truncated),
                Some((t, WireType::EndGroup)) => {
                    if t != tag {
                        return Err(ProtoError::UnexpectedEndGroup(t));
                    }
                    return Ok(&self.data[start..before]);
                }
                Some((t, wt)) => self.skip_field(t, wt)?,
            }
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if n > self.remaining() {
            return Err(ProtoError::Truncated);
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(v: u64) -> Vec<u8> {
        let mut b = Vec::new();
        encode_varint(v, &mut b);
        b
    }

    #[test]
    fn varint_lengths_match_encoding() {
        for v in [0, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            assert_eq!(varint(v).len(), varint_len(v), "{v}");
            assert_eq!(decode_varint(&varint(v)).unwrap(), (v, varint_len(v)));
        }
        assert_eq!(varint(u64::MAX).len(), 10);
        assert_eq!(key_len(1), 1);
        assert_eq!(key_len(16), 2);
        assert_eq!(key_len(MAX_TAG), 5);
    }

    #[test]
    fn overlong_and_truncated_varints() {
        assert_eq!(decode_varint(&[0x80]), Err(ProtoError::Truncated));
        assert_eq!(decode_varint(&[]), Err(ProtoError::Truncated));
        // Padded but within ten bytes is accepted.
        assert_eq!(decode_varint(&[0x81, 0x80, 0x00]).unwrap(), (1, 3));
        // Eleven bytes, or a tenth byte above 1, overflow.
        assert_eq!(decode_varint(&[0xff; 11]), Err(ProtoError::VarintOverflow));
        let mut ten = vec![0xff; 9];
        ten.push(0x02);
        assert_eq!(decode_varint(&ten), Err(ProtoError::VarintOverflow));
    }

    #[test]
    fn zigzag_round_trips() {
        assert_eq!(zigzag_encode32(0), 0);
        assert_eq!(zigzag_encode32(-1), 1);
        assert_eq!(zigzag_encode32(1), 2);
        assert_eq!(zigzag_encode32(i32::MAX), 0xffff_fffe);
        assert_eq!(zigzag_encode32(i32::MIN), 0xffff_ffff);
        assert_eq!(zigzag_encode64(i64::MIN), u64::MAX);
        for v in [0, 1, -1, 63, -64, i32::MAX, i32::MIN] {
            assert_eq!(zigzag_decode32(zigzag_encode32(v)), v);
        }
        for v in [0, 1, -1, i64::MAX, i64::MIN] {
            assert_eq!(zigzag_decode64(zigzag_encode64(v)), v);
        }
    }

    #[test]
    fn tags_are_validated() {
        assert!(!is_valid_tag(0));
        assert!(is_valid_tag(1));
        assert!(is_valid_tag(18_999) && !is_valid_tag(19_000) && !is_valid_tag(19_999));
        assert!(is_valid_tag(20_000) && is_valid_tag(MAX_TAG) && !is_valid_tag(MAX_TAG + 1));

        assert_eq!(
            Reader::new(&[0x00]).read_key(),
            Err(ProtoError::InvalidTag(0))
        );
        assert_eq!(
            Reader::new(&[0x0e]).read_key(),
            Err(ProtoError::InvalidWireType { tag: 1, bits: 6 })
        );
    }

    #[test]
    fn lengths_are_checked_against_the_input() {
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f, 1, 2]);
        assert!(matches!(
            r.read_bytes(),
            Err(ProtoError::LengthOutOfBounds { remaining: 2, .. })
        ));
        assert_eq!(
            Reader::new(&[1, 2, 3]).read_fixed32(),
            Err(ProtoError::Truncated)
        );
    }

    #[test]
    fn groups_are_skipped_and_bounded() {
        // field 1 group { field 2 varint 5 } end
        let data = [0x0b, 0x10, 0x05, 0x0c];
        let mut r = Reader::new(&data);
        let (tag, wt) = r.read_key().unwrap().unwrap();
        assert_eq!(wt, WireType::StartGroup);
        assert_eq!(r.skip_group(tag).unwrap(), &[0x10, 0x05]);
        assert!(r.is_empty());

        // Mismatched end tag, unterminated group, deep nesting.
        let mut r = Reader::new(&[0x0b, 0x14]);
        r.read_key().unwrap();
        assert_eq!(r.skip_group(1), Err(ProtoError::UnexpectedEndGroup(2)));
        let mut r = Reader::new(&[0x0b, 0x10, 0x01]);
        r.read_key().unwrap();
        assert_eq!(r.skip_group(1), Err(ProtoError::Truncated));
        let deep = vec![0x0b; 10_000];
        let mut r = Reader::new(&deep);
        r.read_key().unwrap();
        assert!(matches!(r.skip_group(1), Err(ProtoError::DepthExceeded(_))));
    }

    #[test]
    fn budget_limits_entries_and_input() {
        let limits = Limits::new().with_max_entries(2);
        let data = [0x08, 0x01, 0x08, 0x02, 0x08, 0x03];
        let mut r = Reader::with_limits(&data, limits).unwrap();
        assert!(r.read_key().is_ok());
        r.read_varint().unwrap();
        assert!(r.read_key().is_ok());
        r.read_varint().unwrap();
        assert!(matches!(r.read_key(), Err(ProtoError::LimitExceeded(_))));

        let small = Limits::new().with_max_input_bytes(3);
        assert!(Reader::with_limits(&data, small).is_err());
    }
}
