//! WebSocket opening handshake (RFC 6455 §4): SHA-1 + base64 accept key and the
//! client/server upgrade request/response formatting.

/// The RFC 6455 GUID appended to the client key before hashing.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Computes the `Sec-WebSocket-Accept` value for a client key.
pub(crate) fn accept_key(client_key: &str) -> String {
    let mut input = client_key.to_string();
    input.push_str(WS_GUID);
    base64_encode(&sha1(input.as_bytes()))
}

/// Standard base64 encoding.
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// SHA-1 of `data` — delegates to the public [`ferroly::hash`](crate::hash)
/// implementation so the algorithm lives in one place.
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    ferroly::hash::sha1(data).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_known_vector() {
        // RFC 6455 §1.3 example.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_basic() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
    }
}
