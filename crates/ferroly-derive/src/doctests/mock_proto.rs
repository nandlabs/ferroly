// A stand-in for `ferroly::proto`, textually included (after
// `mock_codec.rs`, which declares `extern crate self as ferroly`) at the top of
// every doctest in `doctests/mod.rs`.
//
// It declares every item the protobuf derives expand to, with blanket
// `FieldValue` impls so any field type is accepted: a compile failure in a
// `rejects` snippet can then only come from the derive itself (or from the
// real `tags_disjoint` const check). Keep it in step with
// `crates/ferroly/src/proto/field.rs` and `message.rs`.
#[allow(dead_code, unused_variables)]
pub mod proto {
    pub trait Message: Default {
        fn encode_raw(&self, buf: &mut Vec<u8>);
        fn encoded_len(&self) -> usize;
        fn merge_from(&mut self, r: &mut __private::Reader<'_>) -> Result<(), __private::ProtoError>;
        fn __encoded_len_cached(&self, c: &mut __private::SizeCache) -> usize {
            0
        }
        fn __encode_cached(&self, buf: &mut Vec<u8>, c: &mut __private::SizeCache) {}
        fn __forget_required(&self, r: &mut __private::Reader<'_>) {}
    }

    pub trait Oneof: Sized {
        const TAGS: &'static [u32];
        fn encode_raw(&self, buf: &mut Vec<u8>);
        fn encoded_len(&self) -> usize;
        fn __encoded_len_cached(&self, c: &mut __private::SizeCache) -> usize {
            0
        }
        fn __encode_cached(&self, buf: &mut Vec<u8>, c: &mut __private::SizeCache) {}
        fn __forget_required(&self, r: &mut __private::Reader<'_>) {}
        fn merge_field(
            slot: &mut Option<Self>,
            tag: u32,
            wire_type: __private::WireType,
            r: &mut __private::Reader<'_>,
        ) -> Result<bool, __private::ProtoError>;
    }

    pub trait Enumeration: Copy + 'static {
        fn to_i32(self) -> i32;
        fn from_i32(value: i32) -> Option<Self>;
        fn proto_name(self) -> &'static str;
        fn from_proto_name(name: &str) -> Option<Self>;
    }

    pub struct EnumValue<E>(i32, core::marker::PhantomData<E>);
    impl<E> Default for EnumValue<E> {
        fn default() -> Self {
            EnumValue(0, core::marker::PhantomData)
        }
    }

    #[derive(Default)]
    pub struct UnknownFields;
    impl UnknownFields {
        pub fn encode_raw(&self, buf: &mut Vec<u8>) {}
        pub fn encoded_len(&self) -> usize {
            0
        }
        pub fn merge_field(
            &mut self,
            tag: u32,
            wire_type: __private::WireType,
            r: &mut __private::Reader<'_>,
        ) -> Result<(), __private::ProtoError> {
            Ok(())
        }
    }

    pub mod __private {
        pub use super::{Message, Oneof, UnknownFields};

        pub struct SizeCache;
        impl SizeCache {
            pub const fn new() -> Self {
                SizeCache
            }
        }

        pub enum Int32 {}
        pub enum Int64 {}
        pub enum UInt32 {}
        pub enum UInt64 {}
        pub enum SInt32 {}
        pub enum SInt64 {}
        pub enum Fixed32 {}
        pub enum Fixed64 {}
        pub enum SFixed32 {}
        pub enum SFixed64 {}
        pub enum Float {}
        pub enum Double {}
        pub enum Bool {}
        pub enum Str {}
        pub enum Bytes {}
        pub enum Msg {}
        pub enum Enum {}

        #[derive(Debug)]
        pub struct ProtoError;
        impl ProtoError {
            pub fn missing_required(message: &'static str, field: &'static str) -> Self {
                ProtoError
            }
        }

        #[derive(Clone, Copy, PartialEq)]
        pub enum WireType {
            Varint,
        }

        pub struct Reader<'a>(&'a [u8]);
        impl Reader<'_> {
            pub fn read_key(&mut self) -> Result<Option<(u32, WireType)>, ProtoError> {
                Ok(None)
            }
            pub fn skip_field(&mut self, tag: u32, wire_type: WireType) -> Result<(), ProtoError> {
                Ok(())
            }
            pub fn note_required(
                &mut self,
                addr: *const (),
                key: &'static str,
                message: &'static str,
                missing: u64,
                fields: &'static [&'static str],
            ) -> Result<(), ProtoError> {
                Ok(())
            }
            pub fn forget_required(&mut self, addr: *const (), key: &'static str) {}
        }

        pub trait FieldValue<K>: Default {}
        impl<K, T: Default> FieldValue<K> for T {}

