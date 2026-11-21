// Package screencode 是 ScreenCode v1 码制的 Go 实现。
//
// 它与 Rust 的 `screencode` crate 逐字节同构：同样的白边、黑边框、64×16 模块帧头、
// slot 布局与纠删码分组。任何一侧改动都会破坏兼容性，改动前先读 `docs/screencode-v1.md`。
package screencode

const (
	// FormatVersion 是帧头里的码制版本，必须与 Rust 侧一致。
	FormatVersion uint8 = 1
	// MarginModules 是码图外侧白边的模块数。
	MarginModules uint32 = 4
	// BorderModules 是黑边框的模块厚度。
	BorderModules uint32 = 2
	// HeaderW / HeaderH 是帧头区域的模块尺寸。
	HeaderW uint32 = 64
	HeaderH uint32 = 16
	// HeaderBytes 是单份帧头的字节数。
	HeaderBytes = 40
	// HeaderCopies 是同一帧里帧头重复的份数。
	HeaderCopies = 3
	// DefaultBlockBytes 是分片净荷的默认字节数。
	DefaultBlockBytes = 4096
	// SlotOverhead 是每个 slot 的额外开销：idx(4) + crc32(4)。
	SlotOverhead = 8
	// PadSlotIndex 是补位 slot 的索引，接收端跳过。
	PadSlotIndex uint32 = 0xFFFFFFFF

	// FecKMax 是单组数据分片上限。
	FecKMax = 100
	// FecMaxShardsPerGroup 是单组分片总数上限（GF(256)）。
	FecMaxShardsPerGroup = 256
)
