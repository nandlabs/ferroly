//! Typed key/value message attributes.

/// One attribute value.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AttrValue {
    /// A UTF-8 string.
    Str(String),
    /// A signed integer.
    Int(i64),
    /// A floating-point number.
    Float(f64),
    /// A boolean.
    Bool(bool),
    /// Raw bytes.
    Bytes(Vec<u8>),
}

impl AttrValue {
    /// The string, if this is a [`Str`](AttrValue::Str).
    pub fn as_str(&self) -> Option<&str> {
        match self {
            AttrValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The integer, if this is an [`Int`](AttrValue::Int).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            AttrValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The number as a float: a [`Float`](AttrValue::Float) as is, an
    /// [`Int`](AttrValue::Int) converted; `None` for anything else.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            AttrValue::Float(f) => Some(*f),
            AttrValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// The boolean, if this is a [`Bool`](AttrValue::Bool).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            AttrValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The bytes, if this is a [`Bytes`](AttrValue::Bytes).
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            AttrValue::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// The name of the value's type: `"string"`, `"int"`, `"float"`, `"bool"`
    /// or `"bytes"`.
    pub fn type_name(&self) -> &'static str {
        match self {
            AttrValue::Str(_) => "string",
            AttrValue::Int(_) => "int",
            AttrValue::Float(_) => "float",
            AttrValue::Bool(_) => "bool",
            AttrValue::Bytes(_) => "bytes",
        }
    }

    /// The value as text: a string as is, a number or boolean in its usual
    /// form, bytes as lowercase hex (two digits per byte).
    pub fn to_text(&self) -> String {
        match self {
            AttrValue::Str(s) => s.clone(),
            AttrValue::Int(i) => i.to_string(),
            AttrValue::Float(f) => f.to_string(),
            AttrValue::Bool(b) => b.to_string(),
            AttrValue::Bytes(bytes) => {
                let mut out = String::with_capacity(bytes.len() * 2);
                for b in bytes {
                    out.push_str(&format!("{b:02x}"));
                }
                out
            }
        }
    }
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        AttrValue::Str(v.to_string())
    }
}

impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        AttrValue::Str(v)
    }
}

impl From<&String> for AttrValue {
    fn from(v: &String) -> Self {
        AttrValue::Str(v.clone())
    }
}

impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        AttrValue::Int(v)
    }
}

impl From<i32> for AttrValue {
    fn from(v: i32) -> Self {
        AttrValue::Int(i64::from(v))
    }
}

impl From<u32> for AttrValue {
    fn from(v: u32) -> Self {
        AttrValue::Int(i64::from(v))
    }
}

impl From<u16> for AttrValue {
    fn from(v: u16) -> Self {
        AttrValue::Int(i64::from(v))
    }
}

impl From<u8> for AttrValue {
    fn from(v: u8) -> Self {
        AttrValue::Int(i64::from(v))
    }
}

impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        AttrValue::Float(v)
    }
}

impl From<f32> for AttrValue {
    fn from(v: f32) -> Self {
        AttrValue::Float(f64::from(v))
    }
}

impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        AttrValue::Bool(v)
    }
}

impl From<Vec<u8>> for AttrValue {
    fn from(v: Vec<u8>) -> Self {
        AttrValue::Bytes(v)
    }
}

impl From<&[u8]> for AttrValue {
    fn from(v: &[u8]) -> Self {
        AttrValue::Bytes(v.to_vec())
    }
}

/// An ordered map of attribute names to typed values. A name may appear more
/// than once (see [`append`](Attributes::append)).
///
/// Names are compared exactly (case-sensitively).
///
/// ```
/// use ferroly::messaging::{AttrValue, Attributes};
///
/// let mut attrs = Attributes::new();
/// attrs.insert("tenant", "acme");
/// attrs.insert("attempt", 2);
/// attrs.append("tag", "a");
/// attrs.append("tag", "b");
/// assert_eq!(attrs.get_str("tenant"), Some("acme"));
/// assert_eq!(attrs.get_i64("attempt"), Some(2));
/// assert_eq!(attrs.get_all("tag").count(), 2);
/// assert_eq!(attrs.get("missing"), None::<&AttrValue>);
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attributes {
    entries: Vec<(String, AttrValue)>,
}

