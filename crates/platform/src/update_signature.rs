//! Ed25519 verification of an update's signature (DESIGN.md §11.4).
//!
//! `WinSparkle` verifies a download before it hands Code++ the file, and
//! then leaves the file where the user's other programs can rewrite it
//! (measured, DESIGN.md §11.11). So Code++ verifies the file it is about
//! to run itself, against the same public key and the signature the feed
//! carries for it. This module is that verification and nothing else: no
//! file is opened and no request made here.
//!
//! The signature is plain Ed25519 (RFC 8032) over the file's bytes, which
//! is what `winsparkle-tool sign` and Sparkle's `sign_update` produce.
//! Measured for `WinSparkle` 0.9.4: OpenSSL verifies its signature as
//! Ed25519 over the raw file, and this module's tests hold one of its
//! signatures.
//!
//! Windows only, because only the Windows updater needs it: Sparkle
//! verifies and installs by itself on macOS, and Linux has no in-app
//! updater. Windows' own CNG offers no Ed25519 (measured on build 26300),
//! so this is the one place the tree uses a crypto crate.

use ed25519_dalek::{Signature, VerifyingKey};

/// Length of an Ed25519 public key, in bytes.
pub const PUBLIC_KEY_LEN: usize = 32;

/// Length of an Ed25519 signature, in bytes.
pub const SIGNATURE_LEN: usize = 64;

/// Whether `signature` is `public_key`'s Ed25519 signature of `message`.
///
/// Strict verification: besides the signature equation, a public key or
/// signature point of small order is refused, and so is a signature
/// whose scalar is not in canonical form. A key that is not a point on
/// the curve verifies nothing.
///
/// Two things this cannot do for its caller, and that make the answer
/// worth something only when the caller does them (DESIGN.md §11.4):
///
/// * `message` is the whole file, in memory. The caller bounds what it
///   reads, since the file is one another program may have swapped for
///   something very large.
/// * A `true` is about the bytes handed in. The caller reads them
///   through the handle it then keeps open, denying writes, until the
///   installer is done with the file; bytes read from a path that is
///   opened again afterwards prove nothing about what runs.
#[must_use]
pub fn verifies(
    public_key: &[u8; PUBLIC_KEY_LEN],
    signature: &[u8; SIGNATURE_LEN],
    message: &[u8],
) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    key.verify_strict(message, &Signature::from_bytes(signature))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a hex string of exactly `N` bytes.
    fn hex<const N: usize>(text: &str) -> [u8; N] {
        assert_eq!(text.len(), 2 * N, "{text}");
        let mut out = [0u8; N];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex");
        }
        out
    }

    /// RFC 8032 §7.1, tests 1 to 3: public key, message, signature.
    const RFC8032: [(&str, &[u8], &str); 3] = [
        (
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            b"",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
             5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        ),
        (
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
            b"\x72",
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
             085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        ),
        (
            "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
            b"\xaf\x82",
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
             18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
        ),
    ];

    /// A signature made by `winsparkle-tool sign` (`WinSparkle` 0.9.4)
    /// with a throwaway key: the public key it printed, the file's
    /// bytes, and the signature it printed, each as the tool gives them,
    /// the two base64 texts decoded.
    const WINSPARKLE_KEY: &str = "19f63b6140366f7bef22513ed26990cc65ab4beef74902ffebfebc3311af86f8";
    const WINSPARKLE_MESSAGE: &[u8] = b"Code++ update signature test\n";
    const WINSPARKLE_SIGNATURE: &str = "\
        1719fffdd061acd9e97e3463d955c431421811fdc9c5b8ab57aa1852d929a1fe\
        73cdeba7b99b6ecd427937838ea5303bb09b928ca913e6aa147c8a5356b53504";

    #[test]
    fn the_rfc_8032_vectors_verify() {
        for (key, message, signature) in RFC8032 {
            let signature: String = signature.split_whitespace().collect();
            assert!(verifies(&hex(key), &hex(&signature), message), "{key}");
        }
    }

    #[test]
    fn a_signature_made_by_winsparkles_tool_verifies() {
        let signature: String = WINSPARKLE_SIGNATURE.split_whitespace().collect();
        assert!(verifies(
            &hex(WINSPARKLE_KEY),
            &hex(&signature),
            WINSPARKLE_MESSAGE
        ));
    }

    /// One bit changed anywhere, in the message, the signature or the
    /// key, and the signature no longer verifies; nor does it for a
    /// message cut short or grown by a byte.
    #[test]
    fn a_changed_message_signature_or_key_does_not_verify() {
        let key: [u8; PUBLIC_KEY_LEN] = hex(WINSPARKLE_KEY);
        let signature: String = WINSPARKLE_SIGNATURE.split_whitespace().collect();
        let signature: [u8; SIGNATURE_LEN] = hex(&signature);
        let message = WINSPARKLE_MESSAGE;

        for i in 0..message.len() {
            let mut changed = message.to_vec();
            changed[i] ^= 1;
            assert!(!verifies(&key, &signature, &changed), "message byte {i}");
        }
        assert!(!verifies(&key, &signature, &message[..message.len() - 1]));
        let mut longer = message.to_vec();
        longer.push(b'\n');
        assert!(!verifies(&key, &signature, &longer));
        assert!(!verifies(&key, &signature, b""));

        for i in 0..SIGNATURE_LEN {
            let mut changed = signature;
            changed[i] ^= 1;
            assert!(!verifies(&key, &changed, message), "signature byte {i}");
        }
        for i in 0..PUBLIC_KEY_LEN {
            let mut changed = key;
            changed[i] ^= 1;
            assert!(!verifies(&changed, &signature, message), "key byte {i}");
        }
    }

    /// Another key's valid signature of the same message does not verify.
    #[test]
    fn another_keys_signature_does_not_verify() {
        let (rfc_key, rfc_message, rfc_signature) = RFC8032[1];
        let rfc_signature: String = rfc_signature.split_whitespace().collect();
        assert!(verifies(&hex(rfc_key), &hex(&rfc_signature), rfc_message));
        assert!(!verifies(
            &hex(WINSPARKLE_KEY),
            &hex(&rfc_signature),
            rfc_message
        ));
    }

    /// Keys strict verification refuses whatever the signature: the
    /// identity point, which has small order, and bytes that are no
    /// point at all. Under the identity key, the signature made of the
    /// identity point and a zero scalar (`identity_signature` below)
    /// satisfies the plain equation for any message, which is what
    /// strict verification is there to refuse.
    #[test]
    fn a_weak_or_invalid_key_verifies_nothing() {
        let mut identity = [0u8; PUBLIC_KEY_LEN];
        identity[0] = 1;
        let zero_signature = [0u8; SIGNATURE_LEN];
        let mut identity_signature = [0u8; SIGNATURE_LEN];
        identity_signature[0] = 1;
        for message in [&b""[..], b"anything"] {
            assert!(!verifies(&identity, &zero_signature, message));
            assert!(!verifies(&identity, &identity_signature, message));
            assert!(!verifies(&[0u8; PUBLIC_KEY_LEN], &zero_signature, message));
            assert!(!verifies(
                &[0xFF; PUBLIC_KEY_LEN],
                &identity_signature,
                message
            ));
        }
    }
}
