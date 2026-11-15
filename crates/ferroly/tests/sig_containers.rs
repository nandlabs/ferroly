//! Ed25519 public-key containers: DER / PEM `SubjectPublicKeyInfo`
//! (RFC 8410) and OpenSSH `ssh-ed25519` lines (RFC 8709).

#![cfg(feature = "sig")]

use ferroly::hash::hex;
use ferroly::sig::{Ed25519PublicKey, SigError};

/// RFC 8410 §10.1: the example Ed25519 public key.
const RFC8410_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n\
-----END PUBLIC KEY-----\n";
const RFC8410_KEY: &str = "19bf44096984cdfe8541bac167dc3b96c85086aa30b6b6cb0c5c38ad703166e1";

/// Generated with `ssh-keygen -t ed25519`; the key bytes were extracted
/// independently with Python's base64 module.
const OPENSSH_LINE: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN/V6lp3hgZ39wVdKUhB15lepJLyIqLV2AH2TkbQpBFL test@example";
const OPENSSH_KEY: &str = "dfd5ea5a77860677f7055d294841d7995ea492f222a2d5d801f64e46d0a4114b";

#[test]
fn spki_pem_and_der_decode_the_rfc8410_example() {
    let key = Ed25519PublicKey::from_pem(RFC8410_PEM).unwrap();
    assert_eq!(hex::encode(key.as_bytes()), RFC8410_KEY);
    // Surrounding text and CRLF line breaks are fine.
    let crlf = format!("subject: example\r\n{}", RFC8410_PEM.replace('\n', "\r\n"));
    assert_eq!(Ed25519PublicKey::from_pem(&crlf), Ok(key.clone()));
    let der = key.to_spki_der();
    assert_eq!(der.len(), 44);
    assert_eq!(
        hex::encode(&der),
        format!("302a300506032b6570032100{RFC8410_KEY}")
    );
    assert_eq!(Ed25519PublicKey::from_spki_der(&der), Ok(key));
}

#[test]
fn malformed_spki_is_refused() {
    let der = hex::decode(format!("302a300506032b6570032100{RFC8410_KEY}")).unwrap();
    let mut cases: Vec<Vec<u8>> = vec![der[..43].to_vec(), [&der[..], &[0]].concat(), Vec::new()];
    // id-X25519 (1.3.101.110) instead of id-Ed25519.
    let mut x25519 = der.clone();
    x25519[8] = 0x6e;
    cases.push(x25519);
    // Unused bits in the BIT STRING.
    let mut bits = der.clone();
    bits[11] = 0x01;
    cases.push(bits);
    for case in cases {
        assert_eq!(
            Ed25519PublicKey::from_spki_der(&case),
            Err(SigError::MalformedKey)
        );
    }
    for pem in [
        "",
        "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n",
        "-----BEGIN PUBLIC KEY-----\n!!!!\n-----END PUBLIC KEY-----",
        "-----BEGIN RSA PUBLIC KEY-----\nMCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n-----END RSA PUBLIC KEY-----",
    ] {
        assert_eq!(Ed25519PublicKey::from_pem(pem), Err(SigError::MalformedKey), "{pem}");
    }
    // A small-order point is refused even in a well-formed container.
    let mut small = der[..12].to_vec();
    small.extend_from_slice(&[0u8; 32]);
    assert_eq!(
        Ed25519PublicKey::from_spki_der(&small),
        Err(SigError::MalformedKey)
    );
}

#[test]
fn openssh_lines_decode() {
    let key = Ed25519PublicKey::from_openssh(OPENSSH_LINE).unwrap();
    assert_eq!(hex::encode(key.as_bytes()), OPENSSH_KEY);
    // Without a comment, and with extra blanks.
    let bare = OPENSSH_LINE.rsplit_once(' ').unwrap().0;
    assert_eq!(Ed25519PublicKey::from_openssh(bare), Ok(key.clone()));
    assert_eq!(
        Ed25519PublicKey::from_openssh(&format!("  {bare}\t\n")),
        Ok(key)
    );
    for bad in [
        "",
        "ssh-ed25519",
        "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIN/V6lp3hgZ39wVdKUhB15lepJLyIqLV2AH2TkbQpBFL",
        // Truncated blob, and a blob naming another key type.
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN/V6lp3hgZ39wVdKUhB15lepJLyIqLV2AH2TkbQ",
        "ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQABAAAAgQ==",
        "ssh-ed25519 not-base64",
    ] {
        assert_eq!(
            Ed25519PublicKey::from_openssh(bad),
            Err(SigError::MalformedKey),
            "{bad}"
        );
    }
}
