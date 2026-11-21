package screencode

import "fmt"

// Geometry 是一帧的版式：帧像素尺寸、模块像素、模块网格与分片净荷。
type Geometry struct {
	// FrameW / FrameH 是整张 PNG 的像素尺寸。
	FrameW uint32
	FrameH uint32
	// ModulePx 是每个模块占多少像素。
	ModulePx uint32
	// GridW / GridH 是整个码（含黑边框）的模块列数 / 行数。
	GridW uint32
	GridH uint32
	// BlockBytes 是每个分片的净荷字节数。
	BlockBytes int
}

// PlanGeometry 规划一帧的版式；校验顺序与报错文案与 Rust `Geometry::plan` 一致。
func PlanGeometry(frameW, frameH, modulePx uint32, blockBytes int) (Geometry, error) {
	if modulePx == 0 {
		return Geometry{}, fmt.Errorf("module_px 必须 >= 1")
	}
	if blockBytes == 0 || blockBytes > 65535 {
		return Geometry{}, fmt.Errorf("block_bytes 必须在 1..=%d 之间", 65535)
	}

	marginPx := MarginModules * modulePx
	if frameW <= marginPx*2+modulePx || frameH <= marginPx*2+modulePx {
		return Geometry{}, fmt.Errorf(
			"帧尺寸 %dx%d 太小，放不下 %dpx 白边 + %dpx 模块",
			frameW, frameH, marginPx, modulePx,
		)
	}

	gridW := (frameW - marginPx*2) / modulePx
	gridH := (frameH - marginPx*2) / modulePx

	minW := BorderModules*2 + HeaderW
	minH := BorderModules*2 + HeaderH
	if gridW < minW || gridH < minH {
		return Geometry{}, fmt.Errorf(
			"模块网格 %dx%d 太小，至少需要 %dx%d（%dpx 模块）",
			gridW, gridH, minW, minH, modulePx,
		)
	}

	geo := Geometry{
		FrameW:     frameW,
		FrameH:     frameH,
		ModulePx:   modulePx,
		GridW:      gridW,
		GridH:      gridH,
		BlockBytes: blockBytes,
	}

	if geo.DataBits() < geo.SlotBytes()*8 {
		return Geometry{}, fmt.Errorf(
			"数据区只有 %d 位，放不下一个 %d+%d 字节的分片；请减小 --block-bytes 或增大帧尺寸",
			geo.DataBits(), blockBytes, SlotOverhead,
		)
	}

	return geo, nil
}

// CodeX 是码图（含黑边框）左上角的 x 像素。
func (g Geometry) CodeX() uint32 { return MarginModules * g.ModulePx }

// CodeY 是码图（含黑边框）左上角的 y 像素。
func (g Geometry) CodeY() uint32 { return MarginModules * g.ModulePx }

// CodeWPx 是码图的像素宽度。
func (g Geometry) CodeWPx() uint32 { return g.GridW * g.ModulePx }

// CodeHPx 是码图的像素高度。
func (g Geometry) CodeHPx() uint32 { return g.GridH * g.ModulePx }

// SlotBytes 是每个 slot 的字节数（净荷 + 8 字节开销）。
func (g Geometry) SlotBytes() int { return g.BlockBytes + SlotOverhead }

// DataBits 是扣掉边框与帧头之后的数据位数。
func (g Geometry) DataBits() int {
	inner := (int(g.GridW) - int(BorderModules)*2) * (int(g.GridH) - int(BorderModules)*2)
	return inner - int(HeaderW)*int(HeaderH)
}

// DataBytes 是数据区的字节数（向下取整）。
func (g Geometry) DataBytes() int { return g.DataBits() / 8 }

// SlotsPerFrame 是一帧能放多少个 slot。
func (g Geometry) SlotsPerFrame() int { return g.DataBits() / (g.SlotBytes() * 8) }

// IsBorder 判断模块坐标是否落在黑边框上。
func (g Geometry) IsBorder(x, y uint32) bool {
	return x < BorderModules || y < BorderModules ||
		x >= g.GridW-BorderModules || y >= g.GridH-BorderModules
}

// IsHeader 判断模块坐标是否落在帧头区域。
func (g Geometry) IsHeader(x, y uint32) bool {
	x0 := BorderModules
	y0 := BorderModules
	return x >= x0 && x < x0+HeaderW && y >= y0 && y < y0+HeaderH
}
