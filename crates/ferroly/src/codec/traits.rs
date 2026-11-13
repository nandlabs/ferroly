//! The `Encode` / `Decode` traits and their standard-library impls.
//!
//! # Strict and lenient scalar decoding
//!
//! Scalars decode in one of two modes, chosen by the entry point rather than by
//! the type:
//!
//! - **Lenient** — [`Decode::decode`] (the [`Value`] path). A string holding a
//!   scalar (`"42"`, `" true "`, `"1.5"`) decodes into the matching number or
//!   bool. This is what text-only sources need: XML content, environment
//!   variables, and `.properties` files carry every scalar as a string.
//! - **Strict** — [`Decode::decode_from`] (the [`Decoder`] path). A string is
//!   only ever a string. Typed formats (JSON, TOML) decode this way, so
//!   `"42"` is rejected where a number is expected.
//!
//! [`from_value_strict`] applies the strict rules to an already-parsed
//! [`Value`].

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, Hash};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ferroly::codec::{CodecError, Value};

/// A streaming output sink that a format (e.g. JSON) implements so that
/// [`Encode::encode_to`] can write a value directly, without building an
/// intermediate [`Value`] tree. The sink owns all punctuation/separators.
pub trait Encoder: Sized {
    /// Emits a null.
    fn encode_null(&mut self);
    /// Emits a boolean.
    fn encode_bool(&mut self, v: bool);
    /// Emits a signed integer.
    fn encode_i64(&mut self, v: i64);
    /// Emits an unsigned integer.
    fn encode_u64(&mut self, v: u64);
    /// Emits a floating-point number.
    fn encode_f64(&mut self, v: f64);
    /// Emits a string.
    fn encode_str(&mut self, v: &str);
    /// Emits a byte string.
    fn encode_bytes(&mut self, v: &[u8]);
    /// Begins a sequence of `len` items.
    fn begin_seq(&mut self, len: usize);
    /// Emits one sequence element (handling any separator).
    fn seq_entry<V: Encode + ?Sized>(&mut self, v: &V);
    /// Ends the current sequence.
    fn end_seq(&mut self);
    /// Begins a map of `len` entries.
    fn begin_map(&mut self, len: usize);
    /// Emits one map entry (handling any separator and the key).
    fn map_entry<V: Encode + ?Sized>(&mut self, key: &str, v: &V);
    /// Ends the current map.
    fn end_map(&mut self);
}

/// Converts a value into the [`Value`] model, or streams it to an [`Encoder`].
pub trait Encode {
    /// Produces the [`Value`] representation of `self`.
    fn encode(&self) -> Value;

    /// Streams `self` directly to `e`, bypassing the [`Value`] tree. The default
    /// replays `self.encode()`; concrete impls and the derive override it to
    /// write field by field.
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        emit_value(e, &self.encode());
    }
}

/// Walks a [`Value`] into an [`Encoder`] — the fallback path and the
/// implementation of `Encode for Value`.
pub fn emit_value<E: Encoder>(e: &mut E, value: &Value) {
    match value {
        Value::Null => e.encode_null(),
        Value::Bool(b) => e.encode_bool(*b),
        Value::Int(i) => e.encode_i64(*i),
        Value::UInt(u) => e.encode_u64(*u),
        Value::Float(f) => e.encode_f64(*f),
        Value::Str(s) => e.encode_str(s),
        Value::Bytes(b) => e.encode_bytes(b),
        Value::Array(a) => {
            e.begin_seq(a.len());
            for x in a {
                e.seq_entry(x);
            }
            e.end_seq();
        }
        Value::Object(o) => {
            e.begin_map(o.len());
            for (k, x) in o {
                e.map_entry(k, x);
            }
            e.end_map();
        }
    }
}

