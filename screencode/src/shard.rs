//! 分片 slot 编解码：固定长度，自带全局下标与 CRC32。
//!
//! 布局：`idx(u32 LE) | payload(block_bytes) | crc32(u32 LE)`，CRC 覆盖 idx+payload。
//! 固定长度意味着局部损坏只影响自己这个 slot，不会让后续 slot 错位。

use crate::{PAD_SLOT, SLOT_OVERHEAD};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRecord {
    pub idx: u32,
    pub payload: Vec<u8>,
}

/// 编码一个 slot
pub fn encode_slot(idx: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + SLOT_OVERHEAD);
    out.extend_from_slice(&idx.to_le_bytes());
    out.extend_from_slice(payload);
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// 填充 slot：识别端读到 `PAD_SLOT` 直接跳过
pub fn pad_slot(block_bytes: usize) -> Vec<u8> {
    encode_slot(PAD_SLOT, &vec![0u8; block_bytes])
}

/// 解析并校验一个 slot；长度必须正好是 `block_bytes + 8`
pub fn parse_slot(bytes: &[u8], block_bytes: usize) -> Option<SlotRecord> {
    if bytes.len() != block_bytes + SLOT_OVERHEAD {
        return None;
    }
    let crc_off = block_bytes + 4;
    let want = u32::from_le_bytes([
        bytes[crc_off],
        bytes[crc_off + 1],
        bytes[crc_off + 2],
        bytes[crc_off + 3],
    ]);
    if crc32fast::hash(&bytes[..crc_off]) != want {
        return None;
    }
    let idx = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    Some(SlotRecord {
        idx,
        payload: bytes[4..4 + block_bytes].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_roundtrip() {
        let payload: Vec<u8> = (0..64).map(|i| (i * 7) as u8).collect();
        let bytes = encode_slot(42, &payload);
        let parsed = parse_slot(&bytes, 64).expect("parse");
        assert_eq!(parsed.idx, 42);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn slot_detects_corruption() {
        let mut bytes = encode_slot(7, &[1, 2, 3, 4]);
        bytes[5] ^= 0x40;
        assert!(parse_slot(&bytes, 4).is_none());
    }

    #[test]
    fn slot_rejects_wrong_length() {
        let bytes = encode_slot(7, &[1, 2, 3, 4]);
        assert!(parse_slot(&bytes, 5).is_none());
    }

    #[test]
    fn pad_slot_uses_pad_marker() {
        let bytes = pad_slot(8);
        let parsed = parse_slot(&bytes, 8).expect("parse");
        assert_eq!(parsed.idx, PAD_SLOT);
        assert_eq!(parsed.payload, vec![0u8; 8]);
    }
}
