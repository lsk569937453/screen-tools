//! 识别：从一张灰度抓屏里定位码框、读帧头、按模块采样出 slot。
//!
//! 不依赖任何定位图案分组：码是固定网格，找到外框与模块尺寸后直接按坐标采样。
//! 任何被遮挡/缩放/撕裂影响的模块只影响自己所属的 slot，由上层 CRC + 纠删码兜底。

use crate::header::Header;
use crate::shard::{parse_slot, SlotRecord};
use crate::{BORDER_MODULES, HEADER_H, HEADER_W};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Default)]
pub struct DecodeOptions {
    /// 手工指定码框（自动定位失败时用）
    pub rect: Option<Rect>,
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub header: Header,
    pub rect: Rect,
    /// 实测模块像素（含缩放；1:1 时接近生成值）
    pub module_x: f64,
    pub module_y: f64,
    pub slots: Vec<SlotRecord>,
    pub slot_count: usize,
    pub crc_failures: usize,
}

#[derive(Debug, Clone)]
pub enum DetectError {
    Empty,
    NoRect,
    NoHeader,
    BadGeometry { module_x: f64, module_y: f64 },
}

impl std::fmt::Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectError::Empty => write!(f, "画面为空"),
            DetectError::NoRect => write!(f, "没有找到码框（黑边框）"),
            DetectError::NoHeader => write!(f, "码框找到了，但 3 份帧头都校验失败"),
            DetectError::BadGeometry { module_x, module_y } => {
                write!(f, "模块尺寸异常: {:.3} x {:.3}", module_x, module_y)
            }
        }
    }
}

impl std::error::Error for DetectError {}

/// RGBA -> 灰度
pub fn to_gray(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut gray = vec![0u8; width * height];
    let (pixels, _) = rgba.as_chunks::<4>();
    for (out, p) in gray.iter_mut().zip(pixels) {
        // 0.299R + 0.587G + 0.114B，整数近似
        *out = ((299 * p[0] as u32 + 587 * p[1] as u32 + 114 * p[2] as u32) / 1000) as u8;
    }
    gray
}

/// 识别一帧。
pub fn decode_frame(
    gray: &[u8],
    width: usize,
    height: usize,
    hint: Option<&Header>,
    options: &DecodeOptions,
) -> Result<DecodedFrame, DetectError> {
    if gray.len() != width * height || width < 32 || height < 32 {
        return Err(DetectError::Empty);
    }

    let threshold = otsu(gray);

    let (rect, thickness) = match options.rect {
        Some(rect) => (clamp_rect(rect, width, height), 0.0),
        None => find_rect(gray, width, height, threshold).ok_or(DetectError::NoRect)?,
    };

    let m0 = if thickness > 0.5 {
        thickness
    } else {
        // 手工给了 rect：先用帧头自带的生成值猜一个，再用 rect/grid 校正
        hint.map(|h| h.module_px as f64).unwrap_or(2.0)
    };

    let header = read_header(gray, width, height, threshold, rect, m0)
        .or_else(|| hint.cloned())
        .ok_or(DetectError::NoHeader)?;

    let module_x = rect.w as f64 / header.grid_w as f64;
    let module_y = rect.h as f64 / header.grid_h as f64;

    if !valid_module(module_x, module_y) {
        return Err(DetectError::BadGeometry { module_x, module_y });
    }

    let (slots, slot_count, crc_failures) = sample_slots(
        gray, width, height, threshold, rect, &header, module_x, module_y,
    );

    Ok(DecodedFrame {
        header,
        rect,
        module_x,
        module_y,
        slots,
        slot_count,
        crc_failures,
    })
}

fn valid_module(mx: f64, my: f64) -> bool {
    if !(mx.is_finite() && my.is_finite()) || mx < 0.8 || my < 0.8 || mx > 64.0 || my > 64.0 {
        return false;
    }
    let diff = (mx - my).abs() / mx.max(my);
    diff <= 0.02
}

fn clamp_rect(rect: Rect, width: usize, height: usize) -> Rect {
    let x = rect.x.min(width as u32 - 1);
    let y = rect.y.min(height as u32 - 1);
    let w = rect.w.min(width as u32 - x);
    let h = rect.h.min(height as u32 - y);
    Rect { x, y, w, h }
}

