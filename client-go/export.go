package main

import (
	"fmt"
	"os"
	"path/filepath"
)

// exportAll 把所有帧写成 `frame_00001.png` 这样的 1 基文件名，不起 HTTP 服务。
func exportAll(p *player, dir string, log *logger) error {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("创建目录 %s 失败: %w", dir, err)
	}
	total := p.frameCount
	for index := 0; index < total; index++ {
		png, err := p.framePNG(index)
		if err != nil {
			return err
		}
		path := filepath.Join(dir, fmt.Sprintf("frame_%05d.png", index+1))
		if err := os.WriteFile(path, png, 0o644); err != nil {
			return fmt.Errorf("写 %s 失败: %w", path, err)
		}
		if (index+1)%10 == 0 || index+1 == total {
			// os.Stdout 无缓冲，直接写即可立即显示进度，不需要额外 flush。
			fmt.Printf("\r导出 %d/%d", index+1, total)
		}
	}
	fmt.Println()
	log.Infof("已写出 %d 帧到 %s/（每帧 %s）", total, dir, formatBytes(uint64(p.bytesPerFrame())))
	return nil
}
