package main

import (
	"fmt"
	"strconv"
	"strings"
)

// 与 Rust 侧 `client/src/cli.rs` 保持一致的默认值与名称。
const (
	programName    = "client"
	programVersion = "0.1.0"

	defaultFrameWidth  = 1920
	defaultFrameHeight = 1080
	defaultModulePx    = 2
	defaultBlockBytes  = 4096
	defaultParity      = 0.10
	defaultDwellMs     = 300
	defaultPort        = 9090
	defaultProcess     = "HSR"
	defaultExportDir   = "qr_output"

	minFrameWidth  = 320
	maxFrameWidth  = 8192
	minFrameHeight = 240
	maxFrameHeight = 8192
)

// frameSpec 是 `--frame` 的目标尺寸；auto 表示读 `--process` 窗口客户区。
type frameSpec struct {
	auto   bool
	width  uint32
	height uint32
}

func defaultFrameSpec() frameSpec {
	return frameSpec{width: defaultFrameWidth, height: defaultFrameHeight}
}

// cli 是解析后的命令行参数，字段含义与 Rust 侧 `Cli` 一一对应。
type cli struct {
	file       string
	hasFile    bool
	frame      frameSpec
	auto       bool
	modulePx   uint32
	blockBytes int
	parity     float64
	dwellMs    uint64
	port       uint16
	process    string
	export     string
	hasExport  bool
	probe      bool
	verbose    bool
}

// wantsHelp 对应 `client help`（大小写不敏感）等价于 `--help`。
func (c cli) wantsHelp() bool {
	return c.hasFile && strings.EqualFold(c.file, "help")
}

// resolvedFrame 处理隐藏的 `--auto` 兼容写法（优先级高于 `--frame`）。
func (c cli) resolvedFrame() frameSpec {
	if c.auto {
		return frameSpec{auto: true}
	}
	return c.frame
}

// cliAction 表示解析阶段就决定好、不需要进入主流程的动作。
type cliAction int

const (
	actionRun       cliAction = iota
	actionHelpLong            // --help
	actionHelpShort           // -h
	actionVersion             // -V / --version
)

// cliError 是 clap 风格的参数错误：正文已经排好版，交给 stderr 并退出码 2。
type cliError struct {
	body string
}

func (e *cliError) Error() string { return e.body }

func errUnexpectedArgument(bin, token string) *cliError {
	var builder strings.Builder
	fmt.Fprintf(&builder, "error: unexpected argument '%s' found\n\n", token)
	if strings.HasPrefix(token, "-") {
		fmt.Fprintf(&builder, "  tip: to pass '%s' as a value, use '-- %s'\n\n", token, token)
	}
	fmt.Fprintf(&builder, "Usage: %s [OPTIONS] [FILE]\n\nFor more information, try '--help'.\n", bin)
	return &cliError{body: builder.String()}
}

func errInvalidValue(display, value, reason string) *cliError {
	return &cliError{body: fmt.Sprintf(
		"error: invalid value '%s' for '%s': %s\n\nFor more information, try '--help'.\n",
		value, display, reason,
	)}
}

func errValueRequired(display string) *cliError {
	return &cliError{body: fmt.Sprintf(
		"error: a value is required for '%s' but none was supplied\n\nFor more information, try '--help'.\n",
		display,
	)}
}

func errDuplicate(bin, display string) *cliError {
	return &cliError{body: fmt.Sprintf(
		"error: the argument '%s' cannot be used multiple times\n\nUsage: %s [OPTIONS] [FILE]\n\nFor more information, try '--help'.\n",
		display, bin,
	)}
}

func errUnexpectedValue(bin, display, value string) *cliError {
	return &cliError{body: fmt.Sprintf(
		"error: unexpected value '%s' for '%s' found; no more were expected\n\nUsage: %s %s [FILE]\n\nFor more information, try '--help'.\n",
		value, display, bin, display,
	)}
}

// 各选项在错误信息里的展示形式（与 clap 的 value_name 一致）。
const (
	displayFrame      = "--frame <WxH|auto>"
	displayModulePx   = "--module-px <N>"
	displayBlockBytes = "--block-bytes <N>"
	displayParity     = "--parity <F>"
	displayDwellMs    = "--dwell-ms <N>"
	displayPort       = "--port <N>"
	displayProcess    = "--process <NAME>"
	displayExport     = "--export [<DIR>]"
	displayProbe      = "--probe"
	displayVerbose    = "--verbose"
	displayAuto       = "--auto"
)

// longFlags 列出所有长选项及其是否必须带值。
var longFlags = map[string]bool{
	"frame":       true,
	"module-px":   true,
	"block-bytes": true,
	"parity":      true,
	"dwell-ms":    true,
	"port":        true,
	"process":     true,
	"export":      false,
	"probe":       false,
	"verbose":     false,
	"auto":        false,
	"help":        false,
	"version":     false,
}

