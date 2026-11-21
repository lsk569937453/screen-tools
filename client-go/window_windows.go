//go:build windows

package main

import (
	"fmt"
	"sort"
	"strings"
	"unsafe"

	"golang.org/x/sys/windows"
)

// 与 screencode::win 保持一致：只匹配可见且标题非空的顶层窗口。
const (
	maxTitleLen                    = 512
	processQueryLimitedInformation = 0x1000
)

var (
	user32 = windows.NewLazySystemDLL("user32.dll")

	procIsIconic            = user32.NewProc("IsIconic")
	procGetClientRect       = user32.NewProc("GetClientRect")
	procClientToScreen      = user32.NewProc("ClientToScreen")
	procGetWindowTextLength = user32.NewProc("GetWindowTextLengthW")
	procGetWindowText       = user32.NewProc("GetWindowTextW")
)

// rect 是客户区在屏幕坐标系里的位置与大小（物理像素）。
type rect struct {
	x      int32
	y      int32
	width  int32
	height int32
}

func (r rect) isEmpty() bool { return r.width == 0 || r.height == 0 }

func (r rect) area() int64 { return int64(r.width) * int64(r.height) }

func (r rect) describe() string {
	return fmt.Sprintf("%dx%d @ (%d,%d)", r.width, r.height, r.x, r.y)
}

// windowInfo 是一个候选窗口。
type windowInfo struct {
	pid       uint32
	process   string
	title     string
	frame     rect
	minimized bool
}

func (w windowInfo) describe() string {
	text := fmt.Sprintf("进程 %s（pid %d）标题「%s」客户区 %s", w.process, w.pid, w.title, w.frame.describe())
	if w.minimized {
		text += "（已最小化）"
	}
	return text
}

// point 用于 ClientToScreen。
type point struct {
	x int32
	y int32
}

// listWindows 按客户区面积降序返回所有可见、标题非空的顶层窗口。
func listWindows() ([]windowInfo, error) {
	found := make([]windowInfo, 0, 32)
	callback := windows.NewCallback(func(hwnd uintptr, _ uintptr) uintptr {
		handle := windows.HWND(hwnd)
		if !windows.IsWindowVisible(handle) {
			return 1
		}
		title := windowTitle(handle)
		if title == "" {
			return 1
		}
		pid := windowPID(handle)
		minimized := isMinimized(handle)
		frame := rect{}
		if !minimized {
			frame, _ = clientFrame(handle)
		}
		found = append(found, windowInfo{
			pid:       pid,
			process:   processName(pid),
			title:     title,
			frame:     frame,
			minimized: minimized,
		})
		return 1
	})
	if err := windows.EnumWindows(callback, nil); err != nil {
		return nil, fmt.Errorf("EnumWindows 调用失败: %w", err)
	}
	sort.SliceStable(found, func(i, j int) bool { return found[i].frame.area() > found[j].frame.area() })
	return found, nil
}

// findWindow 按进程名与标题做「包含 + 忽略大小写」匹配，优先非最小化、面积大的窗口。
func findWindow(process, title string) (windowInfo, error) {
	needProcess := strings.ToLower(strings.TrimSpace(process))
	needTitle := strings.ToLower(strings.TrimSpace(title))
	if needProcess == "" && needTitle == "" {
		return windowInfo{}, fmt.Errorf("--process / --window 至少要给一个")
	}

	all, err := listWindows()
	if err != nil {
		return windowInfo{}, err
	}
	matches := make([]windowInfo, 0, len(all))
	for _, window := range all {
		if needProcess != "" && !strings.Contains(strings.ToLower(window.process), needProcess) {
			continue
		}
		if needTitle != "" && !strings.Contains(strings.ToLower(window.title), needTitle) {
			continue
		}
		matches = append(matches, window)
	}
	if len(matches) == 0 {
		return windowInfo{}, fmt.Errorf("没有找到符合条件的窗口")
	}
	sort.SliceStable(matches, func(i, j int) bool {
		if matches[i].minimized != matches[j].minimized {
			return !matches[i].minimized
		}
		return matches[i].frame.area() > matches[j].frame.area()
	})
	return matches[0], nil
}

