//! Field-level encoding used by the derives (`ferroly::proto::__private`).
//!
//! A field is described by a *kind* marker (the protobuf scalar type, such as
//! [`SInt64`] or [`Str`]) and a *shape* (implicit presence, `Option`, `Vec`,
//! map, oneof). [`FieldValue<K>`] says how a Rust type is written as kind `K`;
//! the free functions implement each shape on top of it. The derive expands
//! every field to one call per direction, so the generated code stays small and
//! all wire logic lives here.
//!
//! This module is not a stable API: it is public only so derived code in other
//! crates can reach it.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, Hash};

use super::error::ProtoError;
use super::message::{EnumValue, Message, Oneof};
use super::wire::{self, Reader, WireType};

macro_rules! kinds {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$doc])*
            #[derive(Debug, Clone, Copy)]
            pub enum $name {}
        )*
    };
}

kinds!(
    /// `int32`: varint, negative values sign-extended to ten bytes.
    Int32,
    /// `int64`: varint.
    Int64,
    /// `uint32`: varint.
    UInt32,
    /// `uint64`: varint.
    UInt64,
    /// `sint32`: zig-zag varint.
    SInt32,
    /// `sint64`: zig-zag varint.
    SInt64,
    /// `fixed32`: four bytes.
    Fixed32,
    /// `fixed64`: eight bytes.
    Fixed64,
    /// `sfixed32`: four bytes.
    SFixed32,
    /// `sfixed64`: eight bytes.
    SFixed64,
    /// `float`: four bytes.
    Float,
    /// `double`: eight bytes.
    Double,
    /// `bool`: varint 0 or 1.
    Bool,
    /// `string`: length-delimited UTF-8.
    Str,
    /// `bytes`: length-delimited.
    Bytes,
    /// An embedded message: length-delimited.
    Msg,
    /// An enum: `int32` varint.
    Enum,
);

/// The lengths of the embedded messages of one value, computed once before
/// it is encoded.
///
/// Every embedded message is written with its length in front, so a naive
/// encoder asks each level for the length of its subtree: a message nested
/// `d` deep is measured `d` times. Encoding instead runs one length pass that
/// records each embedded message's length in pre-order, then one write pass
/// that reads them back in the same order, so encoding is linear in the
/// output whatever the depth.
#[derive(Debug, Default)]
pub struct SizeCache {
    sizes: Vec<usize>,
    pos: usize,
}

impl SizeCache {
    /// An empty cache (allocates only once an embedded message is measured).
    pub const fn new() -> Self {
        SizeCache {
            sizes: Vec::new(),
            pos: 0,
        }
    }

    fn reserve(&mut self) -> usize {
        self.sizes.push(0);
        self.sizes.len() - 1
    }

    fn set(&mut self, slot: usize, len: usize) {
        self.sizes[slot] = len;
    }

    /// The next recorded length, or `None` when the length pass did not run
    /// (the writer then measures as it goes).
    fn next(&mut self) -> Option<usize> {
        let len = self.sizes.get(self.pos).copied();
        if len.is_some() {
            self.pos += 1;
        }
        len
    }
}

