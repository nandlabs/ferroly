//! A redacting, wipe-on-drop container for secret bytes.

use std::fmt;

/// Secret bytes (a key, token or password) that cannot be printed by accident.
///
/// - [`Debug`](fmt::Debug) and [`Display`](fmt::Display) print `[REDACTED]`,
///   never the content, so a `Secret` inside a `#[derive(Debug)]` struct, a log
///   line or an error message does not leak.
/// - Reading the bytes is explicit: [`expose`](Secret::expose) /
///   [`expose_str`](Secret::expose_str).
/// - Equality ([`PartialEq`], [`ct_eq`](Secret::ct_eq)) is constant-time in the
///   content (only the lengths are compared early).
/// - The buffer is overwritten with zeros when the value is dropped.
///
/// ```
/// # #[cfg(feature = "hash")] {
/// use ferroly::hash::Secret;
///
/// let key = Secret::from("s3cr3t-api-key");
/// assert_eq!(format!("{key:?}"), "Secret([REDACTED])");
/// assert_eq!(format!("{key}"), "[REDACTED]");
/// assert_eq!(key.expose_str(), Some("s3cr3t-api-key"));
/// assert!(key.ct_eq(b"s3cr3t-api-key"));
/// # }
/// ```
///
/// # The wipe is best-effort
///
/// ferroly forbids `unsafe`, so the wipe is an ordinary write followed by
/// [`std::hint::black_box`]; there is no volatile write and the optimiser is
/// *discouraged*, not prevented, from eliding it. It also cannot reach copies
/// that exist outside the `Secret`: the `&str`/`&[u8]` you built it from,
/// anything you copy out of [`expose`](Secret::expose), a `String` that
/// reallocated before being converted, swap or core dumps. Treat it as
/// defence in depth, not as a guarantee. (`From<String>` and `From<Vec<u8>>`
/// take over the existing allocation without copying.)
pub struct Secret {
    bytes: Vec<u8>,
}

impl Secret {
    /// Wraps secret bytes. `Vec<u8>` and `String` are taken over without a
    /// copy; borrowed data is copied.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Secret {
            bytes: bytes.into(),
        }
    }

    /// The secret bytes. Keep the borrow short and do not log it.
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }

    /// The secret as text, or `None` if it is not valid UTF-8.
    pub fn expose_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    /// The length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Compares against `other` without an early exit on the first differing
    /// byte. Only the length is compared (and therefore revealed) up front.
    pub fn ct_eq(&self, other: &[u8]) -> bool {
        if self.bytes.len() != other.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in self.bytes.iter().zip(other) {
            diff |= a ^ b;
        }
        std::hint::black_box(diff) == 0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Zero the initialised bytes, then the rest of the allocation (a
        // `String` that was truncated before conversion may have left data in
        // the spare capacity).
        for b in self.bytes.iter_mut() {
            *b = 0;
        }
        self.bytes.clear();
        for slot in self.bytes.spare_capacity_mut() {
            slot.write(0);
        }
        std::hint::black_box(&self.bytes);
    }
}

impl Clone for Secret {
    fn clone(&self) -> Self {
        Secret {
            bytes: self.bytes.clone(),
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(&other.bytes)
    }
}

impl Eq for Secret {}

impl From<Vec<u8>> for Secret {
    fn from(bytes: Vec<u8>) -> Self {
        Secret { bytes }
    }
}

impl From<String> for Secret {
    fn from(text: String) -> Self {
        Secret {
            bytes: text.into_bytes(),
        }
    }
}

impl From<&str> for Secret {
    fn from(text: &str) -> Self {
        Secret {
            bytes: text.as_bytes().to_vec(),
        }
    }
}

impl From<&String> for Secret {
    fn from(text: &String) -> Self {
        Secret::from(text.as_str())
    }
}

impl From<&[u8]> for Secret {
    fn from(bytes: &[u8]) -> Self {
        Secret {
            bytes: bytes.to_vec(),
        }
    }
}

impl<const N: usize> From<&[u8; N]> for Secret {
    fn from(bytes: &[u8; N]) -> Self {
        Secret {
            bytes: bytes.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_are_redacted() {
        let secret = Secret::from("hunter2-hunter2");
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Config {
            user: &'static str,
            password: Secret,
        }
        let cfg = Config {
            user: "alice",
            password: secret.clone(),
        };
        for text in [
            format!("{secret}"),
            format!("{secret:?}"),
            format!("{secret:#?}"),
            format!("{cfg:?}"),
            format!("{:?}", Some(&secret)),
        ] {
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.contains("[REDACTED]"), "{text}");
        }
    }

    #[test]
    fn expose_returns_the_content() {
        let text = Secret::from(String::from("p@ss"));
        assert_eq!(text.expose(), b"p@ss");
        assert_eq!(text.expose_str(), Some("p@ss"));
        assert_eq!(text.len(), 4);
        assert!(!text.is_empty());

        let binary = Secret::new(vec![0xff, 0xfe]);
        assert_eq!(binary.expose(), [0xff, 0xfe]);
        assert_eq!(binary.expose_str(), None);
        assert!(Secret::new(Vec::new()).is_empty());
        assert_eq!(Secret::from(b"abc").expose(), b"abc");
        assert_eq!(Secret::from(&b"abc"[..]).expose(), b"abc");
    }

    #[test]
    fn equality_is_by_content() {
        let a = Secret::from("same");
        assert!(a == Secret::from("same"));
        assert!(a != Secret::from("sama"));
        assert!(a != Secret::from("same-but-longer"));
        assert!(a.ct_eq(b"same"));
        assert!(!a.ct_eq(b"sam"));
        assert!(!a.ct_eq(b""));
    }

    #[test]
    fn clone_is_independent_of_the_original() {
        let a = Secret::from("abc");
        let b = a.clone();
        drop(a);
        assert_eq!(b.expose(), b"abc");
    }
}