        pub const fn wire_type_of<K, T: FieldValue<K>>() -> WireType {
            WireType::Varint
        }
        pub fn oneof_replace<O: Oneof, K, T: FieldValue<K>>(slot: &Option<O>, r: &mut Reader<'_>) -> Result<(), ProtoError> {
            Ok(())
        }
        pub fn forget_message<M>(value: &M, r: &mut Reader<'_>) {}
        pub fn forget_optional_message<M>(value: &Option<M>, r: &mut Reader<'_>) {}
        pub fn forget_oneof<O: Oneof>(value: &Option<O>, r: &mut Reader<'_>) {}

        pub trait MapField: Default {
            type Key;
            type Value;
        }
        impl<K, V> MapField for std::collections::BTreeMap<K, V> {
            type Key = K;
            type Value = V;
        }
        impl<K, V> MapField for std::collections::HashMap<K, V> {
            type Key = K;
            type Value = V;
        }

        pub fn encode_implicit<K, T: FieldValue<K>>(tag: u32, v: &T, buf: &mut Vec<u8>, c: &mut SizeCache) {}
        pub fn len_implicit<K, T: FieldValue<K>>(tag: u32, v: &T) -> usize { 0 }
        pub fn len_implicit_cached<K, T: FieldValue<K>>(tag: u32, v: &T, c: &mut SizeCache) -> usize { 0 }
        pub fn encode_always<K, T: FieldValue<K>>(tag: u32, v: &T, buf: &mut Vec<u8>, c: &mut SizeCache) {}
        pub fn len_always<K, T: FieldValue<K>>(tag: u32, v: &T) -> usize { 0 }
        pub fn len_always_cached<K, T: FieldValue<K>>(tag: u32, v: &T, c: &mut SizeCache) -> usize { 0 }
        pub fn encode_optional<K, T: FieldValue<K>>(tag: u32, v: &Option<T>, buf: &mut Vec<u8>, c: &mut SizeCache) {}
        pub fn len_optional<K, T: FieldValue<K>>(tag: u32, v: &Option<T>) -> usize { 0 }
        pub fn len_optional_cached<K, T: FieldValue<K>>(tag: u32, v: &Option<T>, c: &mut SizeCache) -> usize { 0 }
        pub fn encode_repeated<K, T: FieldValue<K>>(tag: u32, v: &[T], packed: bool, buf: &mut Vec<u8>, c: &mut SizeCache) {}
        pub fn len_repeated<K, T: FieldValue<K>>(tag: u32, v: &[T], packed: bool) -> usize { 0 }
        pub fn len_repeated_cached<K, T: FieldValue<K>>(tag: u32, v: &[T], packed: bool, c: &mut SizeCache) -> usize { 0 }
        pub fn encode_map<KK, VK, M: MapField>(tag: u32, m: &M, buf: &mut Vec<u8>, c: &mut SizeCache) {}
        pub fn len_map<KK, VK, M: MapField>(tag: u32, m: &M) -> usize { 0 }
        pub fn len_map_cached<KK, VK, M: MapField>(tag: u32, m: &M, c: &mut SizeCache) -> usize { 0 }
        pub fn encode_oneof<O: Oneof>(v: &Option<O>, buf: &mut Vec<u8>, c: &mut SizeCache) {}
        pub fn len_oneof<O: Oneof>(v: &Option<O>) -> usize { 0 }
        pub fn len_oneof_cached<O: Oneof>(v: &Option<O>, c: &mut SizeCache) -> usize { 0 }
        pub fn merge_value<K, T: FieldValue<K>>(wt: WireType, v: &mut T, r: &mut Reader<'_>) -> Result<bool, ProtoError> { Ok(true) }
        pub fn merge_optional<K, T: FieldValue<K>>(wt: WireType, v: &mut Option<T>, r: &mut Reader<'_>) -> Result<bool, ProtoError> { Ok(true) }
        pub fn merge_repeated<K, T: FieldValue<K>>(wt: WireType, v: &mut Vec<T>, r: &mut Reader<'_>) -> Result<bool, ProtoError> { Ok(true) }
        pub fn merge_map<KK, VK, M: MapField>(wt: WireType, m: &mut M, r: &mut Reader<'_>) -> Result<bool, ProtoError> { Ok(true) }
        pub fn merge_oneof<O: Oneof>(slot: &mut Option<O>, tag: u32, wt: WireType, r: &mut Reader<'_>) -> Result<bool, ProtoError> { Ok(false) }

        // The real implementation: the enum range check is a const assertion.
        pub const fn fits_i32(value: i128) -> bool {
            value >= i32::MIN as i128 && value <= i32::MAX as i128
        }

        // The real implementation: the oneof overlap check is a const assertion.
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
    }
}