/// A streaming input source that a format (e.g. JSON) implements so that
/// [`Decode::decode_from`] can build a value directly, without materializing the
/// whole [`Value`] tree. Scalar leaves may still be pulled as a small [`Value`]
/// via [`decode_value`](Decoder::decode_value).
///
/// A `Decoder` is a **typed** source: the scalar impls decode strictly from it
/// (a string is never coerced into a number or bool). See [`Decode`] for the
/// strict/lenient split.
pub trait Decoder: Sized {
    /// Pulls the next value as a [`Value`] (used for scalar leaves and the
    /// fallback path). For a scalar this allocates nothing beyond the scalar.
    fn decode_value(&mut self) -> Result<Value, CodecError>;
    /// Pulls the next value as a string (single allocation; no double-copy).
    fn decode_string(&mut self) -> Result<String, CodecError>;
    /// Returns `true` if the next value is `null` (without consuming it).
    fn peek_null(&mut self) -> bool;
    /// Consumes a `null`.
    fn decode_null(&mut self) -> Result<(), CodecError>;
    /// Reads a sequence, invoking `f` positioned at each element.
    fn read_seq<F>(&mut self, f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self) -> Result<(), CodecError>;
    /// Reads a map, invoking `f` with each key positioned at its value.
    fn read_map<F>(&mut self, f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self, &str) -> Result<(), CodecError>;
    /// Skips the next value (an unknown field).
    fn skip_value(&mut self) -> Result<(), CodecError>;
}

/// Constructs a value from the [`Value`] model, or pulls it from a [`Decoder`].
///
/// # Strict and lenient scalar decoding
///
/// The two methods differ in how the scalar impls treat strings, and the
/// entry point — not the target type — picks which one runs:
///
/// - [`decode`](Decode::decode) is **lenient**: a string holding a scalar
///   (`"42"`, `" true "`, `"1.5"`) decodes into the matching number or bool,
///   and a lone value or `null` stands in for a one-element or empty `Vec`.
///   Text-only sources need this — XML content, environment variables and
///   `.properties` files carry every scalar as a string. `xml::decode`,
///   `yaml::decode` and `config` use it.
/// - [`decode_from`](Decode::decode_from) is **strict**: a string is only ever
///   a string, and a sequence must be a real array. Typed formats decode this
///   way — `json::decode` and `toml::decode` — so `"42"` is rejected where a
///   number is expected. [`from_value_strict`] applies the same rules to an
///   already-parsed [`Value`].
///
/// In both modes a float decodes into an integer type only when it is a whole
/// number that fits (`3.0` → `3`; `1.9` and `1e300` are errors).
pub trait Decode: Sized {
    /// Attempts to build `Self` from a [`Value`].
    ///
    /// This is the **lenient** path: the scalar impls also accept a string
    /// holding the scalar (`"42"`, `"true"`), because text-only sources (XML,
    /// environment variables, `.properties`) have no other way to carry one.
    /// Use [`from_value_strict`] to decode a [`Value`] without that coercion.
    fn decode(value: &Value) -> Result<Self, CodecError>;

    /// Builds `Self` directly from a [`Decoder`], bypassing the full [`Value`]
    /// tree. This is the **strict** path: the scalar impls reject a string
    /// where a number or bool is expected.
    ///
    /// The default pulls a [`Value`] and defers to [`decode`](Decode::decode),
    /// so a hand-written impl that only provides `decode` keeps `decode`'s
    /// behaviour for its own subtree; containers, the std scalars, and the
    /// derive override this method.
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        Self::decode(&d.decode_value()?)
    }
}

/// A strict [`Decoder`] over an already-parsed [`Value`] tree.
///
/// It walks the tree with the same typed rules a streaming format uses: arrays
/// must be arrays, and a string is never coerced into a number or bool. The
/// TOML codec decodes through it; [`from_value_strict`] is the shorthand.
#[derive(Debug)]
pub struct ValueDecoder<'a> {
    cur: &'a Value,
}

impl<'a> ValueDecoder<'a> {
    /// Creates a decoder positioned at `value`.
    pub fn new(value: &'a Value) -> Self {
        ValueDecoder { cur: value }
    }
}

impl<'a> Decoder for ValueDecoder<'a> {
    fn decode_value(&mut self) -> Result<Value, CodecError> {
        Ok(self.cur.clone())
    }

