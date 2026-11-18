//! 分片库：把收到的分片按全局下标落盘，支持续传与纠删恢复。
//!
//! 落盘布局：`.qr_blocks.bin` 是稀疏文件，第 `idx` 个分片在 `idx * block_bytes` 处；
//! `.qr_state.json` 记录帧头快照与「已收到哪些分片」的位图。

use anyhow::{anyhow, bail, Result};
use screencode::fec;
use screencode::header::Header;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const BLOCKS_FILE: &str = ".qr_blocks.bin";
pub const STATE_FILE: &str = ".qr_state.json";
pub const LEGACY_SESSION: &str = ".qr_session.json";
pub const FORMAT: &str = "screencode/1";

#[derive(Debug, Clone)]
pub struct ShardPaths {
    pub blocks: PathBuf,
    pub state: PathBuf,
}

impl ShardPaths {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            blocks: dir.join(BLOCKS_FILE),
            state: dir.join(STATE_FILE),
        }
    }

    pub fn current() -> Self {
        Self::in_dir(Path::new("."))
    }
}

#[derive(Serialize, Deserialize)]
struct StateFile {
    format: String,
    /// 40 字节帧头的 hex
    header: String,
    /// 分片位图（每字节 8 个分片，MSB 在前）的 hex
    received: String,
    updated_at: u64,
}

pub struct ShardStore {
    pub header: Option<Header>,
    received: Vec<bool>,
    received_count: usize,
    block_bytes: usize,
    file: Option<File>,
    paths: ShardPaths,
    dirty: bool,
}