impl Attributes {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of entries (a repeated name counts once per entry).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Sets `name` to exactly one value. If the name exists, its first entry
    /// is replaced in place, every other entry with that name is removed, and
    /// the first old value is returned; otherwise the entry is added at the
    /// end and `None` is returned.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        value: impl Into<AttrValue>,
    ) -> Option<AttrValue> {
        let name = name.into();
        let value = value.into();
        match self.entries.iter().position(|(n, _)| *n == name) {
            Some(first) => {
                let old = std::mem::replace(&mut self.entries[first].1, value);
                let mut index = 0;
                self.entries.retain(|(n, _)| {
                    let keep = index <= first || *n != name;
                    index += 1;
                    keep
                });
                Some(old)
            }
            None => {
                self.entries.push((name, value));
                None
            }
        }
    }

    /// Adds an entry at the end, keeping any existing entries with the same
    /// name.
    pub fn append(&mut self, name: impl Into<String>, value: impl Into<AttrValue>) {
        self.entries.push((name.into(), value.into()));
    }

    /// The first value with that name.
    pub fn get(&self, name: &str) -> Option<&AttrValue> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Every value with that name, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a AttrValue> + 'a {
        self.entries
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, v)| v)
    }

    /// The first value with that name, if it is a string.
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(AttrValue::as_str)
    }

    /// The first value with that name, if it is an integer.
    pub fn get_i64(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(AttrValue::as_i64)
    }

    /// The first value with that name as a float, if it is a number.
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.get(name).and_then(AttrValue::as_f64)
    }

    /// The first value with that name, if it is a boolean.
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        self.get(name).and_then(AttrValue::as_bool)
    }

    /// The first value with that name, if it is bytes.
    pub fn get_bytes(&self, name: &str) -> Option<&[u8]> {
        self.get(name).and_then(AttrValue::as_bytes)
    }

    /// Whether any entry has that name.
    pub fn contains_key(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| n == name)
    }

    /// Removes **every** entry with that name and returns the first removed
    /// value.
    pub fn remove(&mut self, name: &str) -> Option<AttrValue> {
        let mut first = None;
        let mut kept = Vec::with_capacity(self.entries.len());
        for (n, v) in std::mem::take(&mut self.entries) {
            if n == name {
                if first.is_none() {
                    first = Some(v);
                }
            } else {
                kept.push((n, v));
            }
        }
        self.entries = kept;
        first
    }

    /// The entries, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &AttrValue)> + '_ {
        self.entries.iter().map(|(n, v)| (n.as_str(), v))
    }

    /// The names, in order; a repeated name is repeated.
    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries.iter().map(|(n, _)| n.as_str())
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl<K, V> FromIterator<(K, V)> for Attributes
where
    K: Into<String>,
    V: Into<AttrValue>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut attrs = Attributes::new();
        attrs.extend(iter);
        attrs
    }
}

impl<K, V> Extend<(K, V)> for Attributes
where
    K: Into<String>,
    V: Into<AttrValue>,
{
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (name, value) in iter {
            self.append(name, value);
        }
    }
}

impl IntoIterator for Attributes {
    type Item = (String, AttrValue);
    type IntoIter = std::vec::IntoIter<(String, AttrValue)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_replaces_in_place_and_removes_duplicates() {
        let mut attrs = Attributes::new();
        attrs.append("a", 1);
        attrs.append("b", 2);
        attrs.append("a", 3);
        attrs.append("c", 4);
        let old = attrs.insert("a", 9);
        assert_eq!(old, Some(AttrValue::Int(1)));
        let keys: Vec<&str> = attrs.keys().collect();
        assert_eq!(keys, ["a", "b", "c"]);
        assert_eq!(attrs.get_i64("a"), Some(9));
        assert_eq!(attrs.insert("d", true), None);
        assert_eq!(attrs.keys().last(), Some("d"));
    }