    fn decode_string(&mut self) -> Result<String, CodecError> {
        match self.cur {
            Value::Str(s) => Ok(s.clone()),
            _ => Err(CodecError::expected("string")),
        }
    }

    fn peek_null(&mut self) -> bool {
        self.cur.is_null()
    }

    fn decode_null(&mut self) -> Result<(), CodecError> {
        if self.cur.is_null() {
            Ok(())
        } else {
            Err(CodecError::expected("null"))
        }
    }

    fn read_seq<F>(&mut self, mut f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self) -> Result<(), CodecError>,
    {
        let parent = self.cur;
        let items = match parent {
            Value::Array(items) => items,
            _ => return Err(CodecError::expected("array")),
        };
        for item in items {
            self.cur = item;
            f(self)?;
        }
        self.cur = parent;
        Ok(())
    }

    fn read_map<F>(&mut self, mut f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self, &str) -> Result<(), CodecError>,
    {
        let parent = self.cur;
        let entries = match parent {
            Value::Object(entries) => entries,
            _ => return Err(CodecError::expected("object")),
        };
        for (key, value) in entries {
            self.cur = value;
            f(self, key)?;
        }
        self.cur = parent;
        Ok(())
    }

    fn skip_value(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
}

/// Decodes a [`Value`] **strictly**: unlike [`Decode::decode`], a string is not
/// accepted where a number or bool is expected, and a sequence must be an
/// array.
///
/// ```
/// use ferroly::codec::{from_value_strict, Decode, Value};
///
/// let text = Value::Str("42".into());
/// assert_eq!(i32::decode(&text).unwrap(), 42);          // lenient
/// assert!(from_value_strict::<i32>(&text).is_err());    // strict
/// assert_eq!(from_value_strict::<i32>(&Value::Int(42)).unwrap(), 42);
/// ```
pub fn from_value_strict<T: Decode>(value: &Value) -> Result<T, CodecError> {
    T::decode_from(&mut ValueDecoder::new(value))
}

// ---- scalar conversion ---------------------------------------------------

// The exclusive float bounds of each integer range (2^63, 2^64, 2^127, 2^128),
// all exactly representable as `f64`.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
const TWO_POW_127: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
const TWO_POW_128: f64 = 340_282_366_920_938_463_463_374_607_431_768_211_456.0;

/// Checks that `f` is a whole number in `[min, max_exclusive)`. A float with a
/// fractional part is not an integer at all; a whole one outside the range is
/// out of range for `ty`.
fn integral(
    f: f64,
    min: f64,
    max_exclusive: f64,
    what: &'static str,
    ty: &'static str,
) -> Result<f64, CodecError> {
    if !f.is_finite() || f.fract() != 0.0 {
        return Err(CodecError::expected(what));
    }
    if f < min || f >= max_exclusive {
        return Err(CodecError::out_of_range(ty));
    }
    Ok(f)
}

fn signed_from(value: &Value, lenient: bool, ty: &'static str) -> Result<i64, CodecError> {
    match value {
        Value::Int(i) => Ok(*i),
        Value::UInt(u) => i64::try_from(*u).map_err(|_| CodecError::out_of_range(ty)),
        Value::Float(f) => Ok(integral(*f, -TWO_POW_63, TWO_POW_63, "integer", ty)? as i64),
        Value::Str(s) if lenient => s
            .trim()
            .parse::<i64>()
            .map_err(|_| CodecError::expected("integer")),
        _ => Err(CodecError::expected("integer")),
    }
}

fn unsigned_from(value: &Value, lenient: bool, ty: &'static str) -> Result<u64, CodecError> {
    match value {
        Value::UInt(u) => Ok(*u),
        Value::Int(i) if *i >= 0 => Ok(*i as u64),
        Value::Float(f) => Ok(integral(*f, 0.0, TWO_POW_64, "unsigned integer", ty)? as u64),
        Value::Str(s) if lenient => s
            .trim()
            .parse::<u64>()
            .map_err(|_| CodecError::expected("unsigned integer")),
        _ => Err(CodecError::expected("unsigned integer")),
    }
}

/// 128-bit integers have no [`Value`] variant, so values beyond 64 bits travel
/// as decimal strings — which is why a string is accepted here in both modes
/// (lenient mode additionally trims surrounding whitespace).
fn signed128_from(value: &Value, lenient: bool) -> Result<i128, CodecError> {
    match value {
        Value::Int(i) => Ok(i128::from(*i)),
        Value::UInt(u) => Ok(i128::from(*u)),
        Value::Float(f) => Ok(integral(*f, -TWO_POW_127, TWO_POW_127, "integer", "i128")? as i128),
        Value::Str(s) => {
            let text = if lenient { s.trim() } else { s.as_str() };
            text.parse::<i128>()
                .map_err(|_| CodecError::expected("integer"))
        }
        _ => Err(CodecError::expected("integer")),
    }
}

fn unsigned128_from(value: &Value, lenient: bool) -> Result<u128, CodecError> {
    match value {
        Value::UInt(u) => Ok(u128::from(*u)),
        Value::Int(i) if *i >= 0 => Ok(*i as u128),
        Value::Float(f) => Ok(integral(*f, 0.0, TWO_POW_128, "unsigned integer", "u128")? as u128),
        Value::Str(s) => {
            let text = if lenient { s.trim() } else { s.as_str() };
            text.parse::<u128>()
                .map_err(|_| CodecError::expected("unsigned integer"))
        }
        _ => Err(CodecError::expected("unsigned integer")),
    }
}

fn float_from(value: &Value, lenient: bool) -> Result<f64, CodecError> {
    match value {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        Value::UInt(u) => Ok(*u as f64),
        Value::Str(s) if lenient => s
            .trim()
            .parse::<f64>()
            .map_err(|_| CodecError::expected("number")),
        _ => Err(CodecError::expected("number")),
    }
}

fn bool_from(value: &Value, lenient: bool) -> Result<bool, CodecError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Str(s) if lenient => match s.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(CodecError::expected("bool")),
        },
        _ => Err(CodecError::expected("bool")),
    }
}