impl ShardStore {
    /// 新建（清空旧的）分片库
    pub fn create(paths: ShardPaths) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&paths.blocks)
            .map_err(|e| anyhow!("创建 {} 失败: {}", paths.blocks.display(), e))?;
        let _ = fs::remove_file(&paths.state);
        Ok(Self {
            header: None,
            received: Vec::new(),
            received_count: 0,
            block_bytes: 0,
            file: Some(file),
            paths,
            dirty: false,
        })
    }

    /// 打开上次的分片库；没有或不可用时返回 None
    pub fn open(paths: ShardPaths) -> Result<Option<Self>> {
        if !paths.state.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&paths.state)
            .map_err(|e| anyhow!("读取 {} 失败: {}", paths.state.display(), e))?;
        let state: StateFile = match serde_json::from_str(&text) {
            Ok(state) => state,
            Err(e) => {
                bail!("会话文件 {} 解析失败: {}", paths.state.display(), e);
            }
        };
        if state.format != FORMAT {
            bail!("会话格式 {} 与当前 {} 不一致", state.format, FORMAT);
        }

        let header_bytes =
            hex_decode(&state.header).ok_or_else(|| anyhow!("会话里的帧头不是合法 hex"))?;
        let header_array: [u8; 40] = header_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("会话里的帧头长度不对"))?;
        let header =
            Header::from_bytes(&header_array).ok_or_else(|| anyhow!("会话里的帧头校验失败"))?;

        let total = header.total_shards as usize;
        let bitmap =
            hex_decode(&state.received).ok_or_else(|| anyhow!("会话里的分片位图不是合法 hex"))?;
        let mut received = vec![false; total];
        for (i, byte) in bitmap.iter().enumerate() {
            for bit in 0..8 {
                let idx = i * 8 + bit;
                if idx >= total {
                    break;
                }
                received[idx] = (byte >> (7 - bit)) & 1 == 1;
            }
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&paths.blocks)
            .map_err(|e| anyhow!("打开 {} 失败: {}", paths.blocks.display(), e))?;

        let meta = file.metadata()?;
        let block_bytes = header.block_bytes as usize;
        let want = (total * block_bytes) as u64;
        if meta.len() < want {
            bail!(
                "分片库 {} 只有 {} 字节，小于需要的 {}",
                paths.blocks.display(),
                meta.len(),
                want
            );
        }

        let received_count = received.iter().filter(|v| **v).count();
        Ok(Some(Self {
            header: Some(header),
            received,
            received_count,
            block_bytes,
            file: Some(file),
            paths,
            dirty: false,
        }))
    }

    /// 首次看到帧头时初始化；返回 true 表示本次新建了会话
    pub fn ensure(&mut self, header: &Header) -> Result<bool> {
        if self.header.is_some() {
            // 是否一致由调用方判断；这里只负责「已初始化」
            return Ok(false);
        }

        let total = header.total_shards as usize;
        self.block_bytes = header.block_bytes as usize;
        self.received = vec![false; total];
        self.received_count = 0;
        self.header = Some(header.clone());
        let slot_bytes = total * self.block_bytes;
        let file = self.file_mut()?;
        file.set_len(slot_bytes as u64)?;
        file.seek(SeekFrom::Start(0))?;
        self.dirty = true;
        Ok(true)
    }

    fn file_mut(&mut self) -> Result<&mut File> {
        self.file.as_mut().ok_or_else(|| anyhow!("分片库已关闭"))
    }

    /// 关闭底层文件句柄（Windows 上删除文件前必须先关闭）
    pub fn close(&mut self) {
        self.file = None;
    }

    pub fn total_shards(&self) -> usize {
        self.header
            .as_ref()
            .map(|h| h.total_shards as usize)
            .unwrap_or(0)
    }

    pub fn received_count(&self) -> usize {
        self.received_count
    }

    /// 已收到的分片数 / 总数
    pub fn progress(&self) -> (usize, usize) {
        (self.received_count, self.total_shards())
    }

    pub fn has(&self, idx: usize) -> bool {
        self.received.get(idx).copied().unwrap_or(false)
    }

    /// 写入一个分片；返回是否是新分片
    pub fn ingest(&mut self, idx: usize, payload: &[u8]) -> Result<bool> {
        if idx >= self.total_shards() {
            bail!("分片下标 {} 超出总数 {}", idx, self.total_shards());
        }
        if payload.len() != self.block_bytes {
            bail!(
                "分片 {} 长度 {}，期望 {}",
                idx,
                payload.len(),
                self.block_bytes
            );
        }
        if self.received[idx] {
            return Ok(false);
        }

        let block_bytes = self.block_bytes;
        let file = self.file_mut()?;
        file.seek(SeekFrom::Start((idx * block_bytes) as u64))?;
        file.write_all(payload)?;
        self.received[idx] = true;
        self.received_count += 1;
        self.dirty = true;
        Ok(true)
    }

    fn read_shard(&mut self, idx: usize) -> Result<Vec<u8>> {
        let block_bytes = self.block_bytes;
        let mut buf = vec![0u8; block_bytes];
        let file = self.file_mut()?;
        file.seek(SeekFrom::Start((idx * block_bytes) as u64))?;
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// 每组是否都已有 >= k 个有效分片
    pub fn groups_ready(&self) -> (usize, usize) {
        let Some(header) = &self.header else {
            return (0, 0);
        };
        let k = header.k as usize;
        let per = k + header.m as usize;
        let groups = self.total_shards() / per;
        let mut ready = 0;
        for group in 0..groups {
            let base = group * per;
            let have = (0..per).filter(|local| self.received[base + local]).count();
            if have >= k {
                ready += 1;
            }
        }
        (ready, groups)
    }

    /// 用已收到的分片还原文件
    pub fn assemble(&mut self) -> Result<Vec<u8>> {
        let header = self.header.clone().ok_or_else(|| anyhow!("还没有帧头"))?;
        let total = self.total_shards();
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; total];
        for (idx, slot) in shards.iter_mut().enumerate() {
            if self.received[idx] {
                *slot = Some(self.read_shard(idx)?);
            }
        }
        let bytes = fec::assemble(&shards, &header)?;
        Ok(bytes)
    }

    /// 每个分片出现 CRC 失败/缺失导致的组缺口（用于提示提高 parity）
    pub fn group_deficits(&self) -> Vec<(usize, usize)> {
        let Some(header) = &self.header else {
            return Vec::new();
        };
        let k = header.k as usize;
        let per = k + header.m as usize;
        let groups = self.total_shards() / per;
        let mut out = Vec::new();
        for group in 0..groups {
            let base = group * per;
            let have = (0..per).filter(|local| self.received[base + local]).count();
            if have < k {
                out.push((group, k - have));
            }
        }
        out
    }

    /// 定时落盘状态位图（分片本体是即时写的）
    pub fn save_state(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(header) = &self.header else {
            return Ok(());
        };
        let mut bitmap = vec![0u8; self.received.len().div_ceil(8)];
        for (idx, got) in self.received.iter().enumerate() {
            if *got {
                bitmap[idx / 8] |= 0x80 >> (idx % 8);
            }
        }
        let state = StateFile {
            format: FORMAT.to_string(),
            header: hex_encode(&header.to_bytes()),
            received: hex_encode(&bitmap),
            updated_at: unix_now(),
        };
        let text = serde_json::to_string(&state)?;
        let tmp = self.paths.state.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.paths.state)?;
        self.dirty = false;
        Ok(())
    }

    pub fn paths(&self) -> &ShardPaths {
        &self.paths
    }
}

