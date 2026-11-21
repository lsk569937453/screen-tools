//go:build !windows

package main

import "fmt"

// detectFrame 在非 Windows 上无法探测窗口，`--frame auto` 会回退到默认尺寸。
func detectFrame(_ string) (uint32, uint32, bool) { return 0, 0, false }

// probeWindow 在非 Windows 上显式报错，不做静默降级。
func probeWindow(_ string) error {
	return fmt.Errorf("窗口探测只支持 Windows；请用 --frame WxH 手动指定")
}