// ---- primitives ----------------------------------------------------------

impl Encode for bool {
    fn encode(&self) -> Value {
        Value::Bool(*self)
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_bool(*self);
    }
}
impl Decode for bool {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        bool_from(value, true)
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        bool_from(&d.decode_value()?, false)
    }
}

macro_rules! impl_signed {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode(&self) -> Value { Value::Int(*self as i64) }
            fn encode_to<E: Encoder>(&self, e: &mut E) { e.encode_i64(*self as i64); }
        }
        impl Decode for $t {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                let n = signed_from(value, true, stringify!($t))?;
                <$t>::try_from(n).map_err(|_| CodecError::out_of_range(stringify!($t)))
            }
            fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
                let n = signed_from(&d.decode_value()?, false, stringify!($t))?;
                <$t>::try_from(n).map_err(|_| CodecError::out_of_range(stringify!($t)))
            }
        }
    )*};
}
impl_signed!(i8, i16, i32, i64, isize);

macro_rules! impl_unsigned {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode(&self) -> Value { Value::UInt(*self as u64) }
            fn encode_to<E: Encoder>(&self, e: &mut E) { e.encode_u64(*self as u64); }
        }
        impl Decode for $t {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                let n = unsigned_from(value, true, stringify!($t))?;
                <$t>::try_from(n).map_err(|_| CodecError::out_of_range(stringify!($t)))
            }
            fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
                let n = unsigned_from(&d.decode_value()?, false, stringify!($t))?;
                <$t>::try_from(n).map_err(|_| CodecError::out_of_range(stringify!($t)))
            }
        }
    )*};
}
impl_unsigned!(u8, u16, u32, u64, usize);

