//! 帧头：40 字节，同一帧里重复 3 份，任意一份 CRC 通过即可用。

use crate::{FORMAT_VERSION, HEADER_BYTES, HEADER_COPIES, SLOT_OVERHEAD};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    /// 生成时的模块像素（仅参考，识别端以实测为准）
    pub module_px: u8,
    pub grid_w: u16,
    pub grid_h: u16,
    pub block_bytes: u16,
    pub k: u16,
    pub m: u16,
    pub total_shards: u32,
    pub file_len: u64,
    pub hash_prefix: [u8; 8],
}

/// 构造帧头所需的全部字段，避免 `Header::new` 参数过多
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderSpec {
    pub module_px: u32,
    pub grid_w: u32,
    pub grid_h: u32,
    pub block_bytes: usize,
    pub k: usize,
    pub m: usize,
    pub total_shards: usize,
    pub file_len: u64,
    pub hash_prefix: [u8; 8],
}

impl Header {
    pub fn new(spec: HeaderSpec) -> Self {
        Self {
            version: FORMAT_VERSION,
            module_px: spec.module_px.min(255) as u8,
            grid_w: spec.grid_w as u16,
            grid_h: spec.grid_h as u16,
            block_bytes: spec.block_bytes as u16,
            k: spec.k as u16,
            m: spec.m as u16,
            total_shards: spec.total_shards as u32,
            file_len: spec.file_len,
            hash_prefix: spec.hash_prefix,
        }
    }

    pub fn slot_bytes(&self) -> usize {
        self.block_bytes as usize + SLOT_OVERHEAD
    }

    pub fn groups(&self) -> usize {
        let per = self.k as usize + self.m as usize;
        (self.total_shards as usize).checked_div(per).unwrap_or(0)
    }

    pub fn data_blocks(&self) -> usize {
        self.groups() * self.k as usize
    }

    /// 序列化成 40 字节（含 CRC32）
    pub fn to_bytes(&self) -> [u8; HEADER_BYTES] {
        let mut buf = [0u8; HEADER_BYTES];
        buf[0] = b'S';
        buf[1] = b'C';
        buf[2] = self.version;
        buf[3] = self.module_px;
        buf[4..6].copy_from_slice(&self.grid_w.to_le_bytes());
        buf[6..8].copy_from_slice(&self.grid_h.to_le_bytes());
        buf[8..10].copy_from_slice(&self.block_bytes.to_le_bytes());
        buf[10..12].copy_from_slice(&self.k.to_le_bytes());
        buf[12..14].copy_from_slice(&self.m.to_le_bytes());
        buf[14..18].copy_from_slice(&self.total_shards.to_le_bytes());
        buf[18..26].copy_from_slice(&self.file_len.to_le_bytes());
        buf[26..34].copy_from_slice(&self.hash_prefix);
        let crc = crc32fast::hash(&buf[..34]);
        buf[34..38].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// 解析并校验；magic/版本/CRC/字段合法性任一不符返回 None
    pub fn from_bytes(buf: &[u8; HEADER_BYTES]) -> Option<Self> {
        if buf[0] != b'S' || buf[1] != b'C' {
            return None;
        }
        let crc = u32::from_le_bytes([buf[34], buf[35], buf[36], buf[37]]);
        if crc != crc32fast::hash(&buf[..34]) {
            return None;
        }

        let version = buf[2];
        if version != FORMAT_VERSION {
            return None;
        }

        let grid_w = u16::from_le_bytes([buf[4], buf[5]]) as u32;
        let grid_h = u16::from_le_bytes([buf[6], buf[7]]) as u32;
        let block_bytes = u16::from_le_bytes([buf[8], buf[9]]) as usize;
        let k = u16::from_le_bytes([buf[10], buf[11]]) as usize;
        let m = u16::from_le_bytes([buf[12], buf[13]]) as usize;
        let total_shards = u32::from_le_bytes([buf[14], buf[15], buf[16], buf[17]]) as usize;
        let file_len = u64::from_le_bytes([
            buf[18], buf[19], buf[20], buf[21], buf[22], buf[23], buf[24], buf[25],
        ]);
        let mut hash_prefix = [0u8; 8];
        hash_prefix.copy_from_slice(&buf[26..34]);

        if grid_w < crate::HEADER_W + crate::BORDER_MODULES * 2
            || grid_h < crate::HEADER_H + crate::BORDER_MODULES * 2
            || block_bytes == 0
            || k == 0
            || k + m > 256
            || total_shards == 0
            || !total_shards.is_multiple_of(k + m)
            || file_len == 0
        {
            return None;
        }

        Some(Self {
            version,
            module_px: buf[3],
            grid_w: grid_w as u16,
            grid_h: grid_h as u16,
            block_bytes: block_bytes as u16,
            k: k as u16,
            m: m as u16,
            total_shards: total_shards as u32,
            file_len,
            hash_prefix,
        })
    }

    /// 帧头位流：3 份副本 + 0 填充，正好 `HEADER_W * HEADER_H` 位 = 128 字节
    pub fn to_bit_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity((crate::HEADER_W * crate::HEADER_H / 8) as usize);
        for _ in 0..HEADER_COPIES {
            out.extend_from_slice(&self.to_bytes());
        }
        let want = (crate::HEADER_W * crate::HEADER_H / 8) as usize;
        out.resize(want, 0);
        out
    }

    /// 从帧头区域的位流字节里挑出 CRC 通过的那一份
    pub fn from_bit_bytes(bytes: &[u8]) -> Option<Self> {
        for i in 0..HEADER_COPIES {
            let start = i * HEADER_BYTES;
            let end = start + HEADER_BYTES;
            if end > bytes.len() {
                break;
            }
            let mut buf = [0u8; HEADER_BYTES];
            buf.copy_from_slice(&bytes[start..end]);
            if let Some(header) = Self::from_bytes(&buf) {
                return Some(header);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header::new(HeaderSpec {
            module_px: 2,
            grid_w: 1244,
            grid_h: 656,
            block_bytes: 4096,
            k: 100,
            m: 10,
            total_shards: 8470,
            file_len: 31_457_280,
            hash_prefix: [1, 2, 3, 4, 5, 6, 7, 8],
        })
    }

    #[test]
    fn header_roundtrip() {
        let h = sample();
        let bytes = h.to_bytes();
        assert_eq!(Header::from_bytes(&bytes), Some(h));
    }

    #[test]
    fn header_rejects_corruption() {
        let h = sample();
        let mut bytes = h.to_bytes();
        bytes[10] ^= 0xFF; // 破坏 k
        assert_eq!(Header::from_bytes(&bytes), None);

        bytes = h.to_bytes();
        bytes[0] = b'X';
        assert_eq!(Header::from_bytes(&bytes), None);
    }

    #[test]
    fn bit_bytes_has_three_copies_and_padding() {
        let h = sample();
        let bits = h.to_bit_bytes();
        assert_eq!(bits.len(), (crate::HEADER_W * crate::HEADER_H / 8) as usize);
        assert_eq!(Header::from_bit_bytes(&bits), Some(h.clone()));

        // 第 2、3 份坏掉，仍能从第 1 份恢复
        let mut broken = bits.clone();
        for i in 1..HEADER_COPIES {
            broken[i * HEADER_BYTES] ^= 0xFF;
        }
        assert_eq!(Header::from_bit_bytes(&broken), Some(h));
    }
}
