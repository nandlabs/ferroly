//! [`Metadata`]: the custom headers and trailers of a gRPC call.

use std::fmt;

use super::status::Status;
use crate::http::HeaderMap;

/// One metadata value, as yielded by [`Metadata::iter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataValue<'a> {
    /// The value of an ASCII key.
    Ascii(&'a str),
    /// The value of a binary (`-bin`) key, decoded.
    Binary(&'a [u8]),
}

#[derive(Clone, PartialEq, Eq)]
enum Value {
    Ascii(String),
    Binary(Vec<u8>),
}

/// The custom key/value pairs of a gRPC call: request metadata (sent as
/// request headers), and response headers and trailers.
///
/// Keys are lower-case `0-9 a-z - _ .` (upper case is lowered on insert).
/// Keys ending in `-bin` carry **binary** values, which travel base64-encoded
/// and are decoded on receipt (padded or unpadded); every other key carries
/// a printable-ASCII value (`0x20..=0x7E`). A key may have several values.
///
/// Keys the protocol owns cannot be set: `grpc-*`, `content-type`, `te`,
/// `user-agent`, `content-length`, `host` and the HTTP connection-specific
/// headers. Received metadata never contains `grpc-*`, `content-type`, `te`
/// or `content-length`.
///
/// ```
/// use ferroly::grpc::Metadata;
///
/// let mut md = Metadata::new();
/// md.insert("x-request-id", "abc-123")?;
/// md.append("x-tag", "a")?;
/// md.append("x-tag", "b")?;
/// md.insert_bin("trace-bin", vec![0u8, 1, 2, 255])?;
/// assert_eq!(md.get("x-request-id"), Some("abc-123"));
/// assert_eq!(md.get_all("x-tag").collect::<Vec<_>>(), ["a", "b"]);
/// assert_eq!(md.get_bin("trace-bin"), Some(&[0u8, 1, 2, 255][..]));
/// assert!(md.insert("grpc-status", "0").is_err()); // reserved
/// assert!(md.insert("x-bin", "text").is_err()); // `-bin` keys take bytes
/// # Ok::<(), ferroly::grpc::Status>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    entries: Vec<(String, Value)>,
}

impl fmt::Debug for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut m = f.debug_map();
        for (k, v) in &self.entries {
            match v {
                Value::Ascii(s) => m.entry(k, s),
                Value::Binary(b) => m.entry(k, &format_args!("<{} bytes>", b.len())),
            };
        }
        m.finish()
    }
}

/// Whether the key is reserved for the protocol (cannot be set).
fn reserved(key: &str) -> bool {
    key.starts_with("grpc-")
        || matches!(
            key,
            "content-type"
                | "te"
                | "user-agent"
                | "content-length"
                | "host"
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "transfer-encoding"
                | "upgrade"
        )
}

/// Whether a received header is the protocol's, not metadata.
fn protocol_header(key: &str) -> bool {
    key.starts_with(':')
        || key.starts_with("grpc-")
        || matches!(key, "content-type" | "te" | "content-length")
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'-' | b'_' | b'.'))
}

