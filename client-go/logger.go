package main

import (
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

// 日志级别，宽度与 tracing 的五字符对齐列一致。
const (
	levelDebug = "DEBUG"
	levelInfo  = "INFO"
	levelWarn  = "WARN"
	levelError = "ERROR"
)

// timeLayout 是控制台与日志文件共用的时间格式（本机时区，毫秒三位）。
const timeLayout = "2006-01-02 15:04:05.000"

// logger 复刻 common::logging 的行为：
//   - 控制台（stdout）默认 INFO，`-v` 时也输出 DEBUG；
//   - 日志文件始终记录 DEBUG；
//   - 行格式 `<时间> <级别宽 5 右对齐> <消息>`。
type logger struct {
	mu       sync.Mutex
	verbose  bool
	file     *os.File
	filePath string
}

// newLogger 在 dir 下创建 `程序名-YYYYMMDD-HHMMSS.log`。
// 文件不可用时返回一个仅输出到控制台的 logger 以及具体原因，调用方降级并告警。
func newLogger(program string, verbose bool, dir string) (*logger, error) {
	log := &logger{verbose: verbose}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return log, err
	}
	name := fmt.Sprintf("%s-%s.log", program, time.Now().Format("20060102-150405"))
	path := filepath.Join(dir, name)
	// 追加写：同一秒内重复启动时与 tracing_appender 一致地续写同一个文件。
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
	if err != nil {
		return log, err
	}
	log.file = file
	log.filePath = path
	return log, nil
}

// close 关闭日志文件，进程退出前调用。
func (l *logger) close() {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.file == nil {
		return
	}
	// 关闭失败只在退出阶段发生，没有可用的上报通道。
	_ = l.file.Close()
	l.file = nil
}

func (l *logger) Debugf(format string, args ...any) { l.write(levelDebug, format, args...) }
func (l *logger) Infof(format string, args ...any)  { l.write(levelInfo, format, args...) }
func (l *logger) Warnf(format string, args ...any)  { l.write(levelWarn, format, args...) }
func (l *logger) Errorf(format string, args ...any) { l.write(levelError, format, args...) }

func (l *logger) write(level, format string, args ...any) {
	if l == nil {
		return
	}
	line := fmt.Sprintf("%s %5s %s\n", time.Now().Format(timeLayout), level, fmt.Sprintf(format, args...))

	l.mu.Lock()
	defer l.mu.Unlock()
	if l.file != nil {
		// 日志写入失败不影响业务流程，也不允许 panic。
		_, _ = l.file.WriteString(line)
	}
	if level != levelDebug || l.verbose {
		_, _ = os.Stdout.WriteString(line)
	}
}
