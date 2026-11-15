//! The content digest that keys the cache.

use std::fmt;
use std::str::FromStr;

use ferroly::hash;

use super::StoreError;

/// A SHA-256 content digest, written `sha256:<64 lowercase hex digits>`.
///
/// This is the form OCI registries use (`Docker-Content-Digest`, descriptor
/// `digest` fields), and the cache's key. Parsing accepts upper- or
/// lower-case hex and an algorithm name in any case; [`Display`](fmt::Display) always
/// writes the canonical lower-case form.
///
/// ```
/// use ferroly::store::Digest;
///
/// let d: Digest = "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
///     .parse()
///     .unwrap();
/// assert_eq!(d, Digest::of(b"abc"));
/// assert_eq!(
///     d.to_string(),
///     "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
/// );
/// assert!("md5:abc".parse::<Digest>().is_err());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest {
    bytes: [u8; 32],
}

impl Digest {
    /// The algorithm prefix (`sha256`).
    pub const ALGORITHM: &'static str = "sha256";

    /// Parses `sha256:<hex>`.
    pub fn parse(text: &str) -> Result<Digest, StoreError> {
        let bad = || {
            // Never echo an unbounded string back.
            let shown: String = text.chars().take(80).collect();
            StoreError::InvalidDigest(format!("expected sha256:<64 hex digits>, got {shown:?}"))
        };
        let (algo, hex) = text.split_once(':').ok_or_else(bad)?;
        if !algo.eq_ignore_ascii_case(Self::ALGORITHM) {
            return Err(bad());
        }
        Digest::from_hex(hex).map_err(|_| bad())
    }

    /// Parses a bare 64-digit hex SHA-256 (without the `sha256:` prefix).
    pub fn from_hex(hex: &str) -> Result<Digest, StoreError> {
        if hex.len() != 64 {
            return Err(StoreError::InvalidDigest(
                "a sha256 digest is 64 hex digits".into(),
            ));
        }
        hash::Digest::<32>::from_hex(hex)
            .map(|d| Digest {
                bytes: d.into_bytes(),
            })
            .map_err(|_| StoreError::InvalidDigest("not hex".into()))
    }

    /// Wraps raw SHA-256 output.
    pub const fn from_bytes(bytes: [u8; 32]) -> Digest {
        Digest { bytes }
    }

    /// The SHA-256 of `data`.
    pub fn of(data: &[u8]) -> Digest {
        Digest::from_bytes(hash::sha256(data).into_bytes())
    }

    /// The raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// The 64-digit lower-case hex form, without the `sha256:` prefix.
    pub fn hex(&self) -> String {
        hash::hex::encode(&self.bytes)
    }

    /// The algorithm name (`sha256`).
    pub fn algorithm(&self) -> &'static str {
        Self::ALGORITHM
    }

    /// Constant-time comparison.
    pub fn ct_eq(&self, other: &Digest) -> bool {
        hash::ct_eq(&self.bytes, &other.bytes)
    }
}

impl From<hash::Digest<32>> for Digest {
    fn from(d: hash::Digest<32>) -> Self {
        Digest::from_bytes(d.into_bytes())
    }
}

impl FromStr for Digest {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Digest::parse(s)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", Self::ALGORITHM, self.hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display() {
        let d = Digest::of(b"");
        let s = d.to_string();
        assert_eq!(
            s,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(Digest::parse(&s).unwrap(), d);
        assert_eq!(Digest::parse(&s.to_uppercase()).unwrap(), d);
        for bad in [
            "",
            "sha256",
            "sha256:",
            "sha512:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8555",
            "sha256:g3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "sha256:+3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
        ] {
            assert!(Digest::parse(bad).is_err(), "{bad:?}");
        }
        let long = format!("sha256:{}", "a".repeat(10_000));
        let err = Digest::parse(&long).unwrap_err().to_string();
        assert!(err.len() < 300, "error echoes unbounded input");
    }
}