// 128-bit integers: a value that fits 64 bits encodes as a number; a wider one
// encodes as a decimal string, since the `Value` model (and most JSON readers)
// stop at 64 bits.
impl Encode for i128 {
    fn encode(&self) -> Value {
        if let Ok(i) = i64::try_from(*self) {
            Value::Int(i)
        } else if let Ok(u) = u64::try_from(*self) {
            Value::UInt(u)
        } else {
            Value::Str(self.to_string())
        }
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        if let Ok(i) = i64::try_from(*self) {
            e.encode_i64(i);
        } else if let Ok(u) = u64::try_from(*self) {
            e.encode_u64(u);
        } else {
            e.encode_str(&self.to_string());
        }
    }
}
impl Decode for i128 {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        signed128_from(value, true)
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        signed128_from(&d.decode_value()?, false)
    }
}

impl Encode for u128 {
    fn encode(&self) -> Value {
        match u64::try_from(*self) {
            Ok(u) => Value::UInt(u),
            Err(_) => Value::Str(self.to_string()),
        }
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        match u64::try_from(*self) {
            Ok(u) => e.encode_u64(u),
            Err(_) => e.encode_str(&self.to_string()),
        }
    }
}
impl Decode for u128 {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        unsigned128_from(value, true)
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        unsigned128_from(&d.decode_value()?, false)
    }
}

macro_rules! impl_float {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode(&self) -> Value { Value::Float(*self as f64) }
            fn encode_to<E: Encoder>(&self, e: &mut E) { e.encode_f64(*self as f64); }
        }
        impl Decode for $t {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                float_from(value, true).map(|v| v as $t)
            }
            fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
                float_from(&d.decode_value()?, false).map(|v| v as $t)
            }
        }
    )*};
}
impl_float!(f32, f64);

impl Encode for String {
    fn encode(&self) -> Value {
        Value::Str(self.clone())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_str(self);
    }
}
impl Decode for String {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| CodecError::expected("string"))
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        d.decode_string()
    }
}

impl Encode for str {
    fn encode(&self) -> Value {
        Value::Str(self.to_string())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_str(self);
    }
}

/// A reference encodes as the value it points to (`&str`, `&[T]`, `&T`).
impl<T: Encode + ?Sized> Encode for &T {
    fn encode(&self) -> Value {
        (**self).encode()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        (**self).encode_to(e);
    }
}

impl Encode for char {
    fn encode(&self) -> Value {
        Value::Str(self.to_string())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        let mut buf = [0u8; 4];
        e.encode_str(self.encode_utf8(&mut buf));
    }
}
impl Decode for char {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        value
            .as_str()
            .and_then(|s| s.chars().next().filter(|_| s.chars().count() == 1))
            .ok_or_else(|| CodecError::expected("single-character string"))
    }
}

// ---- containers ----------------------------------------------------------

impl<T: Encode> Encode for Option<T> {
    fn encode(&self) -> Value {
        match self {
            Some(v) => v.encode(),
            None => Value::Null,
        }
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        match self {
            Some(v) => v.encode_to(e),
            None => e.encode_null(),
        }
    }
}
impl<T: Decode> Decode for Option<T> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        match value {
            Value::Null => Ok(None),
            other => Ok(Some(T::decode(other)?)),
        }
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        if d.peek_null() {
            d.decode_null()?;
            Ok(None)
        } else {
            Ok(Some(T::decode_from(d)?))
        }
    }
}

impl<T: Encode> Encode for [T] {
    fn encode(&self) -> Value {
        Value::Array(self.iter().map(Encode::encode).collect())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_seq(self.len());
        for v in self {
            e.seq_entry(v);
        }
        e.end_seq();
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn encode(&self) -> Value {
        self.as_slice().encode()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        self.as_slice().encode_to(e);
    }
}
impl<T: Decode> Decode for Vec<T> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        // Tolerant `Value`-tree decoding so formats without a distinct array
        // syntax round-trip: `Null` (an absent/empty element) → empty vec, and a
        // lone scalar/object → one-element vec. This is what lets XML's
        // repeated-sibling convention reconstruct 0- and 1-element arrays, which
        // are otherwise indistinguishable from "absent" and "scalar". The strict
        // `decode_from` path (JSON, TOML) still requires a real array.
        match value {
            Value::Null => Ok(Vec::new()),
            Value::Array(a) => a.iter().map(T::decode).collect(),
            other => Ok(vec![T::decode(other)?]),
        }
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut out = Vec::new();
        d.read_seq(|d| {
            out.push(T::decode_from(d)?);
            Ok(())
        })?;
        Ok(out)
    }
}

