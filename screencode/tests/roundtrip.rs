//! 端到端往返：文件 → 分片 → 逐帧 PNG → 模拟抓屏 → 识别 → 纠删恢复 → 原文件。

use screencode::detect::{decode_frame, DecodeOptions};
use screencode::fec;
use screencode::header::Header;
use screencode::layout::Geometry;
use screencode::render::render_png;
use screencode::shard::parse_slot;

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

#[test]
fn full_roundtrip_with_erasure_recovery() {
    let block = 1024;
    let file = lcg_bytes(400_000, 0xDEAD_BEEF);
    let plan = fec::plan(file.len(), block, 0.10).expect("plan");
    let shards = fec::encode_shards(&file, &plan, block).expect("encode");

    let geo = Geometry::plan(800, 600, 2, block).expect("geometry");
    let header = Header::new(
        geo.module_px,
        geo.grid_w,
        geo.grid_h,
        geo.block_bytes,
        plan.k,
        plan.m,
        plan.total_shards,
        file.len() as u64,
        [7u8; 8],
    );

    // 模拟接收端：每个 slot 自描述，所以按 idx 落库即可
    let mut store: Vec<Option<Vec<u8>>> = vec![None; plan.total_shards];
    let mut frames = 0usize;
    let mut slot_seen = 0usize;

    for frame in 0..plan.total_shards.div_ceil(geo.slots_per_frame()) {
        let start = frame * geo.slots_per_frame();
        let end = (start + geo.slots_per_frame()).min(plan.total_shards);
        let frame_slots: Vec<Vec<u8>> = shards[start..end]
            .iter()
            .enumerate()
            .map(|(i, payload)| screencode::shard::encode_slot((start + i) as u32, payload))
            .collect();

        let png = render_png(&geo, &header, &frame_slots).expect("render");
        let (w, h, gray) = png_to_gray(&png);
        let decoded = decode_frame(&gray, w, h, None, &DecodeOptions::default()).expect("decode");
        assert_eq!(decoded.header, header);
        frames += 1;

        for rec in decoded.slots {
            if rec.idx as usize >= plan.total_shards {
                continue;
            }
            let idx = rec.idx as usize;
            if store[idx].is_none() {
                slot_seen += 1;
                store[idx] = Some(rec.payload);
            }
        }
    }

    assert_eq!(frames, plan.total_shards.div_ceil(geo.slots_per_frame()));
    assert_eq!(slot_seen, plan.total_shards, "一帧都不该漏");

    // 人为丢掉 5% 的分片，验证纠删恢复
    for (i, slot) in store.iter_mut().enumerate() {
        if i % 20 == 0 {
            *slot = None;
        }
    }

    let out = fec::assemble(&store, &header).expect("assemble");
    assert_eq!(out.len(), file.len());
    assert_eq!(out, file);
}

#[test]
fn torn_frame_mixes_two_frames_safely() {
    let block = 512;
    let file = lcg_bytes(60_000, 42);
    let plan = fec::plan(file.len(), block, 0.10).expect("plan");
    let shards = fec::encode_shards(&file, &plan, block).expect("encode");
    let geo = Geometry::plan(640, 480, 2, block).expect("geometry");
    let header = Header::new(
        2,
        geo.grid_w,
        geo.grid_h,
        block,
        plan.k,
        plan.m,
        plan.total_shards,
        file.len() as u64,
        [3u8; 8],
    );

    let mk = |frame: usize| -> Vec<Vec<u8>> {
        let start = frame * geo.slots_per_frame();
        let end = (start + geo.slots_per_frame()).min(plan.total_shards);
        shards[start..end]
            .iter()
            .enumerate()
            .map(|(i, p)| screencode::shard::encode_slot((start + i) as u32, p))
            .collect()
    };

    let png_a = render_png(&geo, &header, &mk(0)).expect("render a");
    let png_b = render_png(&geo, &header, &mk(1)).expect("render b");
    let (w, h, mut gray_a) = png_to_gray(&png_a);
    let (_, _, gray_b) = png_to_gray(&png_b);

    // 下半帧来自另一帧：槽位自描述，识别端只接受 CRC 通过的 slot
    let split = h / 2;
    gray_a[split * w..].copy_from_slice(&gray_b[split * w..]);

    let decoded =
        decode_frame(&gray_a, w, h, None, &DecodeOptions::default()).expect("decode torn");
    assert!(!decoded.slots.is_empty());
    for rec in &decoded.slots {
        let idx = rec.idx as usize;
        assert!(idx < plan.total_shards);
        // 每个 slot 的 payload 必须与它自己的 idx 对得上
        assert_eq!(
            parse_slot(
                &screencode::shard::encode_slot(rec.idx, &rec.payload),
                block
            )
            .unwrap()
            .idx,
            rec.idx
        );
    }
}