/// How a Rust type is written as the protobuf kind `K`.
pub trait FieldValue<K>: Default {
    /// The wire type of one value.
    const WIRE_TYPE: WireType;
    /// Appends the value (without a key), taking embedded-message lengths
    /// from `cache`.
    fn encode_value(&self, buf: &mut Vec<u8>, cache: &mut SizeCache);
    /// The number of bytes [`encode_value`](Self::encode_value) writes.
    fn value_len(&self) -> usize;
    /// [`value_len`](Self::value_len), recording embedded-message lengths in
    /// `cache` for the write pass.
    fn value_len_cached(&self, cache: &mut SizeCache) -> usize {
        let _ = cache;
        self.value_len()
    }
    /// Reads one value (its key already read) into `self`: replaces a scalar,
    /// merges a message.
    fn merge_value(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError>;
    /// Whether an implicit-presence field holding this value is omitted.
    /// Messages are never omitted.
    fn is_default(&self) -> bool;
    /// Heap bytes a fresh `Self::default()` owns (charged against
    /// [`Limits::max_decoded_bytes`](super::Limits::max_decoded_bytes) when
    /// decoding creates one).
    fn default_heap_bytes() -> usize {
        0
    }
}

macro_rules! varint_kind {
    ($kind:ty, $rust:ty, |$v:ident| $to:expr, |$w:ident| $from:expr) => {
        impl FieldValue<$kind> for $rust {
            const WIRE_TYPE: WireType = WireType::Varint;
            fn encode_value(&self, buf: &mut Vec<u8>, _: &mut SizeCache) {
                let $v = *self;
                wire::encode_varint($to, buf);
            }
            fn value_len(&self) -> usize {
                let $v = *self;
                wire::varint_len($to)
            }
            fn merge_value(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError> {
                let $w = r.read_varint()?;
                *self = $from;
                Ok(())
            }
            fn is_default(&self) -> bool {
                *self == <$rust>::default()
            }
        }
    };
}

// Narrower integers are read by truncating the 64-bit varint, as every
// protobuf implementation does.
varint_kind!(Int32, i32, |v| v as i64 as u64, |w| w as i32);
varint_kind!(Int64, i64, |v| v as u64, |w| w as i64);
varint_kind!(UInt32, u32, |v| u64::from(v), |w| w as u32);
varint_kind!(UInt64, u64, |v| v, |w| w);
varint_kind!(SInt32, i32, |v| u64::from(wire::zigzag_encode32(v)), |w| {
    wire::zigzag_decode32(w as u32)
});
varint_kind!(SInt64, i64, |v| wire::zigzag_encode64(v), |w| {
    wire::zigzag_decode64(w)
});
varint_kind!(Bool, bool, |v| u64::from(v), |w| w != 0);

macro_rules! fixed_kind {
    ($kind:ty, $rust:ty, $wt:ident, $n:literal, $read:ident, |$v:ident| $from:expr, $default:expr) => {
        impl FieldValue<$kind> for $rust {
            const WIRE_TYPE: WireType = WireType::$wt;
            fn encode_value(&self, buf: &mut Vec<u8>, _: &mut SizeCache) {
                buf.extend_from_slice(&self.to_le_bytes());
            }
            fn value_len(&self) -> usize {
                $n
            }
            fn merge_value(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError> {
                let $v = r.$read()?;
                *self = $from;
                Ok(())
            }
            fn is_default(&self) -> bool {
                let check: fn(&$rust) -> bool = $default;
                check(self)
            }
        }
    };
}

fixed_kind!(Fixed32, u32, I32, 4, read_fixed32, |v| v, |v| *v == 0);
fixed_kind!(Fixed64, u64, I64, 8, read_fixed64, |v| v, |v| *v == 0);
fixed_kind!(SFixed32, i32, I32, 4, read_fixed32, |v| v as i32, |v| *v
    == 0);
fixed_kind!(SFixed64, i64, I64, 8, read_fixed64, |v| v as i64, |v| *v
    == 0);
// A float is "default" only when it is +0.0: -0.0 is written, as proto3
// requires.
fixed_kind!(
    Float,
    f32,
    I32,
    4,
    read_fixed32,
    |v| f32::from_bits(v),
    |v| v.to_bits() == 0
);
fixed_kind!(
    Double,
    f64,
    I64,
    8,
    read_fixed64,
    |v| f64::from_bits(v),
    |v| v.to_bits() == 0
);

impl FieldValue<Str> for String {
    const WIRE_TYPE: WireType = WireType::Len;
    fn encode_value(&self, buf: &mut Vec<u8>, _: &mut SizeCache) {
        wire::encode_varint(self.len() as u64, buf);
        buf.extend_from_slice(self.as_bytes());
    }
    fn value_len(&self) -> usize {
        wire::varint_len(self.len() as u64) + self.len()
    }
    fn merge_value(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError> {
        let bytes = r.read_bytes()?;
        let s = std::str::from_utf8(bytes).map_err(|_| ProtoError::InvalidUtf8)?;
        r.charge(s.len().saturating_sub(self.capacity()))?;
        self.clear();
        self.push_str(s);
        Ok(())
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl FieldValue<Bytes> for Vec<u8> {
    const WIRE_TYPE: WireType = WireType::Len;
    fn encode_value(&self, buf: &mut Vec<u8>, _: &mut SizeCache) {
        wire::encode_varint(self.len() as u64, buf);
        buf.extend_from_slice(self);
    }
    fn value_len(&self) -> usize {
        wire::varint_len(self.len() as u64) + self.len()
    }
    fn merge_value(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError> {
        let bytes = r.read_bytes()?;
        r.charge(bytes.len().saturating_sub(self.capacity()))?;
        self.clear();
        self.extend_from_slice(bytes);
        Ok(())
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl<M: Message> FieldValue<Msg> for M {
    const WIRE_TYPE: WireType = WireType::Len;
    fn encode_value(&self, buf: &mut Vec<u8>, cache: &mut SizeCache) {
        let len = match cache.next() {
            Some(len) => len,
            None => self.encoded_len(),
        };
        wire::encode_varint(len as u64, buf);
        self.__encode_cached(buf, cache);
    }
    fn value_len(&self) -> usize {
        let len = self.encoded_len();
        wire::varint_len(len as u64) + len
    }
    fn value_len_cached(&self, cache: &mut SizeCache) -> usize {
        let slot = cache.reserve();
        let len = self.__encoded_len_cached(cache);
        cache.set(slot, len);
        wire::varint_len(len as u64) + len
    }
    fn merge_value(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError> {
        r.read_nested(|r| self.merge_from(r))
    }
    fn is_default(&self) -> bool {
        false
    }
    fn default_heap_bytes() -> usize {
        M::DEFAULT_HEAP_BYTES
    }
}

impl<E> FieldValue<Enum> for EnumValue<E> {
    const WIRE_TYPE: WireType = WireType::Varint;
    fn encode_value(&self, buf: &mut Vec<u8>, _: &mut SizeCache) {
        wire::encode_varint(self.raw() as i64 as u64, buf);
    }
    fn value_len(&self) -> usize {
        wire::varint_len(self.raw() as i64 as u64)
    }
    fn merge_value(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError> {
        *self = EnumValue::from_raw(r.read_varint()? as i32);
        Ok(())
    }
    fn is_default(&self) -> bool {
        self.raw() == 0
    }
}

/// The map types a `map<K, V>` field may be held in.
pub trait MapField: Default {
    /// The key type.
    type Key;
    /// The value type.
    type Value;
    /// Calls `f` for every entry, in the map's iteration order.
    fn for_each_entry(&self, f: impl FnMut(&Self::Key, &Self::Value));
    /// Inserts an entry, replacing an existing value for the key.
    fn insert_entry(&mut self, key: Self::Key, value: Self::Value);
    /// The memory one entry may cost at the map's peak, charged against the
    /// decode budget for each entry decoded: spare room, node or control
    /// bytes, and the old table while a hash map grows.
    fn entry_bytes() -> usize {
        2 * std::mem::size_of::<(Self::Key, Self::Value)>()
    }
}

impl<K: Ord, V> MapField for BTreeMap<K, V> {
    type Key = K;
    type Value = V;
    fn for_each_entry(&self, mut f: impl FnMut(&K, &V)) {
        for (k, v) in self {
            f(k, v);
        }
    }
    fn insert_entry(&mut self, key: K, value: V) {
        self.insert(key, value);
    }
}

impl<K: Eq + Hash, V, S: BuildHasher + Default> MapField for HashMap<K, V, S> {
    type Key = K;
    type Value = V;
    fn for_each_entry(&self, mut f: impl FnMut(&K, &V)) {
        for (k, v) in self {
            f(k, v);
        }
    }
    fn insert_entry(&mut self, key: K, value: V) {
        self.insert(key, value);
    }
    fn entry_bytes() -> usize {
        // A slot per bucket plus a control byte; right after growing the
        // table is 7/16 full and the old one (half the size) is still live
        // while entries move: about 3.4 slots per entry at the peak.
        4 * (std::mem::size_of::<(K, V)>() + 1)
    }
}

fn is_packable(wire_type: WireType) -> bool {
    !matches!(wire_type, WireType::Len)
}

// ---- lengths -----------------------------------------------------------------
//
// Each shape has a plain length (`len_*`, for `encoded_len`) and a cached one
// (`len_*_cached`, the length pass before a write); both share one body,
// generic over how a value is measured.

trait Sizer {
    fn value<K, T: FieldValue<K>>(&mut self, value: &T) -> usize;
}

struct Plain;

impl Sizer for Plain {
    fn value<K, T: FieldValue<K>>(&mut self, value: &T) -> usize {
        value.value_len()
    }
}

impl Sizer for SizeCache {
    fn value<K, T: FieldValue<K>>(&mut self, value: &T) -> usize {
        value.value_len_cached(self)
    }
}

fn len_implicit_in<K, T: FieldValue<K>>(tag: u32, value: &T, s: &mut impl Sizer) -> usize {
    if value.is_default() {
        0
    } else {
        len_always_in::<K, T>(tag, value, s)
    }
}

fn len_always_in<K, T: FieldValue<K>>(tag: u32, value: &T, s: &mut impl Sizer) -> usize {
    wire::key_len(tag) + s.value::<K, T>(value)
}

fn len_optional_in<K, T: FieldValue<K>>(tag: u32, value: &Option<T>, s: &mut impl Sizer) -> usize {
    value
        .as_ref()
        .map_or(0, |v| len_always_in::<K, T>(tag, v, s))
}

fn packed_body<K, T: FieldValue<K>>(values: &[T]) -> usize {
    values.iter().map(FieldValue::<K>::value_len).sum()
}

fn len_repeated_in<K, T: FieldValue<K>>(
    tag: u32,
    values: &[T],
    packed: bool,
    s: &mut impl Sizer,
) -> usize {
    if packed && is_packable(T::WIRE_TYPE) {
        if values.is_empty() {
            return 0;
        }
        let body = packed_body::<K, T>(values);
        wire::key_len(tag) + wire::varint_len(body as u64) + body
    } else {
        values
            .iter()
            .map(|v| len_always_in::<K, T>(tag, v, s))
            .sum()
    }
}

/// [`encode_implicit`]'s length.
pub fn len_implicit<K, T: FieldValue<K>>(tag: u32, value: &T) -> usize {
    len_implicit_in::<K, T>(tag, value, &mut Plain)
}

/// [`len_implicit`], recording embedded-message lengths.
pub fn len_implicit_cached<K, T: FieldValue<K>>(tag: u32, value: &T, c: &mut SizeCache) -> usize {
    len_implicit_in::<K, T>(tag, value, c)
}

/// [`encode_always`]'s length.
pub fn len_always<K, T: FieldValue<K>>(tag: u32, value: &T) -> usize {
    len_always_in::<K, T>(tag, value, &mut Plain)
}

/// [`len_always`], recording embedded-message lengths.
pub fn len_always_cached<K, T: FieldValue<K>>(tag: u32, value: &T, c: &mut SizeCache) -> usize {
    len_always_in::<K, T>(tag, value, c)
}

/// [`encode_optional`]'s length.
pub fn len_optional<K, T: FieldValue<K>>(tag: u32, value: &Option<T>) -> usize {
    len_optional_in::<K, T>(tag, value, &mut Plain)
}

/// [`len_optional`], recording embedded-message lengths.
pub fn len_optional_cached<K, T: FieldValue<K>>(
    tag: u32,
    value: &Option<T>,
    c: &mut SizeCache,
) -> usize {
    len_optional_in::<K, T>(tag, value, c)
}

/// [`encode_repeated`]'s length.
pub fn len_repeated<K, T: FieldValue<K>>(tag: u32, values: &[T], packed: bool) -> usize {
    len_repeated_in::<K, T>(tag, values, packed, &mut Plain)
}

/// [`len_repeated`], recording embedded-message lengths.
pub fn len_repeated_cached<K, T: FieldValue<K>>(
    tag: u32,
    values: &[T],
    packed: bool,
    c: &mut SizeCache,
) -> usize {
    len_repeated_in::<K, T>(tag, values, packed, c)
}

fn entry_len<KK, VK, K: FieldValue<KK>, V: FieldValue<VK>>(k: &K, v: &V) -> usize {
    1 + k.value_len() + 1 + v.value_len()
}

/// [`encode_map`]'s length.
pub fn len_map<KK, VK, M>(tag: u32, map: &M) -> usize
where
    M: MapField,
    M::Key: FieldValue<KK>,
    M::Value: FieldValue<VK>,
{
    let mut total = 0;
    map.for_each_entry(|k, v| {
        let entry = entry_len::<KK, VK, _, _>(k, v);
        total += wire::key_len(tag) + wire::varint_len(entry as u64) + entry;
    });
    total
}

/// [`len_map`], recording entry and embedded-message lengths.
pub fn len_map_cached<KK, VK, M>(tag: u32, map: &M, c: &mut SizeCache) -> usize
where
    M: MapField,
    M::Key: FieldValue<KK>,
    M::Value: FieldValue<VK>,
{
    let mut total = 0;
    map.for_each_entry(|k, v| {
        let slot = c.reserve();
        let entry = 1 + k.value_len_cached(c) + 1 + v.value_len_cached(c);
        c.set(slot, entry);
        total += wire::key_len(tag) + wire::varint_len(entry as u64) + entry;
    });
    total
}

/// [`encode_oneof`]'s length.
pub fn len_oneof<O: Oneof>(value: &Option<O>) -> usize {
    value.as_ref().map_or(0, Oneof::encoded_len)
}

/// [`len_oneof`], recording embedded-message lengths.
pub fn len_oneof_cached<O: Oneof>(value: &Option<O>, c: &mut SizeCache) -> usize {
    value.as_ref().map_or(0, |v| v.__encoded_len_cached(c))
}

// ---- encode ----------------------------------------------------------------
//
// The writers take the cache filled by the matching `len_*_cached` pass.

/// An implicit-presence field: written unless it holds the default.
pub fn encode_implicit<K, T: FieldValue<K>>(
    tag: u32,
    value: &T,
    buf: &mut Vec<u8>,
    c: &mut SizeCache,
) {
    if !value.is_default() {
        encode_always::<K, T>(tag, value, buf, c);
    }
}

/// A field that is always written (proto2 `required`, oneof members).
pub fn encode_always<K, T: FieldValue<K>>(
    tag: u32,
    value: &T,
    buf: &mut Vec<u8>,
    c: &mut SizeCache,
) {
    wire::encode_key(tag, T::WIRE_TYPE, buf);
    value.encode_value(buf, c);
}

/// An explicit-presence field: written when `Some`, even if the default.
pub fn encode_optional<K, T: FieldValue<K>>(
    tag: u32,
    value: &Option<T>,
    buf: &mut Vec<u8>,
    c: &mut SizeCache,
) {
    if let Some(v) = value {
        encode_always::<K, T>(tag, v, buf, c);
    }
}

/// A repeated field; numeric kinds are packed when `packed` is set.
pub fn encode_repeated<K, T: FieldValue<K>>(
    tag: u32,
    values: &[T],
    packed: bool,
    buf: &mut Vec<u8>,
    c: &mut SizeCache,
) {
    if packed && is_packable(T::WIRE_TYPE) {
        if values.is_empty() {
            return;
        }
        let body = packed_body::<K, T>(values);
        wire::encode_key(tag, WireType::Len, buf);
        wire::encode_varint(body as u64, buf);
        for v in values {
            v.encode_value(buf, c);
        }
    } else {
        for v in values {
            encode_always::<K, T>(tag, v, buf, c);
        }
    }
}

/// A map field: one length-delimited entry message (key = 1, value = 2) per
/// entry. Both key and value are always written.
pub fn encode_map<KK, VK, M>(tag: u32, map: &M, buf: &mut Vec<u8>, c: &mut SizeCache)
where
    M: MapField,
    M::Key: FieldValue<KK>,
    M::Value: FieldValue<VK>,
{
    map.for_each_entry(|k, v| {
        let entry = match c.next() {
            Some(len) => len,
            None => entry_len::<KK, VK, _, _>(k, v),
        };
        wire::encode_key(tag, WireType::Len, buf);
        wire::encode_varint(entry as u64, buf);
        encode_always::<KK, M::Key>(1, k, buf, c);
        encode_always::<VK, M::Value>(2, v, buf, c);
    });
}

/// A oneof field: the active variant, if any.
pub fn encode_oneof<O: Oneof>(value: &Option<O>, buf: &mut Vec<u8>, c: &mut SizeCache) {
    if let Some(v) = value {
        v.__encode_cached(buf, c);
    }
}

// ---- decode ----------------------------------------------------------------
//
// Each `merge_*` returns `Ok(false)` without consuming input when the wire type
// does not fit the field, so the caller keeps the field as unknown — what the
// reference implementations do. Memory a decode creates is charged against
// the reader's decoded-bytes budget before it is allocated.

/// A singular field (implicit presence or required).
pub fn merge_value<K, T: FieldValue<K>>(
    wire_type: WireType,
    value: &mut T,
    r: &mut Reader<'_>,
) -> Result<bool, ProtoError> {
    if wire_type != T::WIRE_TYPE {
        return Ok(false);
    }
    value.merge_value(r)?;
    Ok(true)
}

/// An explicit-presence field.
pub fn merge_optional<K, T: FieldValue<K>>(
    wire_type: WireType,
    value: &mut Option<T>,
    r: &mut Reader<'_>,
) -> Result<bool, ProtoError> {
    if wire_type != T::WIRE_TYPE {
        return Ok(false);
    }
    if value.is_none() {
        r.charge(T::default_heap_bytes())?;
    }
    value.get_or_insert_with(T::default).merge_value(r)?;
    Ok(true)
}

/// A repeated field; packed and unpacked encodings are both accepted.
pub fn merge_repeated<K, T: FieldValue<K>>(
    wire_type: WireType,
    values: &mut Vec<T>,
    r: &mut Reader<'_>,
) -> Result<bool, ProtoError> {
    if wire_type == T::WIRE_TYPE {
        r.charge_push(values, T::default_heap_bytes())?;
        let mark = r.required_mark();
        let mut v = T::default();
        v.merge_value(r)?;
        // The element is complete: check its required fields before it moves.
        r.settle_required(mark)?;
        values.push(v);
        Ok(true)
    } else if wire_type == WireType::Len && is_packable(T::WIRE_TYPE) {
        r.read_packed(|r| {
            while !r.is_empty() {
                r.take_entry()?;
                r.charge_push(values, 0)?;
                let mut v = T::default();
                v.merge_value(r)?;
                values.push(v);
            }
            Ok(())
        })?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// A map field: one entry message; a missing key or value is the default, a
/// repeated key keeps the last value.
pub fn merge_map<KK, VK, M>(
    wire_type: WireType,
    map: &mut M,
    r: &mut Reader<'_>,
) -> Result<bool, ProtoError>
where
    M: MapField,
    M::Key: FieldValue<KK>,
    M::Value: FieldValue<VK>,
{
    if wire_type != WireType::Len {
        return Ok(false);
    }
    // Tree and hash maps keep spare room (see `MapField::entry_bytes`).
    r.charge(M::entry_bytes() + M::Key::default_heap_bytes() + M::Value::default_heap_bytes())?;
    let mark = r.required_mark();
    let (k, v) = r.read_nested(|r| {
        let mut k = M::Key::default();
        let mut v = M::Value::default();
        while let Some((tag, wt)) = r.read_key()? {
            let used = match tag {
                1 => merge_value::<KK, _>(wt, &mut k, r)?,
                2 => merge_value::<VK, _>(wt, &mut v, r)?,
                _ => false,
            };
            if !used {
                r.skip_field(tag, wt)?;
            }
        }
        // The entry is complete: check its required fields before it moves.
        r.settle_required(mark)?;
        Ok((k, v))
    })?;
    map.insert_entry(k, v);
    Ok(true)
}

/// A oneof field.
pub fn merge_oneof<O: Oneof>(
    slot: &mut Option<O>,
    tag: u32,
    wire_type: WireType,
    r: &mut Reader<'_>,
) -> Result<bool, ProtoError> {
    O::merge_field(slot, tag, wire_type, r)
}

/// Prepares a oneof slot for a new variant of kind `K` held as `T`: charges
/// its heap and forgets the required-field records of everything the variant
/// it replaces owns. Used by the `Oneof` derive.
pub fn oneof_replace<O: Oneof, K, T: FieldValue<K>>(
    slot: &Option<O>,
    r: &mut Reader<'_>,
) -> Result<(), ProtoError> {
    forget_oneof(slot, r);
    r.charge(T::default_heap_bytes())
}

/// Forgets the required-field records of a message-typed field's value and
/// of everything it owns (used by the derives; see
/// `Message::__forget_required`).
pub fn forget_message<M: Message>(value: &M, r: &mut Reader<'_>) {
    value.__forget_required(r);
}

/// [`forget_message`] for an explicit-presence message field.
pub fn forget_optional_message<M: Message>(value: &Option<M>, r: &mut Reader<'_>) {
    if let Some(v) = value {
        v.__forget_required(r);
    }
}

/// [`forget_message`] for a oneof field's active variant.
pub fn forget_oneof<O: Oneof>(value: &Option<O>, r: &mut Reader<'_>) {
    if let Some(v) = value {
        if r.has_required() {
            v.__forget_required(r);
        }
    }
}

/// The wire type of kind `K` held as `T` (used by the `Oneof` derive).
pub const fn wire_type_of<K, T: FieldValue<K>>() -> WireType {
    T::WIRE_TYPE
}

/// Whether no number in `a` appears in `b` (checked at compile time by the
/// derive, to reject a oneof that reuses a field number of its message).
pub const fn tags_disjoint(a: &[u32], b: &[u32]) -> bool {
    let mut i = 0;
    while i < a.len() {
        let mut j = 0;
        while j < b.len() {
            if a[i] == b[j] {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

/// Whether an enum discriminant fits the `int32` protobuf enums are (checked
/// at compile time by the `Enumeration` derive).
pub const fn fits_i32(value: i128) -> bool {
    value >= i32::MIN as i128 && value <= i32::MAX as i128
}
