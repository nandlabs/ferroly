//! The [`Message`], [`Oneof`] and [`Enumeration`] traits, and the open-enum
//! field type [`EnumValue`].

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use super::error::ProtoError;
use super::field::SizeCache;
use super::limits::Limits;
use super::wire::{self, Reader, WireType};

/// A protobuf message: a type with a wire encoding.
///
/// Usually derived with `#[derive(ferroly::proto::Message)]` (see the module
/// docs for the attributes) or generated from a `.proto` file by
/// [`codegen`](super::codegen). A hand-written implementation supplies the
/// three required methods; everything else is provided.
///
/// ```
/// use ferroly::proto::Message;
///
/// #[derive(ferroly::proto::Message, Default, PartialEq, Debug)]
/// struct Test1 {
///     #[proto(tag = 1)]
///     a: i32,
/// }
///
/// let bytes = Test1 { a: 150 }.encode_to_vec();
/// assert_eq!(bytes, [0x08, 0x96, 0x01]);
/// assert_eq!(Test1::decode(&bytes).unwrap(), Test1 { a: 150 });
/// ```
pub trait Message: Default {
    /// Heap bytes a fresh `Self::default()` owns beyond `size_of::<Self>()`:
    /// 0 for a derived message, the boxed size for `Box<M>`. The decode budget
    /// ([`Limits::max_decoded_bytes`]) charges it for each value decoding
    /// creates; a hand-written message whose default allocates should set it.
    const DEFAULT_HEAP_BYTES: usize = 0;

    /// Appends the message's fields (no length prefix) to `buf`.
    fn encode_raw(&self, buf: &mut Vec<u8>);

    /// The exact number of bytes [`encode_raw`](Self::encode_raw) writes.
    fn encoded_len(&self) -> usize;

    /// Reads fields from `r` until the end of the current message, merging
    /// each into `self`: scalars are replaced, embedded messages are merged,
    /// repeated fields and maps are appended to.
    ///
    /// A derived message reports missing proto2 `required` fields to the
    /// reader rather than failing here, since a later occurrence of the same
    /// sub-message may still carry them; [`Reader::finish_required`] checks
    /// them once the input is read (the `decode` functions do this).
    fn merge_from(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError>;

    /// The length pass of an encode: [`encoded_len`](Self::encoded_len),
    /// recording the length of every embedded message in `cache` so the write
    /// pass need not measure subtrees again. Derived code overrides it; not a
    /// stable API.
    #[doc(hidden)]
    fn __encoded_len_cached(&self, cache: &mut SizeCache) -> usize {
        let _ = cache;
        self.encoded_len()
    }

    /// The write pass of an encode, reading lengths back from `cache`.
    /// Derived code overrides it; not a stable API.
    #[doc(hidden)]
    fn __encode_cached(&self, buf: &mut Vec<u8>, cache: &mut SizeCache) {
        let _ = cache;
        self.encode_raw(buf);
    }

    /// Drops the reader's required-field records of `self` and of every
    /// singular sub-message and oneof variant it owns: decoding is about to
    /// drop `self` (a oneof variant being replaced), and a later value at the
    /// same address must not inherit them. Derived code overrides it; not a
    /// stable API.
    #[doc(hidden)]
    fn __forget_required(&self, r: &mut Reader<'_>) {
        let _ = r;
    }

    /// Appends the encoded message to `buf`.
    fn encode(&self, buf: &mut Vec<u8>) {
        let mut cache = SizeCache::new();
        let len = self.__encoded_len_cached(&mut cache);
        buf.reserve(len);
        self.__encode_cached(buf, &mut cache);
    }

    /// The encoded message as a new vector.
    fn encode_to_vec(&self) -> Vec<u8> {
        let mut cache = SizeCache::new();
        let len = self.__encoded_len_cached(&mut cache);
        let mut buf = Vec::with_capacity(len);
        self.__encode_cached(&mut buf, &mut cache);
        buf
    }

    /// Appends the message prefixed with its length as a varint (the framing
    /// used for embedded messages and for streams of messages).
    fn encode_length_delimited(&self, buf: &mut Vec<u8>) {
        let mut cache = SizeCache::new();
        let len = self.__encoded_len_cached(&mut cache);
        buf.reserve(len + wire::varint_len(len as u64));
        wire::encode_varint(len as u64, buf);
        self.__encode_cached(buf, &mut cache);
    }

    /// Decodes a message with the [default limits](Limits::DEFAULT).
    fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::decode_with(bytes, Limits::DEFAULT)
    }

