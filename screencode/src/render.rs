//! 把分片渲染成一帧 1-bit PNG（黑=0，白=1）。
//!
//! 图像 = 白边 + 黑边框 + 帧头×3 + 数据区。像素与模块的对应关系完全由 `Geometry` 决定，
//! 生成端写位与识别端读位用的是同一份布局。

use anyhow::{bail, Result};

use crate::header::Header;
use crate::layout::Geometry;
use crate::{BORDER_MODULES, HEADER_H, HEADER_W, PAD_SLOT, SLOT_OVERHEAD};

/// 生成一帧 PNG。
///
/// `slots` 是这一帧要放的 slot 字节（每项长度必须是 `geo.slot_bytes()`）；
/// 不足 `slots_per_frame()` 的部分自动用填充 slot 补齐。
pub fn render_png(geo: &Geometry, header: &Header, slots: &[Vec<u8>]) -> Result<Vec<u8>> {
    let slot_bytes = geo.slot_bytes();
    let slots_per_frame = geo.slots_per_frame();

    if slots.len() > slots_per_frame {
        bail!(
            "一帧最多放 {} 个分片，传入 {} 个",
            slots_per_frame,
            slots.len()
        );
    }
    for slot in slots {
        if slot.len() != slot_bytes {
            bail!("slot 长度 {}，期望 {}", slot.len(), slot_bytes);
        }
    }

    // 数据位流：真实 slot + 填充 slot
    let mut data_stream: Vec<u8> = Vec::with_capacity(slots_per_frame * slot_bytes);
    for slot in slots {
        data_stream.extend_from_slice(slot);
    }
    let pad = crate::shard::encode_slot(PAD_SLOT, &vec![0u8; geo.block_bytes]);
    while data_stream.len() < slots_per_frame * slot_bytes {
        data_stream.extend_from_slice(&pad);
    }

    let header_bytes = header.to_bit_bytes();
    let row_start = data_row_starts(geo);

    let width = geo.frame_w as usize;
    let height = geo.frame_h as usize;
    let row_bytes = width.div_ceil(8);
    let mut pixels = vec![0xFFu8; row_bytes * height];

    let code_x = geo.code_x() as usize;
    let code_y = geo.code_y() as usize;
    let m = geo.module_px as usize;

    for py in 0..height {
        let Some(dy) = py.checked_sub(code_y) else {
            continue;
        };
        let my = dy / m;
        if my >= geo.grid_h as usize {
            continue;
        }
        let row_off = py * row_bytes;

        for mx in 0..geo.grid_w as usize {
            if !module_dark(geo, &header_bytes, &data_stream, &row_start, mx, my) {
                continue;
            }
            let px0 = code_x + mx * m;
            for px in px0..(px0 + m).min(width) {
                pixels[row_off + px / 8] &= !(0x80 >> (px % 8));
            }
        }
    }

    encode_png(&pixels, geo.frame_w, geo.frame_h, row_bytes)
}

/// 每一行数据区之前的累计数据位数（行优先）
pub fn data_row_starts(geo: &Geometry) -> Vec<usize> {
    let gw = geo.grid_w as usize;
    let gh = geo.grid_h as usize;
    let b = BORDER_MODULES as usize;
    let mut starts = vec![0usize; gh + 1];
    let mut acc = 0usize;
    for (y, start) in starts.iter_mut().enumerate().take(gh) {
        *start = acc;
        // 上下边框行没有数据位
        if y < b || y >= gh.saturating_sub(b) {
            continue;
        }
        let mut cells = gw.saturating_sub(2 * b);
        if y < b + HEADER_H as usize {
            cells = cells.saturating_sub(HEADER_W as usize);
        }
        acc += cells;
    }
    starts[gh] = acc;
    starts
}

#[inline]
fn data_index(row_start: &[usize], x: usize, y: usize) -> usize {
    let b = BORDER_MODULES as usize;
    let mut idx = row_start[y] + (x - b);
    if y >= b && y < b + HEADER_H as usize && x >= b + HEADER_W as usize {
        idx -= HEADER_W as usize;
    }
    idx
}

#[inline]
fn bit_at(bytes: &[u8], index: usize) -> bool {
    let byte = index / 8;
    if byte >= bytes.len() {
        return false;
    }
    (bytes[byte] >> (7 - (index % 8))) & 1 == 1
}