/// Otsu 阈值（纯黑白通道下等价于找直方图谷底）
pub fn otsu(gray: &[u8]) -> u8 {
    let mut hist = [0u32; 256];
    let mut total = 0u64;
    for &v in gray.iter().step_by(2) {
        hist[v as usize] += 1;
        total += 1;
    }
    if total == 0 {
        return 128;
    }

    let sum: u64 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| i as u64 * c as u64)
        .sum();

    let mut sum_b = 0u64;
    let mut w_b = 0u64;
    let mut best_t = 128u8;
    let mut best_var = -1.0f64;

    for t in 0..256 {
        w_b += hist[t] as u64;
        if w_b == 0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0 {
            break;
        }
        sum_b += t as u64 * hist[t] as u64;
        let m_b = sum_b as f64 / w_b as f64;
        let m_f = (sum - sum_b) as f64 / w_f as f64;
        let var = w_b as f64 * w_f as f64 * (m_b - m_f).powi(2);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    best_t
}

/// 找黑边框：逐行/逐列数黑点，取占比接近最大的连续段作为四条边。
fn find_rect(gray: &[u8], width: usize, height: usize, threshold: u8) -> Option<(Rect, f64)> {
    let is_dark = |v: u8| v <= threshold;

    let mut row_counts = vec![0u32; height];
    let mut col_counts = vec![0u32; width];
    for y in 0..height {
        let row = &gray[y * width..(y + 1) * width];
        let mut c = 0u32;
        for (x, &v) in row.iter().enumerate() {
            if is_dark(v) {
                c += 1;
                col_counts[x] += 1;
            }
        }
        row_counts[y] = c;
    }

    let max_row = *row_counts.iter().max()?;
    let max_col = *col_counts.iter().max()?;
    if max_row < 32 || max_col < 32 {
        return None;
    }

    let row_groups = groups_above(&row_counts, (max_row as f64 * 0.75) as u32);
    let col_groups = groups_above(&col_counts, (max_col as f64 * 0.75) as u32);
    if row_groups.len() < 2 || col_groups.len() < 2 {
        return None;
    }

    let mut best: Option<(Rect, f64)> = None;
    let mut best_score = f64::MAX;

    for top in row_groups.iter() {
        for bottom in row_groups.iter() {
            if bottom.0 <= top.1 {
                continue;
            }
            for left in col_groups.iter() {
                for right in col_groups.iter() {
                    if right.0 <= left.1 {
                        continue;
                    }

                    let th = [
                        (top.1 - top.0) as f64,
                        (bottom.1 - bottom.0) as f64,
                        (left.1 - left.0) as f64,
                        (right.1 - right.0) as f64,
                    ];
                    let min = th.iter().cloned().fold(f64::MAX, f64::min);
                    let max = th.iter().cloned().fold(0.0f64, f64::max);
                    if min < 1.0 || max / min > 1.4 {
                        continue;
                    }

                    let rect = Rect {
                        x: left.0 as u32,
                        y: top.0 as u32,
                        w: (right.1 - left.0) as u32,
                        h: (bottom.1 - top.0) as u32,
                    };
                    if rect.w < 64 || rect.h < 64 {
                        continue;
                    }

                    // 内部必须是「明暗混合」，全黑/全白都不是码；尾部帧可能几乎全是填充，比例会很白
                    let inner_dark = interior_dark_ratio(gray, width, height, rect, threshold);
                    if !(0.0005..=0.9995).contains(&inner_dark) {
                        continue;
                    }

                    // 四条边厚度应一致；用变异系数打分，取最一致的一组
                    let mean = th.iter().sum::<f64>() / 4.0;
                    let score = th.iter().map(|v| (v - mean).abs()).sum::<f64>() / mean;
                    if score < best_score {
                        best_score = score;
                        best = Some((rect, mean / 2.0));
                    }
                }
            }
        }
    }

    best
}

fn groups_above(counts: &[u32], threshold: u32) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &c) in counts.iter().enumerate() {
        if c >= threshold {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            groups.push((s, i));
        }
    }
    if let Some(s) = start {
        groups.push((s, counts.len()));
    }
    groups
}