// displayOf 返回某个长选项在错误信息里的展示形式。
func displayOf(name string) string {
	switch name {
	case "frame":
		return displayFrame
	case "module-px":
		return displayModulePx
	case "block-bytes":
		return displayBlockBytes
	case "parity":
		return displayParity
	case "dwell-ms":
		return displayDwellMs
	case "port":
		return displayPort
	case "process":
		return displayProcess
	case "export":
		return displayExport
	case "probe":
		return displayProbe
	case "verbose":
		return displayVerbose
	case "auto":
		return displayAuto
	default:
		return "--" + name
	}
}

// isKnownFlagToken 判断一个以 '-' 开头的词是否是我们认识的选项（clap 用它区分
// 「缺值」和「未知参数」两种错误）。
func isKnownFlagToken(token string) bool {
	if token == "--" {
		return true
	}
	if strings.HasPrefix(token, "--") {
		name := strings.TrimPrefix(token, "--")
		if cut := strings.IndexByte(name, '='); cut >= 0 {
			name = name[:cut]
		}
		_, ok := longFlags[name]
		return ok
	}
	for _, char := range token[1:] {
		if char != 'h' && char != 'V' && char != 'v' {
			return false
		}
	}
	return len(token) > 1
}

// parseArgs 按 clap 的规则扫描命令行：从左到右，遇到第一个问题就报错。
func parseArgs(bin string, args []string) (cli, cliAction, *cliError) {
	opts := cli{
		frame:      defaultFrameSpec(),
		modulePx:   defaultModulePx,
		blockBytes: defaultBlockBytes,
		parity:     defaultParity,
		dwellMs:    defaultDwellMs,
		port:       defaultPort,
		process:    defaultProcess,
	}
	seen := make(map[string]bool)
	onlyPositionals := false

	for i := 0; i < len(args); i++ {
		token := args[i]

		switch {
		case onlyPositionals:
			if opts.hasFile {
				return opts, actionRun, errUnexpectedArgument(bin, token)
			}
			opts.file, opts.hasFile = token, true

		case token == "--":
			onlyPositionals = true

		case strings.HasPrefix(token, "--"):
			name, value, hasValue := strings.Cut(strings.TrimPrefix(token, "--"), "=")
			if _, known := longFlags[name]; !known {
				return opts, actionRun, errUnexpectedArgument(bin, token)
			}
			switch name {
			case "help", "version":
				// clap 同样先拒绝给这些开关传值，再考虑是否短路输出。
				if hasValue {
					return opts, actionRun, errUnexpectedValue(bin, displayOf(name), value)
				}
				if name == "help" {
					return opts, actionHelpLong, nil
				}
				return opts, actionVersion, nil
			}
			if seen[name] {
				return opts, actionRun, errDuplicate(bin, displayOf(name))
			}
			seen[name] = true

			// --export 的值是可选的（对应 clap 的 num_args=0..=1 + default_missing_value），
			// 必须在「只带开关」的分支之前处理。
			if name == "export" {
				var dir string
				switch {
				case hasValue:
					// 允许空串，与 clap 解析 `--export=` 得到 Some("") 一致。
					dir = value
				case i+1 < len(args) && !strings.HasPrefix(args[i+1], "-"):
					dir = args[i+1]
					i++
				default:
					dir = defaultExportDir
				}
				opts.export, opts.hasExport = dir, true
				continue
			}

			if !longFlags[name] {
				if hasValue {
					return opts, actionRun, errUnexpectedValue(bin, displayOf(name), value)
				}
				switch name {
				case "probe":
					opts.probe = true
				case "verbose":
					opts.verbose = true
				case "auto":
					opts.auto = true
				}
				continue
			}

			if !hasValue {
				next := i + 1
				if next >= len(args) {
					return opts, actionRun, errValueRequired(displayOf(name))
				}
				if strings.HasPrefix(args[next], "-") {
					if isKnownFlagToken(args[next]) {
						return opts, actionRun, errValueRequired(displayOf(name))
					}
					return opts, actionRun, errUnexpectedArgument(bin, args[next])
				}
				value = args[next]
				i++
			}
			if err := applyValue(&opts, name, value); err != nil {
				return opts, actionRun, err
			}

		case strings.HasPrefix(token, "-") && len(token) > 1:
			for _, char := range token[1:] {
				switch char {
				case 'h':
					return opts, actionHelpShort, nil
				case 'V':
					return opts, actionVersion, nil
				case 'v':
					if seen["verbose"] {
						return opts, actionRun, errDuplicate(bin, displayVerbose)
					}
					seen["verbose"] = true
					opts.verbose = true
				default:
					return opts, actionRun, errUnexpectedArgument(bin, "-"+string(char))
				}
			}

		default:
			if opts.hasFile {
				return opts, actionRun, errUnexpectedArgument(bin, token)
			}
			opts.file, opts.hasFile = token, true
		}
	}
	return opts, actionRun, nil
}