    /// Decodes a message with an explicit budget.
    fn decode_with(bytes: &[u8], limits: Limits) -> Result<Self, ProtoError> {
        let mut r = Reader::with_limits(bytes, limits)?;
        let mut message = Self::default();
        message.merge_from(&mut r)?;
        r.finish_required()?;
        Ok(message)
    }

    /// Decodes a message written by
    /// [`encode_length_delimited`](Self::encode_length_delimited); the whole
    /// input must be that one message. Uses the default limits.
    fn decode_length_delimited(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::decode_length_delimited_with(bytes, Limits::DEFAULT)
    }

    /// [`decode_length_delimited`](Self::decode_length_delimited) with an
    /// explicit budget.
    fn decode_length_delimited_with(bytes: &[u8], limits: Limits) -> Result<Self, ProtoError> {
        let mut r = Reader::with_limits(bytes, limits)?;
        let mut message = Self::default();
        r.read_nested(|r| message.merge_from(r))?;
        if !r.is_empty() {
            return Err(ProtoError::Malformed(
                "trailing bytes after a length-delimited message",
            ));
        }
        r.finish_required()?;
        Ok(message)
    }

    /// Merges an encoded message into `self` (as if the two encodings were
    /// concatenated), with the default limits.
    ///
    /// proto2 `required` fields of `self` and its singular sub-messages are
    /// not checked, since `self` may already hold them; new elements of
    /// repeated and map fields are.
    fn merge(&mut self, bytes: &[u8]) -> Result<(), ProtoError> {
        self.merge_with(bytes, Limits::DEFAULT)
    }

    /// [`merge`](Self::merge) with an explicit budget.
    fn merge_with(&mut self, bytes: &[u8], limits: Limits) -> Result<(), ProtoError> {
        let mut r = Reader::with_limits(bytes, limits)?;
        self.merge_from(&mut r)?;
        r.discard_required();
        Ok(())
    }

    /// Resets every field to its default.
    fn clear(&mut self) {
        *self = Self::default();
    }
}

impl<M: Message> Message for Box<M> {
    const DEFAULT_HEAP_BYTES: usize = std::mem::size_of::<M>() + M::DEFAULT_HEAP_BYTES;
    fn encode_raw(&self, buf: &mut Vec<u8>) {
        (**self).encode_raw(buf);
    }
    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }
    fn merge_from(&mut self, r: &mut Reader<'_>) -> Result<(), ProtoError> {
        (**self).merge_from(r)
    }
    fn __encoded_len_cached(&self, cache: &mut SizeCache) -> usize {
        (**self).__encoded_len_cached(cache)
    }
    fn __encode_cached(&self, buf: &mut Vec<u8>, cache: &mut SizeCache) {
        (**self).__encode_cached(buf, cache);
    }
    fn __forget_required(&self, r: &mut Reader<'_>) {
        (**self).__forget_required(r);
    }
}

/// A `oneof`: an enum whose variants are the alternative fields, held in a
/// message as `Option<TheEnum>` marked `#[proto(oneof)]`.
///
/// Derive it with `#[derive(ferroly::proto::Oneof)]`; each variant is a
/// newtype carrying `#[proto(tag = N)]` and, where needed, a type attribute
/// such as `sint32`.
pub trait Oneof: Sized {
    /// The field numbers of every variant.
    const TAGS: &'static [u32];

    /// Appends the active variant as a field (always written, even when its
    /// value is the default — a oneof member has explicit presence).
    fn encode_raw(&self, buf: &mut Vec<u8>);

    /// The number of bytes [`encode_raw`](Self::encode_raw) writes.
    fn encoded_len(&self) -> usize;

    /// The length pass of an encode (see [`Message`]); not a stable API.
    #[doc(hidden)]
    fn __encoded_len_cached(&self, cache: &mut SizeCache) -> usize {
        let _ = cache;
        self.encoded_len()
    }

    /// The write pass of an encode (see [`Message`]); not a stable API.
    #[doc(hidden)]
    fn __encode_cached(&self, buf: &mut Vec<u8>, cache: &mut SizeCache) {
        let _ = cache;
        self.encode_raw(buf);
    }

    /// Drops the reader's required-field records of the active variant and
    /// of everything it owns (see [`Message`]); not a stable API.
    #[doc(hidden)]
    fn __forget_required(&self, r: &mut Reader<'_>) {
        let _ = r;
    }

    /// Reads field `tag` into `slot` when it belongs to this oneof, returning
    /// whether it did. A message variant arriving twice is merged; a different
    /// variant replaces the active one.
    fn merge_field(
        slot: &mut Option<Self>,
        tag: u32,
        wire_type: WireType,
        r: &mut Reader<'_>,
    ) -> Result<bool, ProtoError>;
}

