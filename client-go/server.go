package main

import (
	_ "embed"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"strconv"
	"strings"
)

//go:embed web/index.html
var indexHTML []byte

// info 是 /api/info 的响应体，字段顺序与 Rust 侧 `Info` 一一对应。
type info struct {
	Format        string `json:"format"`
	FrameW        uint32 `json:"frame_w"`
	FrameH        uint32 `json:"frame_h"`
	ModulePx      uint32 `json:"module_px"`
	GridW         uint32 `json:"grid_w"`
	GridH         uint32 `json:"grid_h"`
	FrameCount    int    `json:"frame_count"`
	SlotsPerFrame int    `json:"slots_per_frame"`
	BlockBytes    int    `json:"block_bytes"`
	K             int    `json:"k"`
	M             int    `json:"m"`
	TotalShards   int    `json:"total_shards"`
	FileLen       uint64 `json:"file_len"`
	DwellMs       uint64 `json:"dwell_ms"`
	Source        string `json:"source"`
	Hash          string `json:"hash"`
}

// serve 启动播放服务，常驻直到进程被结束。
func serve(p *player, port uint16, log *logger) error {
	fmt.Println()
	fmt.Printf("播放地址: http://localhost:%d/\n", port)
	fmt.Println("提示: 页面默认自动循环播放；按 Esc 退出播放模式，F 全屏")

	address := fmt.Sprintf("0.0.0.0:%d", port)
	listener, err := net.Listen("tcp", address)
	if err != nil {
		return fmt.Errorf("监听 %s 失败: %v", address, err)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/api/info", func(w http.ResponseWriter, _ *http.Request) { handleInfo(w, p) })
	mux.HandleFunc("/api/frame/", func(w http.ResponseWriter, r *http.Request) { handleFrame(w, r, p, log) })
	mux.HandleFunc("/", handleIndex)
	return http.Serve(listener, noCache(mux))
}

func handleInfo(w http.ResponseWriter, p *player) {
	body, err := json.Marshal(info{
		Format:        "screencode/1",
		FrameW:        p.geometry.FrameW,
		FrameH:        p.geometry.FrameH,
		ModulePx:      p.geometry.ModulePx,
		GridW:         p.geometry.GridW,
		GridH:         p.geometry.GridH,
		FrameCount:    p.frameCount,
		SlotsPerFrame: p.slotsPerFrame,
		BlockBytes:    int(p.header.BlockBytes),
		K:             int(p.header.K),
		M:             int(p.header.M),
		TotalShards:   int(p.header.TotalShards),
		FileLen:       p.fileLen,
		DwellMs:       p.dwellMs,
		Source:        p.source,
		Hash:          p.hashHex,
	})
	if err != nil {
		// 纯数字与字符串字段不会序列化失败；真出错按 500 返回，不静默。
		writeText(w, http.StatusInternalServerError, "渲染失败")
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(body)
}

// handleFrame 返回第 n 帧（1 基）的 PNG，允许带 `.png` 后缀。
func handleFrame(w http.ResponseWriter, r *http.Request, p *player, log *logger) {
	name := strings.TrimPrefix(r.URL.Path, "/api/frame/")
	name = strings.TrimSuffix(name, ".png")
	// Rust 的 usize 解析接受前导 '+'，这里手工剥掉以保持同样的接受范围。
	index, err := strconv.ParseUint(strings.TrimPrefix(name, "+"), 10, 64)
	if err != nil || index < 1 || index > uint64(p.frameCount) {
		writeText(w, http.StatusNotFound, "帧号无效")
		return
	}

	png, err := p.framePNG(int(index - 1))
	if err != nil {
		log.Errorf("渲染第 %d 帧失败: %v", index, err)
		writeText(w, http.StatusInternalServerError, "渲染失败")
		return
	}
	log.Debugf("渲染第 %d 帧（%s）", index, formatBytes(uint64(len(png))))
	w.Header().Set("Content-Type", "image/png")
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(png)
}

func handleIndex(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		writeText(w, http.StatusNotFound, "404 Not Found")
		return
	}
	w.Header().Set("Content-Type", "text/html")
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(indexHTML)
}

func writeText(w http.ResponseWriter, status int, body string) {
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.WriteHeader(status)
	_, _ = w.Write([]byte(body))
}

// noCache 给所有响应加禁止缓存的头；每次取 Header 都会重新覆盖，
// 因此 handler 里设置的 `Cache-Control: no-store` 也会被统一改写。
func noCache(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		next.ServeHTTP(cacheGuard{ResponseWriter: w}, r)
	})
}

type cacheGuard struct {
	http.ResponseWriter
}

func (c cacheGuard) Header() http.Header {
	header := c.ResponseWriter.Header()
	header.Set("Cache-Control", "no-cache, no-store, must-revalidate")
	header.Set("Pragma", "no-cache")
	return header
}
