//! SHA-256 摘要与十六进制展示。

use sha2::{Digest, Sha256};

/// 计算 SHA-256 摘要
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// 计算 SHA-256 并转成小写 hex
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha256(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abc_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn digest_starts_with_hex_prefix() {
        let digest = sha256(b"abc");
        assert_eq!(&hex::encode(digest)[..16], "ba7816bf8f01cfea");
    }
}
