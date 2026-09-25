//! HMAC-SHA256 (RFC 2104), built on `sha256::sha256`. Used by
//! `gems-abac::token` for JWT-style access token signing/verification and
//! by `gems-cluster`'s peer-frame authentication.

use crate::sha256::{sha256, OUTPUT_LEN as HASH_LEN};

const BLOCK_LEN: usize = 64; // SHA-256's block size

pub const TAG_LEN: usize = HASH_LEN;

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; TAG_LEN] {
    let mut block_key = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        let hashed = sha256(key);
        block_key[..HASH_LEN].copy_from_slice(&hashed);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_LEN];
    let mut opad = [0x5cu8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        ipad[i] ^= block_key[i];
        opad[i] ^= block_key[i];
    }

    let mut inner_input = Vec::with_capacity(BLOCK_LEN + message.len());
    inner_input.extend_from_slice(&ipad);
    inner_input.extend_from_slice(message);
    let inner_hash = sha256(&inner_input);

    let mut outer_input = Vec::with_capacity(BLOCK_LEN + HASH_LEN);
    outer_input.extend_from_slice(&opad);
    outer_input.extend_from_slice(&inner_hash);
    sha256(&outer_input)
}

/// Constant-time comparison — verifying a MAC/signature with `==` leaks
/// timing information proportional to how many leading bytes match, which
/// an attacker can use to forge a valid tag one byte at a time. Every
/// caller of `hmac_sha256` that checks a tag against an attacker-supplied
/// value (a JWT signature, a peer-frame tag) must use this instead of
/// `==`.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn matches_rfc4231_test_case_1() {
        // RFC 4231 §4.2: key = 20 bytes of 0x0b, data = "Hi There".
        let key = [0x0bu8; 20];
        let tag = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn matches_rfc4231_test_case_2() {
        // RFC 4231 §4.3: key = "Jefe", data = "what do ya want for nothing?".
        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&tag),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn key_longer_than_block_size_is_hashed_first() {
        // RFC 4231 §4.7: a 131-byte key, exercising the >BLOCK_LEN branch.
        let key = [0xaau8; 131];
        let data = b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.";
        let tag = hmac_sha256(&key, data);
        assert_eq!(
            hex(&tag),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    #[test]
    fn is_deterministic_and_key_dependent() {
        let a = hmac_sha256(b"key1", b"message");
        let b = hmac_sha256(b"key1", b"message");
        let c = hmac_sha256(b"key2", b"message");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn constant_time_eq_matches_regular_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
