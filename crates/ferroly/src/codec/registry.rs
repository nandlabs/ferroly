//! Content-type-keyed encode/decode over the in-house formats.
//!
//! The format set is a closed [`Format`] enum dispatched by `match`, so these
//! are free functions — there is no state to carry.

use ferroly::codec::{json, xml, yaml, CodecError, Decode, Encode};

/// A encoding format supported by the content-type dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// JavaScript Object Notation.
    Json,
    /// Extensible Markup Language (pragmatic subset).
    Xml,
    /// YAML Ain't Markup Language (block-style subset).
    Yaml,
}

impl Format {
    /// Resolves a MIME content type (ignoring `;charset=...` and case) to a
    /// format, recognizing `+json`/`+xml`/`+yaml` structured-syntax suffixes.
    pub fn from_content_type(content_type: &str) -> Option<Format> {
        let ct = content_type
            .split(';')
            .next()
            .unwrap_or(content_type)
            .trim()
            .to_ascii_lowercase();

        match ct.as_str() {
            "application/json" | "text/json" => return Some(Format::Json),
            "application/xml" | "text/xml" => return Some(Format::Xml),
            "application/yaml" | "text/yaml" | "application/x-yaml" | "text/x-yaml" => {
                return Some(Format::Yaml)
            }
            _ => {}
        }

        if ct.ends_with("+json") {
            Some(Format::Json)
        } else if ct.ends_with("+xml") {
            Some(Format::Xml)
        } else if ct.ends_with("+yaml") {
            Some(Format::Yaml)
        } else {
            None
        }
    }
}

/// Resolves a content type to its [`Format`], erroring if unknown.
pub fn resolve(content_type: &str) -> Result<Format, CodecError> {
    Format::from_content_type(content_type)
        .ok_or_else(|| CodecError::UnsupportedContentType(content_type.to_string()))
}

/// Encodes a value to bytes using the codec selected by `content_type`.
pub fn encode<T: Encode>(content_type: &str, value: &T) -> Result<Vec<u8>, CodecError> {
    match resolve(content_type)? {
        Format::Json => Ok(json::encode_to_vec(value)),
        Format::Xml => Ok(xml::encode_to_vec(value)),
        Format::Yaml => Ok(yaml::encode_to_vec(value)),
    }
}

/// Decodes a value from bytes using the codec selected by `content_type`.
pub fn decode<T: Decode>(content_type: &str, bytes: &[u8]) -> Result<T, CodecError> {
    match resolve(content_type)? {
        Format::Json => json::decode_from_slice(bytes),
        Format::Xml => xml::decode_from_slice(bytes),
        Format::Yaml => yaml::decode_from_slice(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::{Decode, Encode};

    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Config {
        name: String,
        port: u16,
    }

    #[test]
    fn resolves_content_types() {
        assert_eq!(
            Format::from_content_type("application/json"),
            Some(Format::Json)
        );
        assert_eq!(
            Format::from_content_type("application/json; charset=utf-8"),
            Some(Format::Json)
        );
        assert_eq!(
            Format::from_content_type("application/vnd.api+json"),
            Some(Format::Json)
        );
        assert_eq!(Format::from_content_type("text/xml"), Some(Format::Xml));
        assert_eq!(Format::from_content_type("application/octet-stream"), None);
    }

    #[test]
    fn json_round_trip() {
        let cfg = Config {
            name: "svc".into(),
            port: 8080,
        };
        let bytes = encode("application/json", &cfg).unwrap();
        let back: Config = decode("application/json", &bytes).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn unknown_content_type_errors() {
        let err = encode(
            "application/octet-stream",
            &Config {
                name: "x".into(),
                port: 1,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CodecError::UnsupportedContentType(_)));
    }

    #[test]
    fn xml_round_trip() {
        let cfg = Config {
            name: "svc".into(),
            port: 8080,
        };
        let bytes = encode("application/xml", &cfg).unwrap();
        let back: Config = decode("application/xml", &bytes).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn yaml_round_trip() {
        let cfg = Config {
            name: "svc".into(),
            port: 8080,
        };
        let bytes = encode("application/yaml", &cfg).unwrap();
        let back: Config = decode("application/yaml", &bytes).unwrap();
        assert_eq!(cfg, back);
    }
}