    #[test]
    fn append_keeps_repeated_names_in_order() {
        let mut attrs = Attributes::new();
        attrs.append("tag", "x");
        attrs.append("other", 1);
        attrs.append("tag", "y");
        let tags: Vec<&str> = attrs.get_all("tag").filter_map(AttrValue::as_str).collect();
        assert_eq!(tags, ["x", "y"]);
        assert_eq!(attrs.len(), 3);
        let keys: Vec<&str> = attrs.keys().collect();
        assert_eq!(keys, ["tag", "other", "tag"]);
    }

    #[test]
    fn typed_getters_match_only_their_type() {
        let attrs: Attributes = vec![
            ("s", AttrValue::from("text")),
            ("i", AttrValue::from(7i64)),
            ("f", AttrValue::from(1.5f64)),
            ("b", AttrValue::from(true)),
            ("y", AttrValue::from(vec![1u8, 2])),
        ]
        .into_iter()
        .collect();
        assert_eq!(attrs.get_str("s"), Some("text"));
        assert_eq!(attrs.get_i64("s"), None);
        assert_eq!(attrs.get_i64("i"), Some(7));
        assert_eq!(attrs.get_str("i"), None);
        assert_eq!(attrs.get_f64("f"), Some(1.5));
        assert_eq!(attrs.get_i64("f"), None);
        assert_eq!(attrs.get_bool("b"), Some(true));
        assert_eq!(attrs.get_str("b"), None);
        assert_eq!(attrs.get_bytes("y"), Some(&[1u8, 2][..]));
        assert_eq!(attrs.get_str("y"), None);
        assert_eq!(attrs.get("s").map(AttrValue::type_name), Some("string"));
        assert_eq!(attrs.get("i").map(AttrValue::type_name), Some("int"));
        assert_eq!(attrs.get("f").map(AttrValue::type_name), Some("float"));
        assert_eq!(attrs.get("b").map(AttrValue::type_name), Some("bool"));
        assert_eq!(attrs.get("y").map(AttrValue::type_name), Some("bytes"));
    }

    #[test]
    fn as_f64_accepts_int() {
        assert_eq!(AttrValue::Int(3).as_f64(), Some(3.0));
        assert_eq!(AttrValue::Float(2.5).as_f64(), Some(2.5));
        assert_eq!(AttrValue::Str("3".into()).as_f64(), None);
        assert_eq!(AttrValue::Bool(true).as_f64(), None);
    }

    #[test]
    fn remove_takes_every_entry_with_the_name() {
        let mut attrs = Attributes::new();
        attrs.append("a", 1);
        attrs.append("b", 2);
        attrs.append("a", 3);
        assert_eq!(attrs.remove("a"), Some(AttrValue::Int(1)));
        assert!(!attrs.contains_key("a"));
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs.remove("a"), None);
        attrs.clear();
        assert!(attrs.is_empty());
    }

    #[test]
    fn from_iterator_and_into_iterator_round_trip() {
        let attrs: Attributes = [("a", 1), ("b", 2), ("a", 3)].into_iter().collect();
        let entries: Vec<(String, AttrValue)> = attrs.clone().into_iter().collect();
        assert_eq!(entries.len(), 3);
        let back: Attributes = entries.into_iter().collect();
        assert_eq!(back, attrs);
        let mut extended = Attributes::new();
        extended.extend([("x", "y")]);
        assert_eq!(
            extended.iter().next(),
            Some(("x", &AttrValue::Str("y".into())))
        );
    }

    #[test]
    fn bytes_render_as_lowercase_hex() {
        assert_eq!(
            AttrValue::Bytes(vec![0x00, 0xab, 0x0f, 0xff]).to_text(),
            "00ab0fff"
        );
        assert_eq!(AttrValue::Int(-4).to_text(), "-4");
        assert_eq!(AttrValue::Bool(false).to_text(), "false");
        assert_eq!(AttrValue::Str("s".into()).to_text(), "s");
        assert_eq!(AttrValue::Float(0.5).to_text(), "0.5");
    }
}