/// A fixed-size array encodes as a sequence and decodes from a sequence of
/// exactly `N` elements.
impl<T: Encode, const N: usize> Encode for [T; N] {
    fn encode(&self) -> Value {
        self.as_slice().encode()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        self.as_slice().encode_to(e);
    }
}
impl<T: Decode, const N: usize> Decode for [T; N] {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        <[T; N]>::try_from(Vec::<T>::decode(value)?)
            .map_err(|_| CodecError::expected("array of the declared length"))
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        <[T; N]>::try_from(Vec::<T>::decode_from(d)?)
            .map_err(|_| CodecError::expected("array of the declared length"))
    }
}

/// Tuples encode as fixed-length sequences: `(1, "a")` → `[1, "a"]`.
macro_rules! impl_tuple {
    ($len:literal; $($idx:tt $name:ident),+) => {
        impl<$($name: Encode),+> Encode for ($($name,)+) {
            fn encode(&self) -> Value {
                Value::Array(vec![$(self.$idx.encode()),+])
            }
            fn encode_to<E: Encoder>(&self, e: &mut E) {
                e.begin_seq($len);
                $(e.seq_entry(&self.$idx);)+
                e.end_seq();
            }
        }
        impl<$($name: Decode),+> Decode for ($($name,)+) {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                const WHAT: &str = concat!("array of ", $len, " element(s)");
                // Like `Vec`, tolerate a lone value standing in for a
                // one-element sequence (how XML writes it).
                let items: &[Value] = match value {
                    Value::Array(items) => items,
                    other if $len == 1 => std::slice::from_ref(other),
                    _ => return Err(CodecError::expected(WHAT)),
                };
                if items.len() != $len {
                    return Err(CodecError::expected(WHAT));
                }
                Ok(($($name::decode(&items[$idx])?,)+))
            }
            fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
                const WHAT: &str = concat!("array of ", $len, " element(s)");
                let mut slots: ($(Option<$name>,)+) = ($(None::<$name>,)+);
                let mut next = 0usize;
                d.read_seq(|d| {
                    match next {
                        $($idx => slots.$idx = Some($name::decode_from(d)?),)+
                        _ => return Err(CodecError::expected(WHAT)),
                    }
                    next += 1;
                    Ok(())
                })?;
                Ok(($(slots.$idx.ok_or_else(|| CodecError::expected(WHAT))?,)+))
            }
        }
    };
}
impl_tuple!(1; 0 T0);
impl_tuple!(2; 0 T0, 1 T1);
impl_tuple!(3; 0 T0, 1 T1, 2 T2);
impl_tuple!(4; 0 T0, 1 T1, 2 T2, 3 T3);
impl_tuple!(5; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4);
impl_tuple!(6; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5);
impl_tuple!(7; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6);
impl_tuple!(8; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7);
impl_tuple!(9; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8);
impl_tuple!(10; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8, 9 T9);
impl_tuple!(11; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8, 9 T9, 10 T10);
impl_tuple!(12; 0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8, 9 T9, 10 T10, 11 T11);

macro_rules! impl_pointer {
    ($($ptr:ident),*) => {$(
        impl<T: Encode + ?Sized> Encode for $ptr<T> {
            fn encode(&self) -> Value {
                (**self).encode()
            }
            fn encode_to<E: Encoder>(&self, e: &mut E) {
                (**self).encode_to(e);
            }
        }
        impl<T: Decode> Decode for $ptr<T> {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                Ok($ptr::new(T::decode(value)?))
            }
            fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
                Ok($ptr::new(T::decode_from(d)?))
            }
        }
        impl Decode for $ptr<str> {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                value.as_str().map($ptr::from).ok_or_else(|| CodecError::expected("string"))
            }
            fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
                Ok($ptr::from(d.decode_string()?))
            }
        }
    )*};
}
impl_pointer!(Box, Rc, Arc);