/// A protobuf enum: a fieldless Rust enum whose discriminants are the enum's
/// numbers.
///
/// Derive it with `#[derive(ferroly::proto::Enumeration)]`, and hold it in a
/// message as [`EnumValue<TheEnum>`] so values this program does not know
/// survive a decode / encode round trip (proto3 "open" enums).
///
/// ```
/// use ferroly::proto::Enumeration;
///
/// #[derive(ferroly::proto::Enumeration, Clone, Copy, PartialEq, Debug)]
/// enum Corpus {
///     #[proto(name = "CORPUS_UNSPECIFIED")]
///     Unspecified = 0,
///     #[proto(name = "CORPUS_WEB")]
///     Web = 1,
/// }
///
/// assert_eq!(Corpus::Web.to_i32(), 1);
/// assert_eq!(Corpus::from_i32(1), Some(Corpus::Web));
/// assert_eq!(Corpus::from_i32(9), None);
/// assert_eq!(Corpus::Web.proto_name(), "CORPUS_WEB");
/// assert_eq!(Corpus::from_proto_name("CORPUS_WEB"), Some(Corpus::Web));
/// ```
pub trait Enumeration: Copy + 'static {
    /// The enum's number.
    fn to_i32(self) -> i32;

    /// The variant with `value`, if this program knows it.
    fn from_i32(value: i32) -> Option<Self>;

    /// The variant's name as written in the `.proto` file.
    fn proto_name(self) -> &'static str;

    /// The variant with the `.proto` name `name`.
    fn from_proto_name(name: &str) -> Option<Self>;
}

/// An open-enum field: the raw number on the wire, viewed as `E` when known.
///
/// Unknown numbers are kept as-is and written back unchanged, as proto3
/// requires. The default is 0 (the first value of every proto3 enum).
///
/// ```
/// use ferroly::proto::{EnumValue, Enumeration};
///
/// #[derive(ferroly::proto::Enumeration, Clone, Copy, PartialEq, Debug)]
/// enum Color { Red = 0, Green = 1 }
///
/// let green: EnumValue<Color> = Color::Green.into();
/// assert_eq!(green.known(), Some(Color::Green));
/// assert!(green == Color::Green);
///
/// let future = EnumValue::<Color>::from_raw(7);
/// assert_eq!(future.known(), None);
/// assert_eq!(future.raw(), 7);
/// assert_eq!(format!("{future:?}"), "7");
/// assert_eq!(format!("{green:?}"), "Green");
/// ```
pub struct EnumValue<E> {
    raw: i32,
    marker: PhantomData<fn() -> E>,
}

impl<E> EnumValue<E> {
    /// The value with number `raw`, known or not.
    pub const fn from_raw(raw: i32) -> Self {
        EnumValue {
            raw,
            marker: PhantomData,
        }
    }

    /// The number on the wire.
    pub const fn raw(self) -> i32 {
        self.raw
    }
}

impl<E: Enumeration> EnumValue<E> {
    /// The value for a known variant.
    pub fn new(value: E) -> Self {
        Self::from_raw(value.to_i32())
    }

    /// The variant, if this program knows the number.
    pub fn known(self) -> Option<E> {
        E::from_i32(self.raw)
    }

    /// Whether this program knows the number.
    pub fn is_known(self) -> bool {
        self.known().is_some()
    }

    /// Replaces the value with a known variant.
    pub fn set(&mut self, value: E) {
        self.raw = value.to_i32();
    }
}

impl<E> Clone for EnumValue<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for EnumValue<E> {}

impl<E> Default for EnumValue<E> {
    fn default() -> Self {
        Self::from_raw(0)
    }
}

impl<E> PartialEq for EnumValue<E> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<E> Eq for EnumValue<E> {}

impl<E> PartialOrd for EnumValue<E> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<E> Ord for EnumValue<E> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<E> Hash for EnumValue<E> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<E: Enumeration> PartialEq<E> for EnumValue<E> {
    fn eq(&self, other: &E) -> bool {
        self.raw == other.to_i32()
    }
}

impl<E: Enumeration + fmt::Debug> fmt::Debug for EnumValue<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.known() {
            Some(v) => fmt::Debug::fmt(&v, f),
            None => write!(f, "{}", self.raw),
        }
    }
}

impl<E: Enumeration> From<E> for EnumValue<E> {
    fn from(value: E) -> Self {
        Self::new(value)
    }
}

impl<E> From<EnumValue<E>> for i32 {
    fn from(value: EnumValue<E>) -> i32 {
        value.raw
    }
}