#[test]
fn offset_capture_still_decodes() {
    let block = 512;
    let file = lcg_bytes(30_000, 7);
    let plan = fec::plan(file.len(), block, 0.0).expect("plan");
    let shards = fec::encode_shards(&file, &plan, block).expect("encode");
    let geo = Geometry::plan(640, 480, 2, block).expect("geometry");
    let header = Header::new(
        2,
        geo.grid_w,
        geo.grid_h,
        block,
        plan.k,
        plan.m,
        plan.total_shards,
        file.len() as u64,
        [0u8; 8],
    );
    let frame_slots: Vec<Vec<u8>> = shards[..plan.total_shards.min(geo.slots_per_frame())]
        .iter()
        .enumerate()
        .map(|(i, p)| screencode::shard::encode_slot(i as u32, p))
        .collect();
    let png = render_png(&geo, &header, &frame_slots).expect("render");
    let (w, h, gray) = png_to_gray(&png);

    // 整体右移 11px、下移 9px
    let (dx, dy) = (11usize, 9usize);
    let mut shifted = vec![255u8; (w + 40) * (h + 40)];
    for y in 0..h {
        let src = &gray[y * w..(y + 1) * w];
        let dst = (y + dy) * (w + 40) + dx;
        shifted[dst..dst + w].copy_from_slice(src);
    }

    let decoded = decode_frame(&shifted, w + 40, h + 40, None, &DecodeOptions::default())
        .expect("decode offset");
    assert_eq!(decoded.header, header);
    assert_eq!(decoded.slots.len(), frame_slots.len());
    assert_eq!(decoded.crc_failures, 0);
}

/// 双线性缩放：模拟「页面/窗口不是 1:1，被 GPU 缩放」时抓到的画面
fn resize_bilinear(gray: &[u8], w: usize, h: usize, nw: usize, nh: usize) -> Vec<u8> {
    let mut out = vec![0u8; nw * nh];
    let sx = w as f64 / nw as f64;
    let sy = h as f64 / nh as f64;
    for y in 0..nh {
        let fy = ((y as f64 + 0.5) * sy - 0.5).clamp(0.0, (h - 1) as f64);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(h - 1);
        let wy = fy - y0 as f64;
        for x in 0..nw {
            let fx = ((x as f64 + 0.5) * sx - 0.5).clamp(0.0, (w - 1) as f64);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(w - 1);
            let wx = fx - x0 as f64;
            let p00 = gray[y0 * w + x0] as f64;
            let p10 = gray[y0 * w + x1] as f64;
            let p01 = gray[y1 * w + x0] as f64;
            let p11 = gray[y1 * w + x1] as f64;
            let top = p00 * (1.0 - wx) + p10 * wx;
            let bottom = p01 * (1.0 - wx) + p11 * wx;
            out[y * nw + x] = (top * (1.0 - wy) + bottom * wy).round() as u8;
        }
    }
    out
}

#[test]
fn scaled_capture_still_decodes() {
    let block = 1024;
    let file = lcg_bytes(90_000, 99);
    let plan = fec::plan(file.len(), block, 0.0).expect("plan");
    let shards = fec::encode_shards(&file, &plan, block).expect("encode");
    let geo = Geometry::plan(640, 480, 2, block).expect("geometry");
    let header = Header::new(
        2,
        geo.grid_w,
        geo.grid_h,
        block,
        plan.k,
        plan.m,
        plan.total_shards,
        file.len() as u64,
        [0u8; 8],
    );
    let frame_slots: Vec<Vec<u8>> = shards[..plan.total_shards.min(geo.slots_per_frame())]
        .iter()
        .enumerate()
        .map(|(i, p)| screencode::shard::encode_slot(i as u32, p))
        .collect();
    let png = render_png(&geo, &header, &frame_slots).expect("render");
    let (w, h, gray) = png_to_gray(&png);

    for scale in [1.0f64, 1.25, 1.5] {
        let nw = (w as f64 * scale).round() as usize;
        let nh = (h as f64 * scale).round() as usize;
        let scaled = resize_bilinear(&gray, w, h, nw, nh);
        let decoded = decode_frame(&scaled, nw, nh, None, &DecodeOptions::default())
            .unwrap_or_else(|e| panic!("scale {} 识别失败: {}", scale, e));
        assert_eq!(decoded.header, header, "scale {}", scale);
        assert!(
            (decoded.module_x - 2.0 * scale).abs() < 0.08,
            "scale {} 实测模块 {:.3}",
            scale,
            decoded.module_x
        );
        assert_eq!(
            decoded.slots.len(),
            frame_slots.len(),
            "scale {} 槽数不对",
            scale
        );
        assert_eq!(decoded.crc_failures, 0, "scale {} 出现 CRC 失败", scale);
    }
}