/// A `Cow` encodes as its borrowed form and decodes into the owned form.
impl<T> Encode for Cow<'_, T>
where
    T: ToOwned + Encode + ?Sized,
{
    fn encode(&self) -> Value {
        self.as_ref().encode()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        self.as_ref().encode_to(e);
    }
}
impl<T> Decode for Cow<'_, T>
where
    T: ToOwned + ?Sized,
    T::Owned: Decode,
{
    fn decode(value: &Value) -> Result<Self, CodecError> {
        Ok(Cow::Owned(<T::Owned>::decode(value)?))
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        Ok(Cow::Owned(<T::Owned>::decode_from(d)?))
    }
}

impl<V: Encode> Encode for HashMap<String, V> {
    fn encode(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.clone(), v.encode())).collect())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_map(self.len());
        for (k, v) in self {
            e.map_entry(k, v);
        }
        e.end_map();
    }
}
impl<V: Decode> Decode for HashMap<String, V> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        let obj = value
            .as_object()
            .ok_or_else(|| CodecError::expected("object"))?;
        obj.iter()
            .map(|(k, v)| Ok((k.clone(), V::decode(v)?)))
            .collect()
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut out = HashMap::new();
        d.read_map(|d, key| {
            out.insert(key.to_string(), V::decode_from(d)?);
            Ok(())
        })?;
        Ok(out)
    }
}

/// A `BTreeMap` encodes its entries in key order, so the output is stable.
impl<V: Encode> Encode for BTreeMap<String, V> {
    fn encode(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.clone(), v.encode())).collect())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_map(self.len());
        for (k, v) in self {
            e.map_entry(k, v);
        }
        e.end_map();
    }
}
impl<V: Decode> Decode for BTreeMap<String, V> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        let obj = value
            .as_object()
            .ok_or_else(|| CodecError::expected("object"))?;
        obj.iter()
            .map(|(k, v)| Ok((k.clone(), V::decode(v)?)))
            .collect()
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut out = BTreeMap::new();
        d.read_map(|d, key| {
            out.insert(key.to_string(), V::decode_from(d)?);
            Ok(())
        })?;
        Ok(out)
    }
}

/// Sets encode as sequences (a `HashSet` in arbitrary order, a `BTreeSet` in
/// sorted order) and decode from sequences, collapsing duplicates.
impl<T: Encode, S> Encode for HashSet<T, S> {
    fn encode(&self) -> Value {
        Value::Array(self.iter().map(Encode::encode).collect())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_seq(self.len());
        for v in self {
            e.seq_entry(v);
        }
        e.end_seq();
    }
}
impl<T, S> Decode for HashSet<T, S>
where
    T: Decode + Eq + Hash,
    S: BuildHasher + Default,
{
    fn decode(value: &Value) -> Result<Self, CodecError> {
        Ok(Vec::<T>::decode(value)?.into_iter().collect())
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut out = HashSet::with_hasher(S::default());
        d.read_seq(|d| {
            out.insert(T::decode_from(d)?);
            Ok(())
        })?;
        Ok(out)
    }
}

impl<T: Encode> Encode for BTreeSet<T> {
    fn encode(&self) -> Value {
        Value::Array(self.iter().map(Encode::encode).collect())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_seq(self.len());
        for v in self {
            e.seq_entry(v);
        }
        e.end_seq();
    }
}
impl<T: Decode + Ord> Decode for BTreeSet<T> {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        Ok(Vec::<T>::decode(value)?.into_iter().collect())
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut out = BTreeSet::new();
        d.read_seq(|d| {
            out.insert(T::decode_from(d)?);
            Ok(())
        })?;
        Ok(out)
    }
}

impl Encode for () {
    fn encode(&self) -> Value {
        Value::Null
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_null();
    }
}
impl Decode for () {
    fn decode(_value: &Value) -> Result<Self, CodecError> {
        Ok(())
    }
}

impl Encode for Value {
    fn encode(&self) -> Value {
        self.clone()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        emit_value(e, self);
    }
}
impl Decode for Value {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        Ok(value.clone())
    }
}