/// 清掉全部分片库/会话文件
pub fn clean(dir: &Path) -> Result<Vec<PathBuf>> {
    let paths = ShardPaths::in_dir(dir);
    let mut removed = Vec::new();
    for path in [paths.blocks, paths.state, dir.join(LEGACY_SESSION)] {
        if path.exists() {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// 会话文件大小（用于日志）
pub fn describe_store(dir: &Path) -> String {
    let paths = ShardPaths::in_dir(dir);
    let blocks = fs::metadata(&paths.blocks)
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0);
    format!("{:.2} MB 分片库", blocks)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// 取消重复计数的辅助：统计每个分片下标出现过的次数（诊断热区）
pub fn bump(map: &mut HashMap<usize, u32>, key: usize) {
    *map.entry(key).or_insert(0) += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use screencode::shard::encode_slot;

    fn temp_paths(tag: &str) -> ShardPaths {
        let dir =
            std::env::temp_dir().join(format!("screencode_store_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        ShardPaths::in_dir(&dir)
    }

    fn sample_header() -> Header {
        Header::new(2, 1244, 656, 1024, 10, 2, 12, 8000, [1; 8])
    }

    #[test]
    fn ingest_persist_and_reopen() {
        let paths = temp_paths("persist");
        let header = sample_header();
        let mut store = ShardStore::create(paths.clone()).expect("create");
        assert!(store.ensure(&header).expect("ensure"));

        let payload: Vec<u8> = (0..1024).map(|i| i as u8).collect();
        assert!(store.ingest(3, &payload).expect("ingest"));
        assert!(!store.ingest(3, &payload).expect("dup"));
        store.save_state().expect("save");

        let mut reopened = ShardStore::open(paths.clone())
            .expect("open")
            .expect("some");
        assert_eq!(reopened.header.as_ref(), Some(&header));
        assert_eq!(reopened.received_count(), 1);
        assert!(reopened.has(3));
        assert_eq!(reopened.read_shard(3).expect("read"), payload);

        let _ = fs::remove_dir_all(paths.blocks.parent().unwrap());
    }

    #[test]
    fn groups_ready_counts_parity() {
        let paths = temp_paths("ready");
        let header = sample_header();
        let mut store = ShardStore::create(paths.clone()).expect("create");
        store.ensure(&header).expect("ensure");

        let payload = vec![0u8; 1024];
        for i in 0..10 {
            store.ingest(i, &payload).expect("ingest");
        }
        assert_eq!(store.groups_ready(), (1, 1));
        let _ = fs::remove_dir_all(paths.blocks.parent().unwrap());
    }

    #[test]
    fn slot_payload_matches_store() {
        let payload = vec![9u8; 1024];
        let slot = encode_slot(5, &payload);
        let parsed = screencode::shard::parse_slot(&slot, 1024).expect("parse");
        assert_eq!(parsed.idx, 5);
        assert_eq!(parsed.payload, payload);
    }

    fn lcg_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    fn png_to_gray(png: &[u8]) -> (usize, usize, Vec<u8>) {
        let decoder = png::Decoder::new(png);
        let mut reader = decoder.read_info().expect("png info");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("png frame");
        let w = info.width as usize;
        let h = info.height as usize;
        let row_bytes = w.div_ceil(8);
        let mut gray = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let bit = (buf[y * row_bytes + x / 8] >> (7 - x % 8)) & 1;
                gray[y * w + x] = if bit == 1 { 255 } else { 0 };
            }
        }
        (w, h, gray)
    }

    /// 生成 → 渲染 → 识别 → 落库 → 丢 5% 分片 → 纠删恢复 → 续传读回
    #[test]
    fn end_to_end_frames_loss_and_resume() {
        use screencode::detect::{decode_frame, DecodeOptions};
        use screencode::fec;
        use screencode::header::Header;
        use screencode::layout::Geometry;
        use screencode::render::render_png;
        use screencode::PAD_SLOT;

        let block = 1024;
        let file = lcg_bytes(120_000, 0xABCD_EF01);
        let plan = fec::plan(file.len(), block, 0.10).expect("plan");
        let shards = fec::encode_shards(&file, &plan, block).expect("encode");
        let geo = Geometry::plan(800, 600, 2, block).expect("geometry");
        let header = Header::new(
            2,
            geo.grid_w,
            geo.grid_h,
            block,
            plan.k,
            plan.m,
            plan.total_shards,
            file.len() as u64,
            [5u8; 8],
        );

        let paths = temp_paths("e2e");
        let mut store = ShardStore::create(paths.clone()).expect("create");
        assert!(store.ensure(&header).expect("ensure"));

        let spf = geo.slots_per_frame();
        let frame_count = plan.total_shards.div_ceil(spf);
        let mut dropped = 0usize;

        for frame in 0..frame_count {
            let start = frame * spf;
            let end = (start + spf).min(plan.total_shards);
            let slots: Vec<Vec<u8>> = (start..end)
                .map(|i| encode_slot(i as u32, &shards[i]))
                .collect();
            let png = render_png(&geo, &header, &slots).expect("render");
            let (w, h, gray) = png_to_gray(&png);
            let decoded =
                decode_frame(&gray, w, h, None, &DecodeOptions::default()).expect("decode");
            assert_eq!(decoded.header, header);

            for rec in decoded.slots {
                if rec.idx == PAD_SLOT || rec.idx as usize >= plan.total_shards {
                    continue;
                }
                // 模拟 5% 的永久损坏：这些分片永远收不到
                if rec.idx as usize % 20 == 0 {
                    dropped += 1;
                    continue;
                }
                store
                    .ingest(rec.idx as usize, &rec.payload)
                    .expect("ingest");
            }
        }

        assert!(dropped > 0);
        let (ready, groups) = store.groups_ready();
        assert_eq!(ready, groups, "每组都应有 >= k 个分片");

        store.save_state().expect("save");
        store.close();

        // 续传读回
        let mut reopened = ShardStore::open(paths.clone())
            .expect("open")
            .expect("some");
        assert_eq!(
            reopened.received_count(),
            plan.total_shards - dropped,
            "位图应完整持久化"
        );

        let out = reopened.assemble().expect("assemble");
        assert_eq!(out.len(), file.len());
        assert_eq!(out, file);

        let _ = fs::remove_dir_all(paths.blocks.parent().unwrap());
    }
}
