package main

import "fmt"

// formatBytes 把字节数格式化成 `123 B` / `1.23 KB` / `4.56 MB`（与 common::format::format_bytes 一致）。
func formatBytes(bytes uint64) string {
	const (
		kb = 1024
		mb = 1024 * 1024
	)
	switch {
	case bytes >= mb:
		return fmt.Sprintf("%.2f MB", float64(bytes)/float64(mb))
	case bytes >= kb:
		return fmt.Sprintf("%.2f KB", float64(bytes)/float64(kb))
	default:
		return fmt.Sprintf("%d B", bytes)
	}
}