fn interior_dark_ratio(gray: &[u8], width: usize, height: usize, rect: Rect, threshold: u8) -> f64 {
    let _ = height;
    let x0 = rect.x as usize;
    let y0 = rect.y as usize;
    let x1 = (rect.x + rect.w) as usize;
    let y1 = (rect.y + rect.h) as usize;
    let mut dark = 0u32;
    let mut total = 0u32;
    for y in (y0..y1).step_by(4) {
        let row = &gray[y * width..(y + 1) * width];
        for x in (x0..x1).step_by(4) {
            if row[x] <= threshold {
                dark += 1;
            }
            total += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        dark as f64 / total as f64
    }
}

/// 读 3 份帧头中的任意一份；对模块尺寸/偏移做少量搜索，容忍 1px 级误差。
fn read_header(
    gray: &[u8],
    width: usize,
    height: usize,
    threshold: u8,
    rect: Rect,
    m0: f64,
) -> Option<Header> {
    let base_x = rect.x as f64;
    let base_y = rect.y as f64;

    let scale_candidates = [1.0, 1.01, 0.99, 1.02, 0.98];
    let offset_candidates = [0.0, -1.0, 1.0, -2.0, 2.0];

    for scale in scale_candidates {
        let m = m0 * scale;
        if m < 0.9 {
            continue;
        }
        for dx in offset_candidates {
            for dy in offset_candidates {
                let ox = base_x + BORDER_MODULES as f64 * m + dx;
                let oy = base_y + BORDER_MODULES as f64 * m + dy;

                let mut bits = Vec::with_capacity((HEADER_W * HEADER_H / 8) as usize);
                let mut acc = 0u8;
                let mut nbits = 0;
                for my in 0..HEADER_H {
                    for mx in 0..HEADER_W {
                        let cx = ox + (mx as f64 + 0.5) * m;
                        let cy = oy + (my as f64 + 0.5) * m;
                        let dark = sample_module(gray, width, height, cx, cy, m, threshold);
                        acc = (acc << 1) | dark as u8;
                        nbits += 1;
                        if nbits == 8 {
                            bits.push(acc);
                            acc = 0;
                            nbits = 0;
                        }
                    }
                }
                if let Some(header) = Header::from_bit_bytes(&bits) {
                    return Some(header);
                }
            }
        }
    }
    None
}

/// 采样一个模块：中心 + 上下左右 0.3 模块，多数表决。
#[inline]
fn sample_module(
    gray: &[u8],
    width: usize,
    height: usize,
    cx: f64,
    cy: f64,
    m: f64,
    threshold: u8,
) -> bool {
    let d = (m * 0.3).max(0.35);
    let points = [
        (cx, cy),
        (cx - d, cy),
        (cx + d, cy),
        (cx, cy - d),
        (cx, cy + d),
    ];
    let mut dark = 0u8;
    for (x, y) in points {
        let xi = x.floor() as isize;
        let yi = y.floor() as isize;
        if xi < 0 || yi < 0 || xi as usize >= width || yi as usize >= height {
            continue;
        }
        if gray[yi as usize * width + xi as usize] <= threshold {
            dark += 1;
        }
    }
    dark >= 3
}

/// 按固定 slot 长度采样数据区
fn sample_slots(
    gray: &[u8],
    width: usize,
    height: usize,
    threshold: u8,
    rect: Rect,
    header: &Header,
    module_x: f64,
    module_y: f64,
) -> (Vec<SlotRecord>, usize, usize) {
    let slot_bytes = header.slot_bytes();
    let slot_bits = slot_bytes * 8;
    let grid_w = header.grid_w as usize;
    let grid_h = header.grid_h as usize;

    let data_cells = (grid_w - 2 * BORDER_MODULES as usize)
        * (grid_h - 2 * BORDER_MODULES as usize)
        - (HEADER_W * HEADER_H) as usize;
    let slot_count = data_cells / slot_bits;

    let target_bytes = slot_count * slot_bytes;
    let mut stream: Vec<u8> = Vec::with_capacity(target_bytes);
    let mut acc = 0u8;
    let mut nbits = 0u8;

    let b = BORDER_MODULES as usize;
    let hx0 = b;
    let hx1 = b + HEADER_W as usize;
    let hy0 = b;
    let hy1 = b + HEADER_H as usize;

    let ox = rect.x as f64;
    let oy = rect.y as f64;

    'outer: for y in b..grid_h - b {
        let cy = oy + (y as f64 + 0.5) * module_y;
        for x in b..grid_w - b {
            if y >= hy0 && y < hy1 && x >= hx0 && x < hx1 {
                continue;
            }
            let cx = ox + (x as f64 + 0.5) * module_x;
            let dark = sample_module(
                gray,
                width,
                height,
                cx,
                cy,
                module_x.min(module_y),
                threshold,
            );
            acc = (acc << 1) | dark as u8;
            nbits += 1;
            if nbits == 8 {
                stream.push(acc);
                acc = 0;
                nbits = 0;
                if stream.len() >= target_bytes {
                    break 'outer;
                }
            }
        }
    }
    stream.truncate(target_bytes);

    let mut slots = Vec::with_capacity(slot_count);
    let mut crc_failures = 0usize;
    for i in 0..slot_count {
        let start = i * slot_bytes;
        match parse_slot(
            &stream[start..start + slot_bytes],
            header.block_bytes as usize,
        ) {
            Some(rec) => slots.push(rec),
            None => crc_failures += 1,
        }
    }

    (slots, slot_count, crc_failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::Header;
    use crate::layout::Geometry;
    use crate::render::render_png;

    fn make_header(geo: &Geometry) -> Header {
        Header::new(
            geo.module_px,
            geo.grid_w,
            geo.grid_h,
            geo.block_bytes,
            10,
            2,
            12,
            40_000,
            [1, 2, 3, 4, 5, 6, 7, 8],
        )
    }

    /// 把 PNG 解码成灰度（测试用，避免依赖 image crate）
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
    fn otsu_separates_black_and_white() {
        let mut gray = vec![0u8; 1000];
        gray[..500].fill(10);
        gray[500..].fill(240);
        let t = otsu(&gray);
        assert!(t >= 10 && t <= 240, "threshold={}", t);
    }

    #[test]
    fn detects_rect_and_header() {
        let geo = Geometry::plan(640, 480, 2, 256).expect("plan");
        let header = make_header(&geo);
        let slot = crate::shard::encode_slot(0, &vec![0xAB; geo.block_bytes]);
        let png = render_png(&geo, &header, &[slot]).expect("render");
        let (w, h, gray) = png_to_gray(&png);

        let decoded = decode_frame(&gray, w, h, None, &DecodeOptions::default()).expect("decode");
        assert_eq!(decoded.header, header);
        assert_eq!(
            decoded.rect,
            Rect {
                x: geo.code_x(),
                y: geo.code_y(),
                w: geo.code_w_px(),
                h: geo.code_h_px()
            }
        );
        assert!((decoded.module_x - 2.0).abs() < 0.01);
    }

    #[test]
    fn tolerates_offset() {
        let geo = Geometry::plan(640, 480, 2, 256).expect("plan");
        let header = make_header(&geo);
        let png = render_png(&geo, &header, &[]).expect("render");
        let (w, h, gray) = png_to_gray(&png);

        // 把码整体下移 7px、右移 5px，周围填白
        let pad = 7;
        let mut shifted = vec![255u8; w * (h + 32)];
        for y in 0..h {
            let dst = (y + pad) * w + 5;
            shifted[dst..dst + w - 5].copy_from_slice(&gray[y * w..y * w + w - 5]);
        }
        let decoded = decode_frame(&shifted, w, h + 32, None, &DecodeOptions::default())
            .expect("decode shifted");
        assert_eq!(decoded.header, header);
        assert!(decoded.rect.y >= 7);
    }

    #[test]
    fn hint_header_used_when_header_damaged() {
        let geo = Geometry::plan(640, 480, 2, 256).expect("plan");
        let header = make_header(&geo);
        let png = render_png(&geo, &header, &[]).expect("render");
        let (w, h, mut gray) = png_to_gray(&png);

        // 把帧头区域涂成全黑，3 份都坏掉
        let b = BORDER_MODULES as usize;
        for my in 0..HEADER_H as usize {
            for mx in 0..HEADER_W as usize {
                let x0 = geo.code_x() as usize + (b + mx) * 2;
                let y0 = geo.code_y() as usize + (b + my) * 2;
                for dy in 0..2 {
                    for dx in 0..2 {
                        gray[(y0 + dy) * w + x0 + dx] = 0;
                    }
                }
            }
        }

        assert!(decode_frame(&gray, w, h, None, &DecodeOptions::default()).is_err());
        let decoded = decode_frame(&gray, w, h, Some(&header), &DecodeOptions::default())
            .expect("decode with hint");
        assert_eq!(decoded.header, header);
    }
}
