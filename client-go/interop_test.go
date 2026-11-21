package main

import (
	"bytes"
	"image"
	"image/png"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// TestInteropWithRust 与 Rust 版对拍，默认跳过：
//
//	SCREENCODE_INTEROP=1 go test -run TestInteropWithRust -v ./...
//
// 覆盖三件事：
//  1. 同一份文件、同样的参数，Go 与 Rust 导出的帧目录逐帧像素完全一致；
//  2. Rust 的 verify_frames 能解析 Go 的帧并还原出同一个文件；
//  3. 帧数与文件名一致。
func TestInteropWithRust(t *testing.T) {
	if os.Getenv("SCREENCODE_INTEROP") != "1" {
		t.Skip("设置 SCREENCODE_INTEROP=1 才与 Rust 版对拍")
	}
	goBin, err := exec.LookPath("go")
	if err != nil {
		t.Skip("没有找到 go，跳过对拍")
	}
	cargoBin, err := exec.LookPath("cargo")
	if err != nil {
		t.Skip("没有找到 cargo，跳过对拍")
	}

	workDir := t.TempDir()
	data := make([]byte, 3*4096+17)
	for i := range data {
		// 确定性的伪随机内容：覆盖各种字节值，包括 0x00 与 0xFF。
		data[i] = byte(i*37 + 11)
	}
	dataPath := filepath.Join(workDir, "sample.bin")
	if err := os.WriteFile(dataPath, data, 0o644); err != nil {
		t.Fatalf("写样本文件失败: %v", err)
	}

	rustDir := filepath.Join(workDir, "rust-frames")
	goDir := filepath.Join(workDir, "go-frames")

	// Rust 侧：用仓库里的 client 导出同一份文件的帧。
	runTool(t, cargoBin, "..", "run", "-q", "-p", "client", "--",
		dataPath, "--export", rustDir, "--frame", "800x600", "--parity", "0.5")

	// Go 侧：先构建出真正的可执行文件，再走与用户相同的导出路径。
	exePath := filepath.Join(workDir, "client-go.exe")
	build := exec.Command(goBin, "build", "-o", exePath, ".")
	build.Dir = "."
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("构建 client-go 失败: %v\n%s", err, out)
	}
	runTool(t, exePath, workDir, dataPath, "--export", goDir, "--frame", "800x600", "--parity", "0.5")

	// 1) 逐帧像素对比。
	rustFrames := listFrames(t, rustDir)
	goFrames := listFrames(t, goDir)
	if len(rustFrames) != len(goFrames) {
		t.Fatalf("帧数不一致：Rust %d 帧，Go %d 帧", len(rustFrames), len(goFrames))
	}
	for index, name := range rustFrames {
		if goFrames[index] != name {
			t.Fatalf("第 %d 帧文件名不一致：Rust %q，Go %q", index+1, name, goFrames[index])
		}
		comparePixels(t, name, decodePNG(t, filepath.Join(rustDir, name)), decodePNG(t, filepath.Join(goDir, name)))
	}

	// 2) 用 Rust 的离线自检工具还原 Go 导出的帧。
	out := runTool(t, cargoBin, "..", "run", "-q", "-p", "screencode", "--example", "verify_frames", "--", goDir)
	if !strings.Contains(out, "还原成功") {
		t.Fatalf("verify_frames 没有报告还原成功:\n%s", out)
	}
}

// runTool 运行外部命令，工作目录相对本包目录，失败时连输出一起报出。
func runTool(t *testing.T, name, dir string, args ...string) string {
	t.Helper()
	command := exec.Command(name, args...)
	command.Dir = dir
	out, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("执行 %s %v 失败: %v\n%s", name, args, err, out)
	}
	return string(out)
}

func listFrames(t *testing.T, dir string) []string {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("读取帧目录 %s 失败: %v", dir, err)
	}
	names := make([]string, 0, len(entries))
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".png") {
			continue
		}
		names = append(names, entry.Name())
	}
	if len(names) == 0 {
		t.Fatalf("帧目录 %s 是空的", dir)
	}
	return names
}

func decodePNG(t *testing.T, path string) image.Image {
	t.Helper()
	file, err := os.Open(path)
	if err != nil {
		t.Fatalf("打开 %s 失败: %v", path, err)
	}
	defer func() {
		_ = file.Close()
	}()
	decoded, err := png.Decode(file)
	if err != nil {
		t.Fatalf("解码 %s 失败: %v", path, err)
	}
	return decoded
}

// comparePixels 逐像素比较两张图，忽略 PNG 压缩方式带来的字节差异。
func comparePixels(t *testing.T, name string, rust, goImage image.Image) {
	t.Helper()
	rustBounds := rust.Bounds()
	goBounds := goImage.Bounds()
	if rustBounds != goBounds {
		t.Fatalf("%s 尺寸不一致：Rust %v，Go %v", name, rustBounds, goBounds)
	}
	if !bytes.Equal(pixels(rust), pixels(goImage)) {
		t.Fatalf("%s 像素不一致", name)
	}
}

// pixels 把图像拍平成灰阶字节，1-bit 与 8-bit 灰度都按 0/255 取值。
func pixels(source image.Image) []byte {
	bounds := source.Bounds()
	out := make([]byte, 0, bounds.Dx()*bounds.Dy())
	for y := bounds.Min.Y; y < bounds.Max.Y; y++ {
		for x := bounds.Min.X; x < bounds.Max.X; x++ {
			red, _, _, _ := source.At(x, y).RGBA()
			out = append(out, byte(red>>8))
		}
	}
	return out
}
