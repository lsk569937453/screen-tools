package screencode

import (
	"bytes"
	"encoding/binary"
	"image"
	"image/png"
	"testing"
)

func sampleGeometry(t *testing.T) Geometry {
	t.Helper()
	geo, err := PlanGeometry(800, 600, 2, 256)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	return geo
}

func sampleFrameHeader(geo Geometry) Header {
	return NewHeader(HeaderSpec{
		ModulePx:    geo.ModulePx,
		GridW:       geo.GridW,
		GridH:       geo.GridH,
		BlockBytes:  geo.BlockBytes,
		K:           10,
		M:           2,
		TotalShards: 12,
		FileLen:     40_000,
		HashPrefix:  [8]byte{9, 9, 9, 9, 9, 9, 9, 9},
	})
}

// pngInfo 直接从 IHDR 读尺寸与像素格式，用于确认「灰度 + 位深 1」。
func pngInfo(t *testing.T, data []byte) (width, height uint32, bitDepth, colorType byte) {
	t.Helper()
	if !bytes.HasPrefix(data, []byte{0x89, 'P', 'N', 'G', 0x0D, 0x0A, 0x1A, 0x0A}) {
		t.Fatal("不是 PNG 文件")
	}
	if string(data[12:16]) != "IHDR" {
		t.Fatalf("第一个 chunk 不是 IHDR，而是 %q", data[12:16])
	}
	width = binary.BigEndian.Uint32(data[16:20])
	height = binary.BigEndian.Uint32(data[20:24])
	return width, height, data[24], data[25]
}

func decodeGray(t *testing.T, data []byte) *image.Gray {
	t.Helper()
	img, err := png.Decode(bytes.NewReader(data))
	if err != nil {
		t.Fatalf("解码 PNG 失败: %v", err)
	}
	gray, ok := img.(*image.Gray)
	if !ok {
		t.Fatalf("解码结果是 %T，期望 *image.Gray", img)
	}
	return gray
}

func TestRenderPNGFormat(t *testing.T) {
	geo := sampleGeometry(t)
	header := sampleFrameHeader(geo)

	data, err := RenderPNG(geo, header, nil)
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}
	width, height, bitDepth, colorType := pngInfo(t, data)
	if width != geo.FrameW || height != geo.FrameH {
		t.Fatalf("尺寸 = %dx%d，期望 %dx%d", width, height, geo.FrameW, geo.FrameH)
	}
	if bitDepth != 1 || colorType != 0 {
		t.Fatalf("像素格式 位深=%d 颜色类型=%d，期望 1/0（灰度 1-bit）", bitDepth, colorType)
	}
	img := decodeGray(t, data)
	if img.Rect.Dx() != int(geo.FrameW) || img.Rect.Dy() != int(geo.FrameH) {
		t.Fatalf("解码尺寸 = %v", img.Rect)
	}
}

func TestRenderPNGBorderAndMargin(t *testing.T) {
	geo := sampleGeometry(t)
	header := sampleFrameHeader(geo)

	data, err := RenderPNG(geo, header, nil)
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}
	img := decodeGray(t, data)
	at := func(x, y int) byte { return img.Pix[y*img.Stride+x] }

	if at(0, 0) != 0xFF {
		t.Fatalf("左上白边应为白，实际 %d", at(0, 0))
	}
	if at(int(geo.CodeX()), int(geo.CodeY())) != 0 {
		t.Fatal("黑边框左上角应为黑")
	}
	// 模块 (1,3) 仍在厚度 2 的边框上
	if at(int(geo.CodeX())+2, int(geo.CodeY())+3*2) != 0 {
		t.Fatal("模块 (1,3) 应为黑边框")
	}
	// 边框内侧第一格位于数据区或帧头，不能是整块纯黑的边框
	if at(int(geo.CodeX())+2*2, int(geo.CodeY())+3*2) != 0xFF {
		t.Fatal("边框内侧第一格应为白（该位为 0 时才是黑）")
	}
}

func TestRenderPNGHeaderBits(t *testing.T) {
	geo := sampleGeometry(t)
	header := sampleFrameHeader(geo)
	headerBytes := header.ToBitBytes()

	data, err := RenderPNG(geo, header, nil)
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}
	img := decodeGray(t, data)
	module := int(geo.ModulePx)
	at := func(x, y int) byte { return img.Pix[y*img.Stride+x] }

	for hy := 0; hy < int(HeaderH); hy++ {
		for hx := 0; hx < int(HeaderW); hx++ {
			bit := (headerBytes[(hy*int(HeaderW)+hx)/8] >> (7 - uint((hy*int(HeaderW)+hx)%8))) & 1
			want := byte(0xFF)
			if bit == 1 {
				want = 0
			}
			// 取模块左上角像素即可代表整个模块
			x := int(geo.CodeX()) + (int(BorderModules)+hx)*module
			y := int(geo.CodeY()) + (int(BorderModules)+hy)*module
			if at(x, y) != want {
				t.Fatalf("帧头模块 (%d,%d) = %d，期望 %d", hx, hy, at(x, y), want)
			}
		}
	}
}

func TestRenderPNGRejectsBadSlots(t *testing.T) {
	geo := sampleGeometry(t)
	header := sampleFrameHeader(geo)

	if _, err := RenderPNG(geo, header, [][]byte{{0, 1, 2}}); err == nil {
		t.Fatal("slot 长度不对时应当报错")
	}

	tooMany := make([][]byte, geo.SlotsPerFrame()+1)
	for i := range tooMany {
		tooMany[i] = make([]byte, geo.SlotBytes())
	}
	if _, err := RenderPNG(geo, header, tooMany); err == nil {
		t.Fatal("slot 数量超出时应报错")
	}
}

func TestRenderPNGDeterministic(t *testing.T) {
	geo := sampleGeometry(t)
	header := sampleFrameHeader(geo)
	slot := EncodeSlot(0, make([]byte, geo.BlockBytes))

	first, err := RenderPNG(geo, header, [][]byte{slot})
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}
	second, err := RenderPNG(geo, header, [][]byte{slot})
	if err != nil {
		t.Fatalf("渲染失败: %v", err)
	}
	if !bytes.Equal(first, second) {
		t.Fatal("同一输入两次渲染结果不同")
	}
}

func TestDataRowStarts(t *testing.T) {
	geo, err := PlanGeometry(900, 700, 3, 128)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	starts := dataRowStarts(geo)
	if starts[int(geo.GridH)] != geo.DataBits() {
		t.Fatalf("行起点累计 = %d，期望 %d", starts[int(geo.GridH)], geo.DataBits())
	}
	if starts[2] != 0 {
		t.Fatalf("边框行不应有数据位，实际 %d", starts[2])
	}
	first := starts[int(BorderModules+1)] - starts[int(BorderModules)]
	if want := int(geo.GridW) - 2*int(BorderModules) - int(HeaderW); first != want {
		t.Fatalf("帧头行数据位 = %d，期望 %d", first, want)
	}
	y := int(BorderModules + HeaderH)
	full := starts[y+1] - starts[y]
	if want := int(geo.GridW) - 2*int(BorderModules); full != want {
		t.Fatalf("帧头之后的行数据位 = %d，期望 %d", full, want)
	}
}