fn valid_ascii(value: &str) -> bool {
    value.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Decodes a binary value: base64, standard alphabet, padded or not.
pub(crate) fn decode_bin(value: &str) -> Option<Vec<u8>> {
    if value.contains('=') {
        crate::hash::base64::decode(value).ok()
    } else {
        crate::hash::base64::decode_no_pad(value).ok()
    }
}

impl Metadata {
    /// Empty metadata.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of values (a key with three values counts three).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no values.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn check(key: &str, binary: bool) -> Result<String, Status> {
        let key = key.to_ascii_lowercase();
        if !valid_key(&key) {
            return Err(Status::invalid_argument(format!(
                "invalid metadata key {key:?}: use 0-9 a-z - _ ."
            )));
        }
        if reserved(&key) {
            return Err(Status::invalid_argument(format!(
                "metadata key {key:?} is reserved"
            )));
        }
        if key.ends_with("-bin") != binary {
            return Err(Status::invalid_argument(if binary {
                format!("binary metadata key {key:?} must end in \"-bin\"")
            } else {
                format!("metadata key {key:?} ends in \"-bin\": use insert_bin / append_bin")
            }));
        }
        Ok(key)
    }

    fn check_ascii(key: &str, value: &str) -> Result<String, Status> {
        let key = Self::check(key, false)?;
        if !valid_ascii(value) {
            return Err(Status::invalid_argument(format!(
                "metadata value for {key:?} must be printable ASCII"
            )));
        }
        Ok(key)
    }

    /// Sets an ASCII value, replacing every value `key` had.
    pub fn insert(&mut self, key: &str, value: impl Into<String>) -> Result<(), Status> {
        let value = value.into();
        let key = Self::check_ascii(key, &value)?;
        self.remove(&key);
        self.entries.push((key, Value::Ascii(value)));
        Ok(())
    }

    /// Adds an ASCII value, keeping the values `key` already has.
    pub fn append(&mut self, key: &str, value: impl Into<String>) -> Result<(), Status> {
        let value = value.into();
        let key = Self::check_ascii(key, &value)?;
        self.entries.push((key, Value::Ascii(value)));
        Ok(())
    }

    /// Sets a binary value (`key` must end in `-bin`), replacing every value
    /// it had.
    pub fn insert_bin(&mut self, key: &str, value: impl Into<Vec<u8>>) -> Result<(), Status> {
        let key = Self::check(key, true)?;
        self.remove(&key);
        self.entries.push((key, Value::Binary(value.into())));
        Ok(())
    }

    /// Adds a binary value (`key` must end in `-bin`).
    pub fn append_bin(&mut self, key: &str, value: impl Into<Vec<u8>>) -> Result<(), Status> {
        let key = Self::check(key, true)?;
        self.entries.push((key, Value::Binary(value.into())));
        Ok(())
    }

    /// The first ASCII value of `key` (case-insensitive).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find_map(|(k, v)| match v {
            Value::Ascii(s) if k.eq_ignore_ascii_case(key) => Some(s.as_str()),
            _ => None,
        })
    }

    /// Every ASCII value of `key`, in order.
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries.iter().filter_map(move |(k, v)| match v {
            Value::Ascii(s) if k.eq_ignore_ascii_case(key) => Some(s.as_str()),
            _ => None,
        })
    }

    /// The first binary value of `key` (case-insensitive).
    pub fn get_bin(&self, key: &str) -> Option<&[u8]> {
        self.entries.iter().find_map(|(k, v)| match v {
            Value::Binary(b) if k.eq_ignore_ascii_case(key) => Some(b.as_slice()),
            _ => None,
        })
    }

    /// Every binary value of `key`, in order.
    pub fn get_all_bin<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.entries.iter().filter_map(move |(k, v)| match v {
            Value::Binary(b) if k.eq_ignore_ascii_case(key) => Some(b.as_slice()),
            _ => None,
        })
    }

    /// Whether `key` has a value.
    pub fn contains(&self, key: &str) -> bool {
        self.entries
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(key))
    }

    /// Removes every value of `key`; whether there was one.
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
        self.entries.len() != before
    }

    /// Every key/value pair, in insertion (or arrival) order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, MetadataValue<'_>)> + '_ {
        self.entries.iter().map(|(k, v)| {
            let v = match v {
                Value::Ascii(s) => MetadataValue::Ascii(s),
                Value::Binary(b) => MetadataValue::Binary(b),
            };
            (k.as_str(), v)
        })
    }

    /// The metadata in a received header section: the protocol's own fields
    /// are skipped, `-bin` values base64-decoded (several comma-separated
    /// values become several entries; an undecodable value is dropped), and
    /// ASCII values kept as they are.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Metadata {
        let mut md = Metadata::new();
        for (name, value) in headers.iter() {
            let key = name.to_ascii_lowercase();
            if protocol_header(&key) || !valid_key(&key) {
                continue;
            }
            if key.ends_with("-bin") {
                for part in value.split(',') {
                    if let Some(bytes) = decode_bin(part.trim()) {
                        md.entries.push((key.clone(), Value::Binary(bytes)));
                    }
                }
            } else if valid_ascii(value) {
                md.entries.push((key, Value::Ascii(value.to_string())));
            }
        }
        md
    }

    /// Writes the metadata as header fields (binary values base64-encoded,
    /// unpadded). Reserved keys — which `insert` refuses, but which received
    /// metadata may carry, such as `host` — are skipped.
    pub(crate) fn write_to(&self, headers: &mut HeaderMap) {
        for (k, v) in &self.entries {
            if reserved(k) {
                continue;
            }
            match v {
                Value::Ascii(s) => headers.append(k.clone(), s.clone()),
                Value::Binary(b) => {
                    headers.append(k.clone(), crate::hash::base64::encode_no_pad(b))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_rules() {
        let mut md = Metadata::new();
        assert!(md.insert("X-Upper", "v").is_ok());
        assert_eq!(md.get("x-upper"), Some("v"));
        assert_eq!(md.get("X-UPPER"), Some("v"));
        for bad in ["", "has space", "ünï", "a:b", "x/y"] {
            assert!(md.insert(bad, "v").is_err(), "{bad:?}");
        }
        for reserved in ["grpc-timeout", "content-type", "te", "user-agent", "host"] {
            assert!(md.insert(reserved, "v").is_err(), "{reserved:?}");
        }
        assert!(md.insert("x", "line\nbreak").is_err());
        assert!(md.insert("x", "tab\t").is_err());
        assert!(md.insert("x", "").is_ok());
        assert!(md.insert_bin("x", vec![1]).is_err());
        assert!(md.insert("x-bin", "v").is_err());
        md.insert("k", "1").unwrap();
        md.append("k", "2").unwrap();
        md.insert("k", "3").unwrap();
        assert_eq!(md.get_all("k").collect::<Vec<_>>(), ["3"]);
        assert!(md.remove("K"));
        assert!(!md.contains("k"));
    }

    #[test]
    fn header_round_trip_and_binary_forms() {
        let mut md = Metadata::new();
        md.insert("x-id", "42").unwrap();
        md.append_bin("blob-bin", vec![0xff, 0x00, 0x10]).unwrap();
        md.append_bin("blob-bin", b"hello".to_vec()).unwrap();
        let mut headers = HeaderMap::new();
        md.write_to(&mut headers);
        assert_eq!(headers.get_all("blob-bin").count(), 2);
        assert_eq!(Metadata::from_headers(&headers), md);

        // Padded, unpadded and comma-joined values all decode; garbage is
        // dropped; protocol headers are not metadata.
        let mut headers = HeaderMap::new();
        headers.append("a-bin", "aGVsbG8=");
        headers.append("b-bin", "aGVsbG8");
        headers.append("c-bin", "aGk, aGVsbG8=");
        headers.append("d-bin", "!!!");
        headers.append("content-type", "application/grpc");
        headers.append("grpc-timeout", "1S");
        headers.append("te", "trailers");
        headers.append("user-agent", "agent/1");
        let md = Metadata::from_headers(&headers);
        assert_eq!(md.get_bin("a-bin"), Some(&b"hello"[..]));
        assert_eq!(md.get_bin("b-bin"), Some(&b"hello"[..]));
        assert_eq!(
            md.get_all_bin("c-bin").collect::<Vec<_>>(),
            [&b"hi"[..], &b"hello"[..]]
        );
        assert!(!md.contains("d-bin"));
        assert!(!md.contains("content-type"));
        assert!(!md.contains("grpc-timeout"));
        assert_eq!(md.get("user-agent"), Some("agent/1"));
        // Received reserved keys are not sent back out.
        let mut out = HeaderMap::new();
        md.write_to(&mut out);
        assert!(!out.contains("user-agent"));
        assert!(format!("{md:?}").contains("<5 bytes>"));
    }
}