// ---- std value types ------------------------------------------------------

/// A [`Duration`] encodes as `{ "secs": u64, "nanos": u32 }`. It decodes from
/// that object, or from a bare non-negative number of seconds (`30`, `1.5`).
impl Encode for Duration {
    fn encode(&self) -> Value {
        Value::Object(vec![
            ("secs".to_string(), Value::UInt(self.as_secs())),
            (
                "nanos".to_string(),
                Value::UInt(u64::from(self.subsec_nanos())),
            ),
        ])
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_map(2);
        e.map_entry("secs", &self.as_secs());
        e.map_entry("nanos", &self.subsec_nanos());
        e.end_map();
    }
}

fn duration_from(value: &Value, lenient: bool) -> Result<Duration, CodecError> {
    const WHAT: &str = "duration ({secs, nanos} or seconds)";
    let seconds = |f: f64| Duration::try_from_secs_f64(f).map_err(|_| CodecError::expected(WHAT));
    match value {
        Value::Object(entries) => {
            let mut secs = None;
            let mut nanos = 0u64;
            for (key, v) in entries {
                match key.as_str() {
                    "secs" => secs = Some(unsigned_from(v, lenient, "u64")?),
                    "nanos" => nanos = unsigned_from(v, lenient, "u32")?,
                    _ => {}
                }
            }
            let secs = secs.ok_or_else(|| CodecError::missing_field("secs"))?;
            if nanos >= 1_000_000_000 {
                return Err(CodecError::out_of_range("nanos"));
            }
            Ok(Duration::new(secs, nanos as u32))
        }
        Value::UInt(u) => Ok(Duration::from_secs(*u)),
        Value::Int(i) if *i >= 0 => Ok(Duration::from_secs(*i as u64)),
        Value::Float(f) => seconds(*f),
        Value::Str(s) if lenient => {
            let text = s.trim();
            match text.parse::<u64>() {
                Ok(secs) => Ok(Duration::from_secs(secs)),
                Err(_) => seconds(
                    text.parse::<f64>()
                        .map_err(|_| CodecError::expected(WHAT))?,
                ),
            }
        }
        _ => Err(CodecError::expected(WHAT)),
    }
}

impl Decode for Duration {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        duration_from(value, true)
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        duration_from(&d.decode_value()?, false)
    }
}

/// Paths encode as strings. A path that is not valid UTF-8 is encoded lossily
/// (invalid sequences become U+FFFD), so it will not round-trip.
impl Encode for Path {
    fn encode(&self) -> Value {
        Value::Str(self.to_string_lossy().into_owned())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_str(&self.to_string_lossy());
    }
}
impl Encode for PathBuf {
    fn encode(&self) -> Value {
        self.as_path().encode()
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        self.as_path().encode_to(e);
    }
}
impl Decode for PathBuf {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        value
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(|| CodecError::expected("path string"))
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        Ok(PathBuf::from(d.decode_string()?))
    }
}

/// Network address types encode as their standard text form (`"127.0.0.1"`,
/// `"[::1]:8080"`) and decode by parsing it.
macro_rules! impl_text_form {
    ($($t:ty => $what:literal),* $(,)?) => {$(
        impl Encode for $t {
            fn encode(&self) -> Value {
                Value::Str(self.to_string())
            }
            fn encode_to<E: Encoder>(&self, e: &mut E) {
                e.encode_str(&self.to_string());
            }
        }
        impl Decode for $t {
            fn decode(value: &Value) -> Result<Self, CodecError> {
                value
                    .as_str()
                    .and_then(|s| s.trim().parse().ok())
                    .ok_or_else(|| CodecError::expected($what))
            }
            fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
                d.decode_string()?
                    .parse()
                    .map_err(|_| CodecError::expected($what))
            }
        }
    )*};
}
impl_text_form!(
    IpAddr => "IP address",
    Ipv4Addr => "IPv4 address",
    Ipv6Addr => "IPv6 address",
    SocketAddr => "socket address",
    SocketAddrV4 => "IPv4 socket address",
    SocketAddrV6 => "IPv6 socket address",
);
