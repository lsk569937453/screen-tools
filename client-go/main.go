// client-go 是 ScreenCode 发送端的 Go 复刻版：把文件编码成单张高密度码并起 HTTP 播放服务。
// 一帧 = 一张 frame_w × frame_h 的 1-bit PNG（ScreenCode v1），码制与 Rust 版逐个字节一致；
// 只用标准库与 x/sys，产物可在 Windows 7 上运行。
package main

import (
	"crypto/sha256"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"screentools/client-go/screencode"
)

// logDir 是日志目录：与 Rust 版一致，固定在工作目录下的 logs/。
const logDir = "logs"

func main() {
	bin := executableName()

	parsed, action, parseErr := parseArgs(bin, os.Args[1:])
	if parseErr != nil {
		fmt.Fprint(os.Stderr, parseErr.body)
		os.Exit(2)
	}
	switch action {
	case actionHelpShort:
		fmt.Print(helpTextFor(helpShortText, bin))
		return
	case actionHelpLong:
		fmt.Print(helpTextFor(helpLongText, bin))
		return
	case actionVersion:
		fmt.Printf("%s %s\n", programName, programVersion)
		return
	}

	// `client help` 等价于 print_long_help()：Usage 用命令名常量，且末尾多一个空行。
	if parsed.wantsHelp() {
		fmt.Print(helpTextFor(helpLongText, programName))
		fmt.Println()
		return
	}

	log, fileErr := newLogger(programName, parsed.verbose, logDir)
	if fileErr != nil {
		log.Warnf("日志文件不可用，仅输出到控制台: %v", fileErr)
	}
	if log.filePath != "" {
		log.Infof("日志文件: %s", log.filePath)
	}

	if err := run(parsed, log); err != nil {
		log.Errorf("%v", err)
		log.close()
		os.Exit(1)
	}
	log.close()
}

// executableName 取 argv[0] 的基名，用作帮助里的 Usage 名称（与 clap 一致）。
func executableName() string {
	if len(os.Args) == 0 || os.Args[0] == "" {
		return programName
	}
	return filepath.Base(os.Args[0])
}

func run(parsed cli, log *logger) error {
	if parsed.probe {
		return probeWindow(parsed.process)
	}
	if !parsed.hasFile {
		return fmt.Errorf("缺少文件参数。用法: client <文件路径> [选项]，或 client --help")
	}

	filePath := parsed.file
	if stat, err := os.Stat(filePath); err != nil || !stat.Mode().IsRegular() {
		return fmt.Errorf("文件不存在或不是普通文件: %s", filePath)
	}
	data, err := os.ReadFile(filePath)
	if err != nil {
		return fmt.Errorf("读取 %s 失败: %v", filePath, err)
	}
	if len(data) == 0 {
		return fmt.Errorf("文件为空，无法生成")
	}

	frameW, frameH := resolveFrame(parsed, log)
	geometry, err := screencode.PlanGeometry(frameW, frameH, parsed.modulePx, parsed.blockBytes)
	if err != nil {
		return err
	}
	plan, err := screencode.PlanFEC(len(data), parsed.blockBytes, parsed.parity)
	if err != nil {
		return err
	}

	digest := sha256.Sum256(data)
	hashPrefix := [8]byte{}
	copy(hashPrefix[:], digest[:8])
	hashHex := fmt.Sprintf("%x", digest)

	started := time.Now()
	shards, err := screencode.EncodeShards(data, plan, parsed.blockBytes)
	if err != nil {
		return err
	}
	header := screencode.NewHeader(screencode.HeaderSpec{
		ModulePx:    geometry.ModulePx,
		GridW:       geometry.GridW,
		GridH:       geometry.GridH,
		BlockBytes:  geometry.BlockBytes,
		K:           plan.K,
		M:           plan.M,
		TotalShards: plan.TotalShards,
		FileLen:     uint64(len(data)),
		HashPrefix:  hashPrefix,
	})

	p := newPlayer(header, geometry, shards, filePath, uint64(len(data)), parsed.dwellMs, hashHex)

	log.Infof("文件: %s", filePath)
	log.Infof(
		"大小: %s（%d 字节）| SHA-256: %s",
		formatBytes(uint64(len(data))),
		len(data),
		hashHex,
	)
	log.Infof(
		"版式: %dx%d px | 模块 %dpx | 网格 %dx%d | 每帧 %d 个分片 / %s",
		geometry.FrameW,
		geometry.FrameH,
		geometry.ModulePx,
		geometry.GridW,
		geometry.GridH,
		p.slotsPerFrame,
		formatBytes(uint64(p.bytesPerFrame())),
	)
	log.Infof(
		"码制: %d KB/帧(%s) | k=%d m=%d | 分片 %d | 总帧数 %d | 停留 %d ms | 编码 %.1fs",
		p.bytesPerFrame()/1024,
		formatBytes(uint64(p.bytesPerFrame())),
		plan.K,
		plan.M,
		plan.TotalShards,
		p.frameCount,
		parsed.dwellMs,
		time.Since(started).Seconds(),
	)

	if parsed.hasExport {
		return exportAll(p, parsed.export, log)
	}
	return serve(p, parsed.port, log)
}

// resolveFrame 处理 `--frame auto`：探测 `--process` 的窗口客户区，失败回退默认尺寸。
func resolveFrame(parsed cli, log *logger) (uint32, uint32) {
	spec := parsed.resolvedFrame()
	if !spec.auto {
		return spec.width, spec.height
	}
	if width, height, ok := detectFrame(parsed.process); ok {
		return width, height
	}
	log.Warnf(
		"没有找到进程 %s 的窗口，回退到 %dx%d",
		parsed.process,
		defaultFrameWidth,
		defaultFrameHeight,
	)
	log.Warnf("--frame 必须 <= 播放页面的可视区（页面面板里会打印“页面可视区 WxH”），否则码图会被浏览器/任务栏裁掉，接收端找不到完整码框")
	return defaultFrameWidth, defaultFrameHeight
}
