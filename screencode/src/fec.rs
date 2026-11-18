//! 文件级纠删码：`k` 个数据分片 + `m` 个校验分片为一组，任意 `k` 个即可恢复整组。
//!
//! 这一层是「不再人工补漏」的关键：被 HUD 压住、抓屏撕裂、显示缩放导致边缘采样错的
//! 分片都只算「丢失」，由校验分片自动补回。

use anyhow::{bail, Result};
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::header::Header;

/// 单组最多多少个数据分片（校验分片数受 `k+m <= 256` 约束）
pub const K_MAX: usize = 100;
/// 单组分片总数上限（GF(256)）
pub const MAX_SHARDS_PER_GROUP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecPlan {
    pub k: usize,
    pub m: usize,
    pub data_blocks: usize,
    pub groups: usize,
    pub total_shards: usize,
}

impl FecPlan {
    pub fn shard_bytes(&self, block_bytes: usize) -> usize {
        block_bytes + crate::SLOT_OVERHEAD
    }

    /// 第 `block` 个数据分片的全局下标
    pub fn global_index(&self, block: usize) -> usize {
        let per = self.k + self.m;
        (block / self.k) * per + (block % self.k)
    }

    /// 全局下标 -> (组, 组内序号)
    pub fn locate(&self, global: usize) -> (usize, usize) {
        let per = self.k + self.m;
        (global / per, global % per)
    }
}

/// 规划纠删码分组
pub fn plan(file_len: usize, block_bytes: usize, parity: f64) -> Result<FecPlan> {
    if file_len == 0 {
        bail!("文件为空，无法生成");
    }
    if block_bytes == 0 {
        bail!("block_bytes 必须 >= 1");
    }
    if !(0.0..=1.0).contains(&parity) {
        bail!("parity 必须在 0.0..=1.0 之间");
    }

    let data_blocks = file_len.div_ceil(block_bytes);
    let k = data_blocks.min(K_MAX).max(1);
    let mut m = if parity > 0.0 {
        ((k as f64) * parity).round() as usize
    } else {
        0
    };
    if parity > 0.0 {
        m = m.max(1);
    }
    m = m.min(MAX_SHARDS_PER_GROUP - k);

    let groups = data_blocks.div_ceil(k);
    let total_shards = groups * (k + m);

    Ok(FecPlan {
        k,
        m,
        data_blocks,
        groups,
        total_shards,
    })
}

/// 把文件编码成全部数据分片 + 校验分片的净荷
pub fn encode_shards(file: &[u8], plan: &FecPlan, block_bytes: usize) -> Result<Vec<Vec<u8>>> {
    let mut shards = vec![vec![0u8; block_bytes]; plan.total_shards];

    for block in 0..plan.data_blocks {
        let start = block * block_bytes;
        let end = (start + block_bytes).min(file.len());
        let idx = plan.global_index(block);
        let len = end - start;
        shards[idx][..len].copy_from_slice(&file[start..end]);
    }

    if plan.m > 0 {
        let rs = ReedSolomon::new(plan.k, plan.m)?;
        for group in 0..plan.groups {
            let base = group * (plan.k + plan.m);
            rs.encode(&mut shards[base..base + plan.k + plan.m])?;
        }
    }

    Ok(shards)
}

/// 从 header 还原规划
pub fn plan_from_header(header: &Header) -> Option<FecPlan> {
    let k = header.k as usize;
    let m = header.m as usize;
    let total_shards = header.total_shards as usize;
    let per = k + m;
    if per == 0 || total_shards % per != 0 {
        return None;
    }
    let groups = total_shards / per;
    let data_blocks = groups * k;
    Some(FecPlan {
        k,
        m,
        data_blocks,
        groups,
        total_shards,
    })
}

