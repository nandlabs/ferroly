//! Pieces of the WebSocket opening handshake (RFC 6455 §4) shared by the
//! client and the server: the accept key and header-token helpers.

use ferroly::hash::base64;
use ferroly::http::HeaderMap;

/// The RFC 6455 GUID appended to the client key before hashing.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Computes the `Sec-WebSocket-Accept` value for a client key.
pub(crate) fn accept_key(client_key: &str) -> String {
    let mut input = String::with_capacity(client_key.len() + WS_GUID.len());
    input.push_str(client_key);
    input.push_str(WS_GUID);
    base64::encode(&ferroly::hash::sha1(input.as_bytes()).into_bytes())
}

/// Encodes a 16-byte nonce as a `Sec-WebSocket-Key` value.
pub(crate) fn encode_key(nonce: &[u8; 16]) -> String {
    base64::encode(nonce)
}

/// Whether `key` is a well-formed `Sec-WebSocket-Key`: canonical, padded
/// base64 of exactly 16 bytes (always 24 characters).
pub(crate) fn valid_client_key(key: &str) -> bool {
    key.len() == 24 && matches!(base64::decode(key), Ok(bytes) if bytes.len() == 16)
}

/// Whether `s` is a non-empty RFC 9110 token (the grammar of subprotocol
/// names and of the values compared in `Upgrade` / `Connection`).
pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// The comma-separated elements of every `name` header line, trimmed, empty
/// elements skipped.
pub(crate) fn list_items<'a>(
    headers: &'a HeaderMap,
    name: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(name)
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

/// Whether the list-valued header `name` contains `token`
/// (case-insensitively).
pub(crate) fn has_token(headers: &HeaderMap, name: &str, token: &str) -> bool {
    list_items(headers, name).any(|item| item.eq_ignore_ascii_case(token))
}

/// The value of a header that must not be repeated: `Ok(None)` when absent,
/// `Err(())` when it appears more than once.
pub(crate) fn single<'a>(headers: &'a HeaderMap, name: &'a str) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name);
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(first.map(str::trim))
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
    fn client_key_must_be_16_bytes_of_canonical_base64() {
        assert!(valid_client_key("dGhlIHNhbXBsZSBub25jZQ=="));
        assert!(valid_client_key(&encode_key(&[7u8; 16])));
        assert!(!valid_client_key(""));
        assert!(!valid_client_key("abc"));
        assert!(!valid_client_key("dGhlIHNhbXBsZSBub25jZQ")); // padding missing
        assert!(!valid_client_key("dGhlIHNhbXBsZSBub25jZQ==\n"));
        assert!(!valid_client_key("!!!!IHNhbXBsZSBub25jZQ=="));
        // 15 and 18 bytes.
        assert!(!valid_client_key("AAAAAAAAAAAAAAAAAAAA"));
        assert!(!valid_client_key("AAAAAAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn header_token_helpers() {
        let mut h = HeaderMap::new();
        h.append("Connection", "keep-alive, Upgrade");
        h.append("Sec-WebSocket-Protocol", "chat");
        h.append("Sec-WebSocket-Protocol", " superchat ,, v2");
        assert!(has_token(&h, "connection", "upgrade"));
        assert!(!has_token(&h, "connection", "close"));
        let offered: Vec<&str> = list_items(&h, "sec-websocket-protocol").collect();
        assert_eq!(offered, ["chat", "superchat", "v2"]);
        assert_eq!(single(&h, "connection"), Ok(Some("keep-alive, Upgrade")));
        assert_eq!(single(&h, "missing"), Ok(None));
        assert_eq!(single(&h, "sec-websocket-protocol"), Err(()));
        assert!(is_token("chat.v2") && !is_token("a b") && !is_token(""));
    }
}