// detectFrame 返回 `--frame auto` 用的窗口客户区尺寸。
func detectFrame(process string) (uint32, uint32, bool) {
	window, err := findWindow(process, "")
	if err != nil || window.frame.isEmpty() {
		return 0, 0, false
	}
	return uint32(window.frame.width), uint32(window.frame.height), true
}

// probeWindow 打印与进程名相关的窗口客户区，供 `--probe` 使用。
func probeWindow(process string) error {
	fmt.Printf("进程『%s』相关的可见窗口：\n", process)
	all, err := listWindows()
	if err != nil {
		return err
	}
	needle := strings.ToLower(process)
	found := false
	for _, window := range all {
		if !strings.Contains(strings.ToLower(window.process), needle) {
			continue
		}
		found = true
		fmt.Printf("  - %s 客户区 %s\n", window.describe(), window.frame.describe())
	}
	if !found {
		fmt.Println("  （没有找到，检查游戏是否已启动、窗口是否最小化）")
	}
	return nil
}

func windowPID(hwnd windows.HWND) uint32 {
	var pid uint32
	_, _ = windows.GetWindowThreadProcessId(hwnd, &pid)
	return pid
}

func isMinimized(hwnd windows.HWND) bool {
	value, _, _ := procIsIconic.Call(uintptr(hwnd))
	return value != 0
}

// windowTitle 读窗口标题；非 UTF-16 名称按替换字符处理，与 Rust 的 lossy 转换一致。
func windowTitle(hwnd windows.HWND) string {
	length, _, _ := procGetWindowTextLength.Call(uintptr(hwnd))
	if int32(length) <= 0 {
		return ""
	}
	limit := int32(length)
	if limit > maxTitleLen {
		limit = maxTitleLen
	}
	buffer := make([]uint16, limit+1)
	copied, _, _ := procGetWindowText.Call(
		uintptr(hwnd),
		uintptr(unsafe.Pointer(&buffer[0])),
		uintptr(len(buffer)),
	)
	if int32(copied) <= 0 {
		return ""
	}
	return windows.UTF16ToString(buffer[:copied])
}

// clientFrame 读客户区尺寸与屏幕坐标；尺寸为 0 时返回 false。
func clientFrame(hwnd windows.HWND) (rect, bool) {
	var client windows.Rect
	if value, _, _ := procGetClientRect.Call(uintptr(hwnd), uintptr(unsafe.Pointer(&client))); value == 0 {
		return rect{}, false
	}
	width := client.Right - client.Left
	height := client.Bottom - client.Top
	if width <= 0 || height <= 0 {
		return rect{}, false
	}
	var origin point
	if value, _, _ := procClientToScreen.Call(uintptr(hwnd), uintptr(unsafe.Pointer(&origin))); value == 0 {
		return rect{}, false
	}
	return rect{x: origin.x, y: origin.y, width: width, height: height}, true
}

// processName 取 exe 文件名主干（去掉目录与扩展名）；拿不到时返回空串。
func processName(pid uint32) string {
	if pid == 0 {
		return ""
	}
	handle, err := windows.OpenProcess(processQueryLimitedInformation, false, pid)
	if err != nil {
		return ""
	}
	defer func() {
		// 关闭句柄失败没有补救手段，忽略即可。
		_ = windows.CloseHandle(handle)
	}()

	buffer := make([]uint16, 1024)
	size := uint32(len(buffer))
	if err := windows.QueryFullProcessImageName(handle, 0, &buffer[0], &size); err != nil || size == 0 {
		return ""
	}
	path := windows.UTF16ToString(buffer[:size])
	fileName := path
	if cut := strings.LastIndexAny(path, `\/`); cut >= 0 {
		fileName = path[cut+1:]
	}
	if cut := strings.LastIndexByte(fileName, '.'); cut > 0 {
		return fileName[:cut]
	}
	return fileName
}