#[inline]
fn module_dark(
    geo: &Geometry,
    header_bytes: &[u8],
    data_stream: &[u8],
    row_start: &[usize],
    x: usize,
    y: usize,
) -> bool {
    use crate::BORDER_MODULES as B;
    let (x32, y32) = (x as u32, y as u32);
    if x32 < B || y32 < B || x32 >= geo.grid_w - B || y32 >= geo.grid_h - B {
        return true;
    }
    if geo.is_header(x32, y32) {
        let hx = x - B as usize;
        let hy = y - B as usize;
        return bit_at(header_bytes, hy * HEADER_W as usize + hx);
    }
    let idx = data_index(row_start, x, y);
    bit_at(data_stream, idx)
}

fn encode_png(pixels: &[u8], width: u32, height: u32, row_bytes: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::One);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header()?;
        let expected = row_bytes * height as usize;
        debug_assert_eq!(pixels.len(), expected);
        writer.write_image_data(pixels)?;
    }
    Ok(out)
}

/// 校验一帧的槽位数量是否与几何一致
pub fn check_slots(geo: &Geometry, slots: &[Vec<u8>]) -> Result<()> {
    if slots.len() > geo.slots_per_frame() {
        bail!(
            "一帧最多 {} 个分片，实际 {}",
            geo.slots_per_frame(),
            slots.len()
        );
    }
    for slot in slots {
        if slot.len() != geo.slot_bytes() {
            bail!(
                "slot 长度 {}，期望 {}",
                slot.len(),
                SLOT_OVERHEAD + geo.block_bytes
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::HeaderSpec;

    fn sample_header(geo: &Geometry) -> Header {
        Header::new(HeaderSpec {
            module_px: geo.module_px,
            grid_w: geo.grid_w,
            grid_h: geo.grid_h,
            block_bytes: geo.block_bytes,
            k: 10,
            m: 2,
            total_shards: 12,
            file_len: 40_000,
            hash_prefix: [9u8; 8],
        })
    }

    fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(bytes);
        let mut reader = decoder.read_info().expect("png info");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("png frame");
        (info.width, info.height, buf)
    }

    #[test]
    fn png_has_exact_size_and_bit_depth() {
        let geo = Geometry::plan(800, 600, 2, 256).expect("plan");
        let header = sample_header(&geo);
        let png = render_png(&geo, &header, &[]).expect("render");
        let (w, h, _) = decode_png(&png);
        assert_eq!((w, h), (800, 600));
    }

    #[test]
    fn border_and_margin_pixels() {
        let geo = Geometry::plan(800, 600, 2, 256).expect("plan");
        let header = sample_header(&geo);
        let png = render_png(&geo, &header, &[]).expect("render");
        let (w, _h, data) = decode_png(&png);
        let row_bytes = (w as usize).div_ceil(8);
        let bit = |x: usize, y: usize| -> u8 { (data[y * row_bytes + x / 8] >> (7 - x % 8)) & 1 };
        // 白边：左上角 (0,0) 是白
        assert_eq!(bit(0, 0), 1);
        // 黑边框：模块 (0,0) 在 code_x/code_y 处
        assert_eq!(bit(geo.code_x() as usize, geo.code_y() as usize), 0);
        // 模块 (1,3) 仍是边框（厚度 2）
        assert_eq!(
            bit(geo.code_x() as usize + 2, geo.code_y() as usize + 3 * 2),
            0
        );
        // 边框内侧数据区第一格不是整块黑（随机数据下大概率）
        assert_eq!(
            bit(geo.code_x() as usize + 2 * 2, geo.code_y() as usize + 3 * 2),
            1
        );
    }

    #[test]
    fn rejects_wrong_slot_length() {
        let geo = Geometry::plan(800, 600, 2, 256).expect("plan");
        let header = sample_header(&geo);
        assert!(render_png(&geo, &header, &[vec![0u8; 10]]).is_err());
    }

    #[test]
    fn row_starts_match_data_bits() {
        let geo = Geometry::plan(900, 700, 3, 128).expect("plan");
        let starts = data_row_starts(&geo);
        assert_eq!(starts[geo.grid_h as usize], geo.data_bits());
        // 边框行没有数据位
        assert_eq!(starts[2], 0);
        // 顶部 2 行边框之后，第 2 行是帧头行：少 64 个数据位
        assert_eq!(
            starts[(crate::BORDER_MODULES + 1) as usize] - starts[crate::BORDER_MODULES as usize],
            (geo.grid_w - 2 * BORDER_MODULES - crate::HEADER_W) as usize
        );
        // 帧头行之后的一行是满宽数据行
        let y = crate::BORDER_MODULES + crate::HEADER_H;
        assert_eq!(
            starts[(y + 1) as usize] - starts[y as usize],
            (geo.grid_w - 2 * BORDER_MODULES) as usize
        );
    }
}
