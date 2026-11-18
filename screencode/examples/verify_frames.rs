//! 离线校验：把 `client --export` 导出的 frame_*.png 逐帧识别并还原文件。
//!
//! 用途：
//! - 不依赖游戏窗口，先确认「生成 → 识别 → 纠删恢复」链路是通的；
//! - 排查某帧/某分片为什么收不到时，直接看每帧的 rect、模块尺寸与 CRC 失败数。
//!
//! 用法：
//!   cargo run -p screencode --example verify_frames -- <目录> [--out 输出文件]

use std::env;
use std::fs;
use std::path::PathBuf;

use screencode::detect::{decode_frame, DecodeOptions};
use screencode::fec;
use screencode::PAD_SLOT;
use sha2::{Digest, Sha256};

fn png_to_gray(png: &[u8]) -> (usize, usize, Vec<u8>) {
    let decoder = png::Decoder::new(png);
    let mut reader = decoder.read_info().expect("解析 PNG 头失败");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("读取 PNG 像素失败");
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

fn main() {
    let args: Vec<String> = env::args().collect();
    let dir = args.get(1).map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("用法: verify_frames <frame 目录> [--out 输出文件]");
        std::process::exit(2);
    });
    let out_path = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读取目录 {} 失败: {}", dir.display(), e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("png")
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.starts_with("frame_"))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    if files.is_empty() {
        panic!("{} 下没有 frame_*.png", dir.display());
    }

    let mut header = None;
    let mut shards: Vec<Option<Vec<u8>>> = Vec::new();
    let mut crc_total = 0usize;
    let mut slots_total = 0usize;

    for (index, path) in files.iter().enumerate() {
        let png = fs::read(path).unwrap_or_else(|e| panic!("读 {} 失败: {}", path.display(), e));
        let (w, h, gray) = png_to_gray(&png);
        let decoded = match decode_frame(&gray, w, h, header.as_ref(), &DecodeOptions::default()) {
            Ok(decoded) => decoded,
            Err(error) => {
                println!(
                    "帧 #{:05} {}: 识别失败: {}",
                    index + 1,
                    path.file_name().unwrap().to_string_lossy(),
                    error
                );
                continue;
            }
        };
        if header.is_none() {
            header = Some(decoded.header.clone());
            shards = vec![None; decoded.header.total_shards as usize];
            let h = &decoded.header;
            println!(
                "帧头: {} 字节 | 分片 {} | k={} m={} | 码 {}x{} 模块 | 实测模块 {:.3}px",
                h.file_len, h.total_shards, h.k, h.m, h.grid_w, h.grid_h, decoded.module_x
            );
        }

        let total = shards.len();
        let mut new_here = 0usize;
        for rec in &decoded.slots {
            slots_total += 1;
            if rec.idx == PAD_SLOT || rec.idx as usize >= total {
                continue;
            }
            let idx = rec.idx as usize;
            if shards[idx].is_none() {
                shards[idx] = Some(rec.payload.clone());
                new_here += 1;
            }
        }
        crc_total += decoded.crc_failures;
        println!(
            "帧 #{:05} | rect=({},{},{},{}) | 槽 {}/{}（CRC 失败 {}，新增 {}）| 累计 {}/{}",
            index + 1,
            decoded.rect.x,
            decoded.rect.y,
            decoded.rect.w,
            decoded.rect.h,
            decoded.slots.len(),
            decoded.slot_count,
            decoded.crc_failures,
            new_here,
            shards.iter().filter(|s| s.is_some()).count(),
            total
        );
    }

    let Some(header) = header else {
        panic!("所有帧都没识别出来");
    };
    let got = shards.iter().filter(|s| s.is_some()).count();
    println!(
        "\n合计: {} 帧 | {} 个槽（CRC 失败 {}）| 收到分片 {}/{}",
        files.len(),
        slots_total,
        crc_total,
        got,
        shards.len()
    );

    match fec::assemble(&shards, &header) {
        Ok(bytes) => {
            let digest = Sha256::digest(&bytes);
            let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
            let prefix_ok = digest[..8] == header.hash_prefix;
            println!(
                "✓ 还原成功: {} 字节 | SHA-256 {} | 前缀校验 {}",
                bytes.len(),
                hex,
                if prefix_ok { "通过" } else { "不匹配" }
            );
            if let Some(path) = out_path {
                fs::write(&path, &bytes).expect("写输出失败");
                println!("已写出 {}", path.display());
            }
        }
        Err(error) => {
            println!("✗ 还原失败: {}", error);
            std::process::exit(1);
        }
    }
}