// applyValue 校验并写入一个带值的选项，错误信息与 Rust 侧逐字一致。
func applyValue(opts *cli, name, value string) *cliError {
	switch name {
	case "frame":
		spec, err := parseFrameSpec(value)
		if err != nil {
			return errInvalidValue(displayFrame, value, err.Error())
		}
		opts.frame = spec
	case "module-px":
		parsed, err := parsePositiveUint(value)
		if err != nil {
			return errInvalidValue(displayModulePx, value, err.Error())
		}
		if parsed > uint64(^uint32(0)) {
			return errInvalidValue(displayModulePx, value, "number too large to fit in target type")
		}
		opts.modulePx = uint32(parsed)
	case "block-bytes":
		parsed, err := parsePositiveUint(value)
		if err != nil {
			return errInvalidValue(displayBlockBytes, value, err.Error())
		}
		opts.blockBytes = int(parsed)
	case "parity":
		parsed, err := parseParity(value)
		if err != nil {
			return errInvalidValue(displayParity, value, err.Error())
		}
		opts.parity = parsed
	case "dwell-ms":
		parsed, err := strconv.ParseUint(strings.TrimSpace(value), 10, 64)
		if err != nil {
			return errInvalidValue(displayDwellMs, value, signedParseError(err))
		}
		opts.dwellMs = parsed
	case "port":
		parsed, err := parsePort(value)
		if err != nil {
			return errInvalidValue(displayPort, value, err.Error())
		}
		opts.port = parsed
	case "process":
		if strings.TrimSpace(value) == "" {
			return errValueRequired(displayProcess)
		}
		opts.process = value
	}
	return nil
}

// signedParseError 把 Go 的整数解析错误映射成 Rust 标准库的措辞。
func signedParseError(err error) string {
	numberError, ok := err.(*strconv.NumError)
	if ok && numberError.Err == strconv.ErrRange {
		return "number too large to fit in target type"
	}
	return "invalid digit found in string"
}

func parsePort(value string) (uint16, error) {
	trimmed := strings.TrimSpace(value)
	if trimmed == "" {
		return 0, fmt.Errorf("cannot parse integer from empty string")
	}
	parsed, err := strconv.ParseUint(trimmed, 10, 64)
	if err != nil {
		return 0, fmt.Errorf("%s", signedParseError(err))
	}
	if parsed > 65535 {
		return 0, fmt.Errorf("%s is not in 0..=65535", trimmed)
	}
	return uint16(parsed), nil
}

func parsePositiveUint(value string) (uint64, error) {
	parsed, err := strconv.ParseUint(strings.TrimSpace(value), 10, 64)
	if err != nil {
		return 0, fmt.Errorf("需要正整数，收到 '%s'", value)
	}
	if parsed == 0 {
		return 0, fmt.Errorf("必须 >= 1，收到 '%s'", value)
	}
	return parsed, nil
}

func parseParity(value string) (float64, error) {
	parsed, err := strconv.ParseFloat(strings.TrimSpace(value), 64)
	if err != nil {
		return 0, fmt.Errorf("--parity 需要数字，收到 '%s'", value)
	}
	if parsed < 0 || parsed > 1 {
		return 0, fmt.Errorf("--parity 必须在 0.0..=1.0 之间，收到 %s", value)
	}
	return parsed, nil
}

// parseFrameSpec 解析 `WxH` / `WXH` / `auto`。
func parseFrameSpec(value string) (frameSpec, error) {
	if strings.EqualFold(value, "auto") {
		return frameSpec{auto: true}, nil
	}
	widthText, heightText, ok := strings.Cut(value, "x")
	if !ok {
		widthText, heightText, ok = strings.Cut(value, "X")
	}
	if !ok {
		return frameSpec{}, fmt.Errorf("--frame 需要 WxH 或 auto，例如 2505x1329；收到 '%s'", value)
	}
	width, err := strconv.ParseUint(strings.TrimSpace(widthText), 10, 32)
	if err != nil {
		return frameSpec{}, fmt.Errorf("--frame 宽度不是数字: '%s'", strings.TrimSpace(widthText))
	}
	height, err := strconv.ParseUint(strings.TrimSpace(heightText), 10, 32)
	if err != nil {
		return frameSpec{}, fmt.Errorf("--frame 高度不是数字: '%s'", strings.TrimSpace(heightText))
	}
	if width < minFrameWidth || width > maxFrameWidth || height < minFrameHeight || height > maxFrameHeight {
		return frameSpec{}, fmt.Errorf(
			"--frame 超出范围（宽 320..=8192、高 240..=8192）；收到 %dx%d", width, height)
	}
	return frameSpec{width: uint32(width), height: uint32(height)}, nil
}

// helpTextFor 把帮助模板里的 Usage 行换成实际程序名。
func helpTextFor(template, bin string) string {
	return strings.Replace(template, "Usage: %s", "Usage: "+bin, 1)
}