/// 把已收到的分片还原成原始文件。
///
/// 每组：数据分片全在就直接取；否则只要有 >= k 个有效分片就做纠删恢复。
pub fn assemble(shards: &[Option<Vec<u8>>], header: &Header) -> Result<Vec<u8>, AssembleError> {
    let plan = plan_from_header(header).ok_or(AssembleError::BadHeader)?;
    if shards.len() < plan.total_shards {
        return Err(AssembleError::BadHeader);
    }

    let mut data_blocks: Vec<Vec<u8>> = Vec::with_capacity(plan.data_blocks);
    for group in 0..plan.groups {
        let base = group * (plan.k + plan.m);
        let mut group_shards: Vec<Option<Vec<u8>>> = (0..plan.k + plan.m)
            .map(|local| shards[base + local].clone())
            .collect();
        let available = group_shards.iter().filter(|s| s.is_some()).count();

        if available < plan.k {
            return Err(AssembleError::GroupShort {
                group,
                have: available,
                need: plan.k,
            });
        }

        if available < plan.k + plan.m && plan.m > 0 {
            let rs = ReedSolomon::new(plan.k, plan.m).map_err(|_| AssembleError::BadHeader)?;
            rs.reconstruct(&mut group_shards)
                .map_err(|_| AssembleError::Reconstruct { group })?;
        }

        for local in 0..plan.k {
            data_blocks.push(group_shards[local].take().unwrap_or_default());
        }
    }

    let mut out = Vec::with_capacity(header.file_len as usize);
    for block in data_blocks {
        out.extend_from_slice(&block);
        if out.len() >= header.file_len as usize {
            break;
        }
    }
    out.truncate(header.file_len as usize);

    if out.len() != header.file_len as usize {
        return Err(AssembleError::BadHeader);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub enum AssembleError {
    BadHeader,
    GroupShort {
        group: usize,
        have: usize,
        need: usize,
    },
    Reconstruct {
        group: usize,
    },
}

impl std::fmt::Display for AssembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssembleError::BadHeader => write!(f, "帧头/分片参数不一致"),
            AssembleError::GroupShort { group, have, need } => {
                write!(
                    f,
                    "第 {} 组只有 {} 个有效分片，至少需要 {}",
                    group, have, need
                )
            }
            AssembleError::Reconstruct { group } => write!(f, "第 {} 组纠删恢复失败", group),
        }
    }
}

impl std::error::Error for AssembleError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_file(len: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_9abc_def0u64;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    fn header_for(file_len: usize, plan: &FecPlan, block_bytes: usize) -> Header {
        Header::new(
            2,
            1244,
            656,
            block_bytes,
            plan.k,
            plan.m,
            plan.total_shards,
            file_len as u64,
            [0u8; 8],
        )
    }

    #[test]
    fn plan_is_consistent() {
        let p = plan(31_457_280, 4096, 0.10).expect("plan");
        assert_eq!(p.data_blocks, 7680);
        assert_eq!(p.k, 100);
        assert_eq!(p.m, 10);
        assert_eq!(p.groups, 77);
        assert_eq!(p.total_shards, 8470);
    }

    #[test]
    fn encode_and_assemble_without_loss() {
        let block = 4096;
        let file = sample_file(200_000);
        let p = plan(file.len(), block, 0.10).expect("plan");
        let shards = encode_shards(&file, &p, block).expect("encode");
        let opts: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        let header = header_for(file.len(), &p, block);
        let out = assemble(&opts, &header).expect("assemble");
        assert_eq!(out, file);
    }

    #[test]
    fn assemble_recovers_dropped_shards() {
        let block = 1024;
        let file = sample_file(300_000);
        let p = plan(file.len(), block, 0.10).expect("plan");
        let shards = encode_shards(&file, &p, block).expect("encode");
        let header = header_for(file.len(), &p, block);

        // 每组丢掉 3 个（< m=10）分片
        let mut dropped = 0;
        let opts: Vec<Option<Vec<u8>>> = shards
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let (_group, local) = p.locate(i);
                if local < 3 && dropped < 1000 {
                    dropped += 1;
                    None
                } else {
                    Some(s)
                }
            })
            .collect();

        let out = assemble(&opts, &header).expect("assemble with erasures");
        assert_eq!(out, file);
    }

    #[test]
    fn assemble_reports_short_group() {
        let block = 1024;
        let file = sample_file(100_000);
        let p = plan(file.len(), block, 0.10).expect("plan");
        let shards = encode_shards(&file, &p, block).expect("encode");
        let header = header_for(file.len(), &p, block);

        // 第 0 组只留 k-1 个分片
        let mut dropped = 0;
        let opts: Vec<Option<Vec<u8>>> = shards
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let (group, local) = p.locate(i);
                if group == 0 && local < 3 {
                    dropped += 1;
                    // 再额外丢到不足 k 个
                }
                if group == 0 && local < p.k + p.m - (p.k - 1) {
                    None
                } else {
                    Some(s)
                }
            })
            .collect();
        assert!(dropped > 0);
        let err = assemble(&opts, &header).expect_err("should be short");
        assert!(matches!(err, AssembleError::GroupShort { .. }));
    }

    #[test]
    fn parity_zero_needs_all_shards() {
        let block = 2048;
        let file = sample_file(64 * 1024);
        let p = plan(file.len(), block, 0.0).expect("plan");
        assert_eq!(p.m, 0);
        let shards = encode_shards(&file, &p, block).expect("encode");
        let header = header_for(file.len(), &p, block);

        let mut opts: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        opts[0] = None;
        assert!(assemble(&opts, &header).is_err());
    }
}
