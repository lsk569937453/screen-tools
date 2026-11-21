package screencode

import (
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"errors"
	"fmt"
	"hash/crc32"
)

// RenderPNG 把一帧的分片渲染成 1-bit 灰度 PNG（黑 = 位 0，白 = 位 1）。
//
// `slots` 是这一帧要放的 slot 字节（每项长度必须是 geo.SlotBytes()）；
// 不足 SlotsPerFrame() 的部分自动用补位 slot 补齐。
func RenderPNG(geo Geometry, header Header, slots [][]byte) ([]byte, error) {
	slotBytes := geo.SlotBytes()
	slotsPerFrame := geo.SlotsPerFrame()

	if len(slots) > slotsPerFrame {
		return nil, fmt.Errorf("一帧最多放 %d 个分片，传入 %d 个", slotsPerFrame, len(slots))
	}
	for _, slot := range slots {
		if len(slot) != slotBytes {
			return nil, fmt.Errorf("slot 长度 %d，期望 %d", len(slot), slotBytes)
		}
	}
	if geo.FrameW == 0 || geo.FrameH == 0 {
		return nil, errors.New("帧尺寸不能为 0")
	}

	// 数据位流：真实 slot + 补位 slot
	dataStream := make([]byte, 0, slotsPerFrame*slotBytes)
	for _, slot := range slots {
		dataStream = append(dataStream, slot...)
	}
	pad := PadSlot(geo.BlockBytes)
	for len(dataStream) < slotsPerFrame*slotBytes {
		dataStream = append(dataStream, pad...)
	}

	headerBytes := header.ToBitBytes()
	rowStart := dataRowStarts(geo)

	width := int(geo.FrameW)
	height := int(geo.FrameH)
	rowBytes := (width + 7) / 8
	pixels := make([]byte, rowBytes*height)
	for i := range pixels {
		pixels[i] = 0xFF
	}

	codeX := int(geo.CodeX())
	codeY := int(geo.CodeY())
	module := int(geo.ModulePx)

	for py := 0; py < height; py++ {
		dy := py - codeY
		if dy < 0 {
			continue
		}
		my := dy / module
		if my >= int(geo.GridH) {
			continue
		}
		rowOffset := py * rowBytes

		for mx := 0; mx < int(geo.GridW); mx++ {
			if !moduleDark(geo, headerBytes, dataStream, rowStart, mx, my) {
				continue
			}
			px0 := codeX + mx*module
			px1 := px0 + module
			if px1 > width {
				px1 = width
			}
			for px := px0; px < px1; px++ {
				pixels[rowOffset+px/8] &^= 0x80 >> (px % 8)
			}
		}
	}

	return encode1BitGray(pixels, width, height, rowBytes)
}

// dataRowStarts 是每一行数据区之前的累计数据位数（行优先）。
func dataRowStarts(geo Geometry) []int {
	gridW := int(geo.GridW)
	gridH := int(geo.GridH)
	border := int(BorderModules)
	starts := make([]int, gridH+1)
	acc := 0
	for y := 0; y < gridH; y++ {
		starts[y] = acc
		// 上下边框行没有数据位
		if y < border || y >= gridH-border {
			continue
		}
		cells := gridW - 2*border
		if y < border+int(HeaderH) {
			cells -= int(HeaderW)
		}
		if cells < 0 {
			cells = 0
		}
		acc += cells
	}
	starts[gridH] = acc
	return starts
}

// dataIndex 把模块坐标换算成数据位流里的位下标。
func dataIndex(rowStart []int, x, y int) int {
	border := int(BorderModules)
	idx := rowStart[y] + (x - border)
	if y >= border && y < border+int(HeaderH) && x >= border+int(HeaderW) {
		idx -= int(HeaderW)
	}
	return idx
}

