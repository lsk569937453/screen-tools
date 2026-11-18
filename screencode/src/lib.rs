//! ScreenCode v1：面向「同机窗口抓屏」通道的单张高密度二维码。
//!
//! 这里定义生成端与识别端共享的唯一契约：版式几何、帧头、分片、纠删码、渲染与识别。

pub const FORMAT_VERSION: u8 = 1;

/// 码四周的白色静区（模块数）
pub const MARGIN_MODULES: u32 = 4;
/// 黑边框厚度（模块数）
pub const BORDER_MODULES: u32 = 2;
/// 帧头区域（模块）
pub const HEADER_W: u32 = 64;
pub const HEADER_H: u32 = 16;
/// 单份帧头字节数
pub const HEADER_BYTES: usize = 40;
/// 帧头冗余份数
pub const HEADER_COPIES: usize = 3;
/// 单分片默认净荷
pub const DEFAULT_BLOCK_BYTES: usize = 4096;
/// 每个 slot 的额外开销：idx(u32) + crc32
pub const SLOT_OVERHEAD: usize = 8;
/// 填充 slot 的 idx
pub const PAD_SLOT: u32 = u32::MAX;

pub mod detect;
pub mod fec;
pub mod header;
pub mod layout;
pub mod render;
pub mod shard;

#[cfg(windows)]
pub mod win;
