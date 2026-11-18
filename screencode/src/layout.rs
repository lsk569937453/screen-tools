//! 版式几何：给定帧尺寸、模块像素，算出整个码的模块网格与数据区。
//!
//! 生成端与识别端必须用**同一份** `Geometry` 计算，任何一侧改动都会破坏兼容性。

use anyhow::{bail, Result};

use crate::{
    BORDER_MODULES, DEFAULT_BLOCK_BYTES, HEADER_H, HEADER_W, MARGIN_MODULES, SLOT_OVERHEAD,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// 整张 PNG 的像素尺寸
    pub frame_w: u32,
    pub frame_h: u32,
    /// 每个模块占多少像素（生成值；识别端实测值可能不同）
    pub module_px: u32,
    /// 整个码（含黑边框）的模块列数/行数
    pub grid_w: u32,
    pub grid_h: u32,
    /// 每个分片的净荷字节数
    pub block_bytes: usize,
}

impl Geometry {
    /// 规划一帧的版式。
    pub fn plan(frame_w: u32, frame_h: u32, module_px: u32, block_bytes: usize) -> Result<Self> {
        if module_px == 0 {
            bail!("module_px 必须 >= 1");
        }
        if block_bytes == 0 || block_bytes > u16::MAX as usize {
            bail!("block_bytes 必须在 1..={} 之间", u16::MAX);
        }

        let margin_px = MARGIN_MODULES * module_px;
        if frame_w <= margin_px * 2 + module_px || frame_h <= margin_px * 2 + module_px {
            bail!(
                "帧尺寸 {}x{} 太小，放不下 {}px 白边 + {}px 模块",
                frame_w,
                frame_h,
                margin_px,
                module_px
            );
        }

        let grid_w = (frame_w - margin_px * 2) / module_px;
        let grid_h = (frame_h - margin_px * 2) / module_px;

        let min_w = BORDER_MODULES * 2 + HEADER_W;
        let min_h = BORDER_MODULES * 2 + HEADER_H;
        if grid_w < min_w || grid_h < min_h {
            bail!(
                "模块网格 {}x{} 太小，至少需要 {}x{}（{}px 模块）",
                grid_w,
                grid_h,
                min_w,
                min_h,
                module_px
            );
        }

        let geometry = Self {
            frame_w,
            frame_h,
            module_px,
            grid_w,
            grid_h,
            block_bytes,
        };

        let slot_bytes = geometry.slot_bytes();
        if geometry.data_bits() < slot_bytes * 8 {
            bail!(
                "数据区只有 {} 位，放不下一个 {}+{} 字节的分片；请减小 --block-bytes 或增大帧尺寸",
                geometry.data_bits(),
                block_bytes,
                SLOT_OVERHEAD
            );
        }

        Ok(geometry)
    }

    /// 默认 block_bytes 的规划
    pub fn plan_default(frame_w: u32, frame_h: u32, module_px: u32) -> Result<Self> {
        Self::plan(frame_w, frame_h, module_px, DEFAULT_BLOCK_BYTES)
    }

    pub fn code_x(&self) -> u32 {
        MARGIN_MODULES * self.module_px
    }

    pub fn code_y(&self) -> u32 {
        MARGIN_MODULES * self.module_px
    }

    pub fn code_w_px(&self) -> u32 {
        self.grid_w * self.module_px
    }

    pub fn code_h_px(&self) -> u32 {
        self.grid_h * self.module_px
    }

    /// 黑边框（模块坐标）
    pub fn is_border(&self, x: u32, y: u32) -> bool {
        x < BORDER_MODULES
            || y < BORDER_MODULES
            || x >= self.grid_w - BORDER_MODULES
            || y >= self.grid_h - BORDER_MODULES
    }

    /// 帧头区域（模块坐标）
    pub fn is_header(&self, x: u32, y: u32) -> bool {
        let bx = BORDER_MODULES;
        let by = BORDER_MODULES;
        x >= bx && x < bx + HEADER_W && y >= by && y < by + HEADER_H
    }

    /// 数据区（模块坐标）
    pub fn is_data(&self, x: u32, y: u32) -> bool {
        !self.is_border(x, y) && !self.is_header(x, y)
    }

    /// 数据区模块总数（位）
    pub fn data_bits(&self) -> usize {
        (self.grid_w - BORDER_MODULES * 2) as usize * (self.grid_h - BORDER_MODULES * 2) as usize
            - (HEADER_W * HEADER_H) as usize
    }

    pub fn slot_bytes(&self) -> usize {
        self.block_bytes + SLOT_OVERHEAD
    }

    pub fn slots_per_frame(&self) -> usize {
        self.data_bits() / (self.slot_bytes() * 8)
    }

    /// 数据区字节容量（不含尾部不足一字节的位）
    pub fn data_bytes(&self) -> usize {
        self.data_bits() / 8
    }

    /// 按行优先遍历数据区模块（生成端写位、识别端读位都用它）
    pub fn data_cells(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let gw = self.grid_w;
        let gh = self.grid_h;
        let b = BORDER_MODULES;
        (b..gh - b)
            .flat_map(move |y| (b..gw - b).map(move |x| (x, y)))
            .filter(move |&(x, y)| !self.is_header(x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_matches_reference_layout() {
        let g = Geometry::plan(2505, 1329, 2, 4096).expect("plan");
        // 2505/2 = 1252，去掉两侧 4 模块白边 = 1244
        assert_eq!(g.grid_w, 1244);
        assert_eq!(g.grid_h, 656);
        assert_eq!(g.code_x(), 8);
        assert_eq!(g.code_w_px(), 2488);
        assert_eq!(g.data_bits(), 1240 * 652 - 64 * 16);
        assert_eq!(g.slot_bytes(), 4104);
        assert_eq!(g.slots_per_frame(), 24);
    }

    #[test]
    fn capacity_table_for_2505x1329() {
        // 与 docs/screencode-v1.md、README 的容量表保持一致
        for (m, gw, gh, slots) in [
            (1u32, 2497u32, 1321u32, 99usize),
            (2, 1244, 656, 24),
            (3, 827, 435, 10),
        ] {
            let g = Geometry::plan(2505, 1329, m, 4096).expect("plan");
            assert_eq!((g.grid_w, g.grid_h), (gw, gh), "m={}", m);
            assert_eq!(g.slots_per_frame(), slots, "m={}", m);
            assert_eq!(
                g.slots_per_frame() * (4096 + SLOT_OVERHEAD),
                slots * 4104,
                "m={}",
                m
            );
        }
    }

    #[test]
    fn plan_rejects_too_small_frames() {
        assert!(Geometry::plan(200, 200, 2, 4096).is_err());
        assert!(Geometry::plan(0, 1000, 2, 4096).is_err());
        assert!(Geometry::plan(2505, 1329, 0, 4096).is_err());
        assert!(Geometry::plan(2505, 1329, 2, 100_000).is_err());
    }

    #[test]
    fn data_cells_are_unique_and_complete() {
        let g = Geometry::plan(800, 600, 2, 64).expect("plan");
        let cells: Vec<(u32, u32)> = g.data_cells().collect();
        assert_eq!(cells.len(), g.data_bits());
        assert!(cells.iter().all(|&(x, y)| g.is_data(x, y)));
        assert!(cells.iter().all(|&(x, y)| !g.is_header(x, y)));
    }
}
