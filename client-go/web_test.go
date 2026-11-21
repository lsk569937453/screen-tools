package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// normalizeEOL 抹平 CRLF/LF 差异：仓库开 core.autocrlf 时同一份文件在不同机器上换行风格不同，
// 对拍只关心内容，不关心换行。
func normalizeEOL(s string) string {
	return strings.ReplaceAll(s, "\r\n", "\n")
}

// 播放页必须与 Rust 版 client 的内嵌页面保持一致，否则两边页面行为会漂移。
func TestWebIndexMatchesRustClient(t *testing.T) {
	rustPath := filepath.Join("..", "client", "web", "index.html")
	want, err := os.ReadFile(rustPath)
	if err != nil {
		t.Skipf("找不到 Rust 版播放页 %s（%v），跳过比对", rustPath, err)
	}
	got, err := os.ReadFile(filepath.Join("web", "index.html"))
	if err != nil {
		t.Fatalf("读取 web/index.html 失败: %v", err)
	}
	if normalizeEOL(string(got)) != normalizeEOL(string(want)) {
		t.Fatalf("web/index.html 与 %s 不一致", rustPath)
	}
}
