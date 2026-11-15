//! Preservation of fields a message type does not declare.

use super::error::ProtoError;
use super::wire::{self, Reader, WireType};

/// The value of one unknown field, as it appeared on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnknownValue {
    /// Wire type 0.
    Varint(u64),
    /// Wire type 1 (eight little-endian bytes).
    Fixed64(u64),
    /// Wire type 5 (four little-endian bytes).
    Fixed32(u32),
    /// Wire type 2: the payload without its length prefix.
    LengthDelimited(Vec<u8>),
    /// Wire types 3/4: the raw bytes between the start- and end-group keys.
    Group(Vec<u8>),
}

/// One field a message type does not declare.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnknownField {
    /// The field number.
    pub tag: u32,
    /// The value.
    pub value: UnknownValue,
}

impl UnknownField {
    /// The wire type the value is written with.
    pub fn wire_type(&self) -> WireType {
        match self.value {
            UnknownValue::Varint(_) => WireType::Varint,
            UnknownValue::Fixed64(_) => WireType::I64,
            UnknownValue::Fixed32(_) => WireType::I32,
            UnknownValue::LengthDelimited(_) => WireType::Len,
            UnknownValue::Group(_) => WireType::StartGroup,
        }
    }
}

/// The unknown fields of a message, kept in arrival order and written back
/// after the known fields when the message is encoded.
///
/// Give a derived message a field of this type marked `#[proto(unknown)]` to
/// preserve what it does not understand (generated code always does):
///
/// ```
/// use ferroly::proto::{Message, UnknownFields};
///
/// #[derive(ferroly::proto::Message, Default, Debug)]
/// struct Old {
///     #[proto(tag = 1)]
///     id: u64,
///     #[proto(unknown)]
///     unknown: UnknownFields,
/// }
///
/// // A newer writer added field 2 (a string).
/// let newer = [0x08, 0x07, 0x12, 0x02, b'h', b'i'];
/// let old = Old::decode(&newer).unwrap();
/// assert_eq!(old.id, 7);
/// assert_eq!(old.unknown.len(), 1);
/// assert_eq!(old.encode_to_vec(), newer); // nothing lost
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UnknownFields {
    fields: Vec<UnknownField>,
}

impl UnknownFields {
    /// An empty set.
    pub const fn new() -> Self {
        UnknownFields { fields: Vec::new() }
    }

    /// Whether no unknown field was seen.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// How many unknown fields were seen (a repeated field counts once per
    /// occurrence).
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// The fields, in arrival order.
    pub fn iter(&self) -> std::slice::Iter<'_, UnknownField> {
        self.fields.iter()
    }

    /// The occurrences of field `tag`, in arrival order.
    pub fn get(&self, tag: u32) -> impl Iterator<Item = &UnknownValue> + '_ {
        self.fields
            .iter()
            .filter(move |f| f.tag == tag)
            .map(|f| &f.value)
    }

    /// Appends a field.
    pub fn push(&mut self, tag: u32, value: UnknownValue) {
        self.fields.push(UnknownField { tag, value });
    }

    /// Removes every field.
    pub fn clear(&mut self) {
        self.fields.clear();
    }

    /// Reads the value of a field whose key was just read and keeps it.
    pub fn merge_field(
        &mut self,
        tag: u32,
        wire_type: WireType,
        r: &mut Reader<'_>,
    ) -> Result<(), ProtoError> {
        // Each kept field costs its record plus its payload; both are charged
        // against the reader's decoded-bytes budget before they are stored.
        let value = match wire_type {
            WireType::Varint => UnknownValue::Varint(r.read_varint()?),
            WireType::I64 => UnknownValue::Fixed64(r.read_fixed64()?),
            WireType::I32 => UnknownValue::Fixed32(r.read_fixed32()?),
            WireType::Len => {
                let bytes = r.read_bytes()?;
                r.charge(bytes.len())?;
                UnknownValue::LengthDelimited(bytes.to_vec())
            }
            WireType::StartGroup => {
                let bytes = r.skip_group(tag)?;
                r.charge(bytes.len())?;
                UnknownValue::Group(bytes.to_vec())
            }
            WireType::EndGroup => return Err(ProtoError::UnexpectedEndGroup(tag)),
        };
        r.charge_push(&self.fields, 0)?;
        self.push(tag, value);
        Ok(())
    }

    /// Writes every field back in wire form.
    pub fn encode_raw(&self, buf: &mut Vec<u8>) {
        for f in &self.fields {
            wire::encode_key(f.tag, f.wire_type(), buf);
            match &f.value {
                UnknownValue::Varint(v) => wire::encode_varint(*v, buf),
                UnknownValue::Fixed64(v) => buf.extend_from_slice(&v.to_le_bytes()),
                UnknownValue::Fixed32(v) => buf.extend_from_slice(&v.to_le_bytes()),
                UnknownValue::LengthDelimited(b) => {
                    wire::encode_varint(b.len() as u64, buf);
                    buf.extend_from_slice(b);
                }
                UnknownValue::Group(b) => {
                    buf.extend_from_slice(b);
                    wire::encode_key(f.tag, WireType::EndGroup, buf);
                }
            }
        }
    }

    /// The number of bytes [`encode_raw`](Self::encode_raw) writes.
    pub fn encoded_len(&self) -> usize {
        self.fields
            .iter()
            .map(|f| {
                wire::key_len(f.tag)
                    + match &f.value {
                        UnknownValue::Varint(v) => wire::varint_len(*v),
                        UnknownValue::Fixed64(_) => 8,
                        UnknownValue::Fixed32(_) => 4,
                        UnknownValue::LengthDelimited(b) => {
                            wire::varint_len(b.len() as u64) + b.len()
                        }
                        UnknownValue::Group(b) => b.len() + wire::key_len(f.tag),
                    }
            })
            .sum()
    }
}

impl<'a> IntoIterator for &'a UnknownFields {
    type Item = &'a UnknownField;
    type IntoIter = std::slice::Iter<'a, UnknownField>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