// bitAt 读位流中的一位（MSB 在前），越界视为 0。
func bitAt(data []byte, index int) bool {
	if index < 0 {
		return false
	}
	byteIndex := index / 8
	if byteIndex >= len(data) {
		return false
	}
	return (data[byteIndex]>>(7-uint(index%8)))&1 == 1
}

// moduleDark 判定一个模块是否黑。
func moduleDark(geo Geometry, headerBytes, dataStream []byte, rowStart []int, x, y int) bool {
	border := int(BorderModules)
	x32, y32 := uint32(x), uint32(y)
	if x32 < BorderModules || y32 < BorderModules ||
		x32 >= geo.GridW-BorderModules || y32 >= geo.GridH-BorderModules {
		return true
	}
	if geo.IsHeader(x32, y32) {
		hx := x - border
		hy := y - border
		return bitAt(headerBytes, hy*int(HeaderW)+hx)
	}
	return bitAt(dataStream, dataIndex(rowStart, x, y))
}

// encode1BitGray 输出「灰度 + 位深 1」的 PNG（color type 0）。
//
// Go 标准库的 png 编码器最小位深是 8 位（灰度）或索引色，无法产出位深 1 的灰度图；
// 而 `screencode` 的离线校验与测试都按 `w/8` 直接读位流，所以这里手写最小 PNG。
// 每行固定 filter 0（与 Rust 侧输出一致），行尾填充位填 1（与 Rust 的 0xFF 初始缓冲一致）。
func encode1BitGray(packed []byte, width, height, rowBytes int) ([]byte, error) {
	if width <= 0 || height <= 0 || rowBytes != (width+7)/8 {
		return nil, fmt.Errorf("1-bit 位图尺寸不合法: %dx%d rowBytes=%d", width, height, rowBytes)
	}
	if len(packed) != rowBytes*height {
		return nil, fmt.Errorf("位图数据长度 %d，期望 %d", len(packed), rowBytes*height)
	}

	raw := make([]byte, 0, (rowBytes+1)*height)
	for y := 0; y < height; y++ {
		raw = append(raw, 0)
		raw = append(raw, packed[y*rowBytes:(y+1)*rowBytes]...)
	}

	var compressed bytes.Buffer
	writer, err := zlib.NewWriterLevel(&compressed, zlib.BestSpeed)
	if err != nil {
		return nil, fmt.Errorf("初始化 zlib 失败: %w", err)
	}
	if _, err := writer.Write(raw); err != nil {
		return nil, fmt.Errorf("压缩位图失败: %w", err)
	}
	if err := writer.Close(); err != nil {
		return nil, fmt.Errorf("结束压缩失败: %w", err)
	}

	var out bytes.Buffer
	out.Write([]byte{0x89, 'P', 'N', 'G', 0x0D, 0x0A, 0x1A, 0x0A})

	var ihdr [13]byte
	binary.BigEndian.PutUint32(ihdr[0:4], uint32(width))
	binary.BigEndian.PutUint32(ihdr[4:8], uint32(height))
	ihdr[8] = 1 // 位深
	ihdr[9] = 0 // 颜色类型：灰度
	ihdr[10] = 0
	ihdr[11] = 0
	ihdr[12] = 0
	writeChunk(&out, "IHDR", ihdr[:])
	writeChunk(&out, "IDAT", compressed.Bytes())
	writeChunk(&out, "IEND", nil)
	return out.Bytes(), nil
}

// writeChunk 写一个 PNG chunk：长度 + 类型 + 数据 + CRC32(type+data)。
func writeChunk(out *bytes.Buffer, kind string, data []byte) {
	var length [4]byte
	binary.BigEndian.PutUint32(length[:], uint32(len(data)))
	out.Write(length[:])

	payload := make([]byte, 0, len(kind)+len(data))
	payload = append(payload, kind...)
	payload = append(payload, data...)
	out.Write(payload)

	var sum [4]byte
	binary.BigEndian.PutUint32(sum[:], crc32.ChecksumIEEE(payload))
	out.Write(sum[:])
}
