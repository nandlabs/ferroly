//! Content-type-keyed encode/decode over the in-house formats.
//!
//! The format set is a closed [`Format`] enum dispatched by `match`, so these
//! are free functions — there is no state to carry.

use ferroly::codec::{json, toml, xml, yaml, CodecError, Decode, Encode, Limits};

/// An encoding format supported by the content-type dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// JavaScript Object Notation.
    Json,
    /// Extensible Markup Language (pragmatic subset).
    Xml,
    /// YAML Ain't Markup Language (block-style subset).
    Yaml,
    /// Tom's Obvious Minimal Language (config subset).
    Toml,
}

impl Format {
    /// Resolves a MIME content type (ignoring `;charset=...` and case) to a
    /// format, recognizing `+json`/`+xml`/`+yaml`/`+toml` structured-syntax
    /// suffixes.
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
            "application/yaml" | "text/yaml" | "application/x-yaml" | "text/x-yaml"
            | "application/yml" | "text/yml" => return Some(Format::Yaml),
            "application/toml" | "text/toml" | "application/x-toml" => return Some(Format::Toml),
            _ => {}
        }

        if ct.ends_with("+json") {
            Some(Format::Json)
        } else if ct.ends_with("+xml") {
            Some(Format::Xml)
        } else if ct.ends_with("+yaml") {
            Some(Format::Yaml)
        } else if ct.ends_with("+toml") {
            Some(Format::Toml)
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
///
/// The only error is an unknown content type: a value the format cannot
/// represent is written in its documented fallback form (a non-finite float as
/// JSON `null`, for example). Use [`try_encode`] to get an error instead.
pub fn encode<T: Encode + ?Sized>(content_type: &str, value: &T) -> Result<Vec<u8>, CodecError> {
    match resolve(content_type)? {
        Format::Json => Ok(json::encode_to_vec(value)),
        Format::Xml => Ok(xml::encode_to_vec(value)),
        Format::Yaml => Ok(yaml::encode_to_vec(value)),
        Format::Toml => Ok(toml::encode_to_vec(value)),
    }
}

/// Like [`encode`], but fails with [`CodecError::Unrepresentable`] when the
/// selected format cannot represent the value faithfully (see `try_encode` in
/// [`json`], [`xml`] and [`toml`]; every value has a YAML form).
pub fn try_encode<T: Encode + ?Sized>(
    content_type: &str,
    value: &T,
) -> Result<Vec<u8>, CodecError> {
    let text = match resolve(content_type)? {
        Format::Json => json::try_encode(value)?,
        Format::Xml => xml::try_encode(value)?,
        Format::Yaml => yaml::encode(value),
        Format::Toml => toml::try_encode(value)?,
    };
    Ok(text.into_bytes())
}

/// Decodes a value from bytes using the codec selected by `content_type`, with
/// the default [`Limits`].
///
/// Scalar handling follows the format: JSON and TOML are typed and decode
/// strictly; XML carries only text and YAML is loosely typed, so those decode
/// leniently (a string holding a number or bool is accepted).
pub fn decode<T: Decode>(content_type: &str, bytes: &[u8]) -> Result<T, CodecError> {
    decode_with(content_type, bytes, Limits::DEFAULT)
}

/// Like [`decode`], with an explicit parser budget — use this for untrusted
/// input, where the peer chooses both the content type and the body.
pub fn decode_with<T: Decode>(
    content_type: &str,
    bytes: &[u8],
    limits: Limits,
) -> Result<T, CodecError> {
    match resolve(content_type)? {
        Format::Json => json::decode_from_slice_with(bytes, limits),
        Format::Xml => xml::decode_from_slice_with(bytes, limits),
        Format::Yaml => yaml::decode_from_slice_with(bytes, limits),
        Format::Toml => toml::decode_from_slice_with(bytes, limits),
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

    #[test]
    fn toml_round_trip() {
        assert_eq!(
            Format::from_content_type("application/toml"),
            Some(Format::Toml)
        );
        let cfg = Config {
            name: "svc".into(),
            port: 8080,
        };
        let bytes = encode("application/toml", &cfg).unwrap();
        let back: Config = decode("application/toml", &bytes).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn decode_with_applies_the_budget_to_every_format() {
        let limits = Limits::new().with_max_input_bytes(16);
        let cfg = Config {
            name: "a-long-service-name".into(),
            port: 8080,
        };
        for ct in [
            "application/json",
            "application/xml",
            "application/yaml",
            "application/toml",
        ] {
            let bytes = encode(ct, &cfg).unwrap();
            assert_eq!(decode::<Config>(ct, &bytes).unwrap(), cfg, "{ct}");
            assert!(
                matches!(
                    decode_with::<Config>(ct, &bytes, limits),
                    Err(CodecError::LimitExceeded(_))
                ),
                "{ct}"
            );
            assert_eq!(
                decode_with::<Config>(ct, &bytes, Limits::DEFAULT).unwrap(),
                cfg
            );
        }
        assert!(matches!(
            decode_with::<Config>("text/plain", b"", limits),
            Err(CodecError::UnsupportedContentType(_))
        ));
    }

    #[test]
    fn try_encode_reports_unrepresentable_values() {
        use ferroly::codec::Value;
        let cfg = Config {
            name: "svc".into(),
            port: 1,
        };
        for ct in [
            "application/json",
            "application/xml",
            "application/yaml",
            "application/toml",
        ] {
            assert_eq!(
                try_encode(ct, &cfg).unwrap(),
                encode(ct, &cfg).unwrap(),
                "{ct}"
            );
        }
        // One unrepresentable value per format.
        let nan = Value::Object(vec![("x".into(), Value::Float(f64::NAN))]);
        let control = Value::Object(vec![("x".into(), Value::Str("\u{1}".into()))]);
        let null_item = Value::Object(vec![("x".into(), Value::Array(vec![Value::Null]))]);
        assert!(matches!(
            try_encode("application/json", &nan),
            Err(CodecError::Unrepresentable(_))
        ));
        assert!(matches!(
            try_encode("application/xml", &control),
            Err(CodecError::Unrepresentable(_))
        ));
        assert!(matches!(
            try_encode("application/toml", &null_item),
            Err(CodecError::Unrepresentable(_))
        ));
        // YAML can write all three, and the others cope with what is not theirs.
        for v in [&nan, &control, &null_item] {
            assert!(try_encode("application/yaml", v).is_ok());
        }
        assert!(try_encode("application/toml", &nan).is_ok());
        assert!(try_encode("application/json", &control).is_ok());
        assert!(try_encode("application/xml", &null_item).is_ok());
        assert!(matches!(
            try_encode("text/plain", &cfg),
            Err(CodecError::UnsupportedContentType(_))
        ));
    }
}
