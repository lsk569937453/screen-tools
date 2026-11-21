package screencode

import "testing"

func TestPlanGeometryReference(t *testing.T) {
	cases := []struct {
		name      string
		frameW    uint32
		frameH    uint32
		modulePx  uint32
		wantGridW uint32
		wantGridH uint32
		wantBits  int
		wantSlots int
		wantSlotB int
		wantDataB int
	}{
		{"module2", 2505, 1329, 2, 1244, 656, 807456, 24, 4104, 100932},
		{"module1", 2505, 1329, 1, 2497, 1321, 3282257, 99, 4104, 410282},
		{"module3", 2505, 1329, 3, 827, 435, 353689, 10, 4104, 44211},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			geo, err := PlanGeometry(tc.frameW, tc.frameH, tc.modulePx, 4096)
			if err != nil {
				t.Fatalf("规划失败: %v", err)
			}
			if geo.GridW != tc.wantGridW || geo.GridH != tc.wantGridH {
				t.Fatalf("网格 = %dx%d，期望 %dx%d", geo.GridW, geo.GridH, tc.wantGridW, tc.wantGridH)
			}
			if got := geo.DataBits(); got != tc.wantBits {
				t.Fatalf("DataBits = %d，期望 %d", got, tc.wantBits)
			}
			if got := geo.DataBytes(); got != tc.wantDataB {
				t.Fatalf("DataBytes = %d，期望 %d", got, tc.wantDataB)
			}
			if got := geo.SlotBytes(); got != tc.wantSlotB {
				t.Fatalf("SlotBytes = %d，期望 %d", got, tc.wantSlotB)
			}
			if got := geo.SlotsPerFrame(); got != tc.wantSlots {
				t.Fatalf("SlotsPerFrame = %d，期望 %d", got, tc.wantSlots)
			}
			if got, want := geo.CodeX(), MarginModules*tc.modulePx; got != want {
				t.Fatalf("CodeX = %d，期望 %d", got, want)
			}
			if got, want := geo.CodeWPx(), geo.GridW*tc.modulePx; got != want {
				t.Fatalf("CodeWPx = %d，期望 %d", got, want)
			}
		})
	}
}

func TestPlanGeometryDefaultFrame(t *testing.T) {
	geo, err := PlanGeometry(1920, 1080, 2, DefaultBlockBytes)
	if err != nil {
		t.Fatalf("默认帧规划失败: %v", err)
	}
	if geo.GridW != 952 || geo.GridH != 532 {
		t.Fatalf("网格 = %dx%d，期望 952x532", geo.GridW, geo.GridH)
	}
	if geo.SlotsPerFrame() < 1 {
		t.Fatalf("每帧槽位 = %d", geo.SlotsPerFrame())
	}
}

func TestPlanGeometryRejects(t *testing.T) {
	cases := []struct {
		name       string
		frameW     uint32
		frameH     uint32
		modulePx   uint32
		blockBytes int
	}{
		{"module0", 800, 600, 0, 4096},
		{"block0", 800, 600, 2, 0},
		{"blockTooBig", 800, 600, 2, 65536},
		{"frameTooSmall", 40, 40, 2, 4096},
		{"gridTooSmall", 160, 120, 2, 4096},
		{"blockTooBigForFrame", 320, 240, 2, 4096},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := PlanGeometry(tc.frameW, tc.frameH, tc.modulePx, tc.blockBytes); err == nil {
				t.Fatalf("期望报错，实际通过")
			}
		})
	}
}

func TestGeometryBorderAndHeader(t *testing.T) {
	geo, err := PlanGeometry(800, 600, 2, 256)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	if !geo.IsBorder(0, 0) || !geo.IsBorder(geo.GridW-1, 5) || !geo.IsBorder(5, geo.GridH-1) {
		t.Fatal("边框判定缺失")
	}
	if geo.IsBorder(BorderModules, BorderModules) {
		t.Fatal("边框内侧不应判为边框")
	}
	if !geo.IsHeader(BorderModules, BorderModules) {
		t.Fatal("帧头左上角应在帧头区")
	}
	if geo.IsHeader(BorderModules+HeaderW, BorderModules) {
		t.Fatal("帧头右侧不应在帧头区")
	}
	if geo.IsHeader(BorderModules, BorderModules+HeaderH) {
		t.Fatal("帧头下方不应在帧头区")
	}
}
