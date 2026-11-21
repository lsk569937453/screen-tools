package main

import (
	"os"
	"path/filepath"
	"testing"
)

// golden 是从 Rust 版 `client` 抓下来的原始输出，用来锁死帮助文案。
func readGolden(t *testing.T, name string) string {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatalf("读取 golden %s 失败: %v", name, err)
	}
	// 仓库开 core.autocrlf 时这里会变回 CRLF，按字节比对前先归一化，避免检查出与内容无关的差异。
	return normalizeEOL(string(data))
}

func TestHelpLongGolden(t *testing.T) {
	_, action, parseErr := parseArgs("client.exe", []string{"--help"})
	if parseErr != nil {
		t.Fatalf("解析 --help 失败: %v", parseErr)
	}
	if action != actionHelpLong {
		t.Fatalf("--help 的动作应为 actionHelpLong，实际 %v", action)
	}
	if got, want := helpTextFor(helpLongText, "client.exe"), readGolden(t, "help_long.txt"); got != want {
		t.Fatalf("长帮助与 Rust 版不一致（got %d 字节，want %d 字节）", len(got), len(want))
	}
}

func TestHelpShortGolden(t *testing.T) {
	_, action, parseErr := parseArgs("client.exe", []string{"-h"})
	if parseErr != nil {
		t.Fatalf("解析 -h 失败: %v", parseErr)
	}
	if action != actionHelpShort {
		t.Fatalf("-h 的动作应为 actionHelpShort，实际 %v", action)
	}
	if got, want := helpTextFor(helpShortText, "client.exe"), readGolden(t, "help_short.txt"); got != want {
		t.Fatalf("短帮助与 Rust 版不一致（got %d 字节，want %d 字节）", len(got), len(want))
	}
}

// `client help` 走 print_long_help()：Usage 用命令名常量，末尾多一个空行。
func TestHelpPositionalGolden(t *testing.T) {
	opts, action, parseErr := parseArgs("client.exe", []string{"help"})
	if parseErr != nil {
		t.Fatalf("解析 help 失败: %v", parseErr)
	}
	if action != actionRun {
		t.Fatalf("help 位置参数不应短路解析，实际动作 %v", action)
	}
	if !opts.wantsHelp() {
		t.Fatalf("help 位置参数应触发长帮助")
	}
	got := helpTextFor(helpLongText, programName) + "\n"
	if want := readGolden(t, "help_positional.txt"); got != want {
		t.Fatalf("help 位置参数输出与 Rust 版不一致（got %d 字节，want %d 字节）", len(got), len(want))
	}
}

// help 的位置参数大小写不敏感。
func TestWantsHelpCaseInsensitive(t *testing.T) {
	for _, value := range []string{"help", "HELP", "Help"} {
		opts, _, parseErr := parseArgs("client.exe", []string{value})
		if parseErr != nil {
			t.Fatalf("解析 %q 失败: %v", value, parseErr)
		}
		if !opts.wantsHelp() {
			t.Fatalf("%q 应触发长帮助", value)
		}
	}
}

func TestParseErrorsMatchClap(t *testing.T) {
	cases := []struct {
		name string
		args []string
		want string
	}{
		{
			name: "未知参数",
			args: []string{"--bogus"},
			want: "error: unexpected argument '--bogus' found\n\n" +
				"  tip: to pass '--bogus' as a value, use '-- --bogus'\n\n" +
				"Usage: client.exe [OPTIONS] [FILE]\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "帧尺寸超范围",
			args: []string{"--frame", "10x10"},
			want: "error: invalid value '10x10' for '--frame <WxH|auto>': " +
				"--frame 超出范围（宽 320..=8192、高 240..=8192）；收到 10x10\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "帧尺寸缺 x",
			args: []string{"--frame", "abc"},
			want: "error: invalid value 'abc' for '--frame <WxH|auto>': " +
				"--frame 需要 WxH 或 auto，例如 2505x1329；收到 'abc'\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "帧尺寸高度非数字",
			args: []string{"--frame", "100x10x"},
			want: "error: invalid value '100x10x' for '--frame <WxH|auto>': " +
				"--frame 高度不是数字: '10x'\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "缺值",
			args: []string{"--frame"},
			want: "error: a value is required for '--frame <WxH|auto>' but none was supplied\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "重复参数",
			args: []string{"--port", "1", "--port", "2"},
			want: "error: the argument '--port <N>' cannot be used multiple times\n\n" +
				"Usage: client.exe [OPTIONS] [FILE]\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "布尔参数带值",
			args: []string{"--probe=true"},
			want: "error: unexpected value 'true' for '--probe' found; no more were expected\n\n" +
				"Usage: client.exe --probe [FILE]\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "奇偶校验超范围",
			args: []string{"--parity", "2"},
			want: "error: invalid value '2' for '--parity <F>': " +
				"--parity 必须在 0.0..=1.0 之间，收到 2\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "端口超范围",
			args: []string{"--port", "70000"},
			want: "error: invalid value '70000' for '--port <N>': 70000 is not in 0..=65535\n\n" +
				"For more information, try '--help'.\n",
		},
		{
			name: "多余位置参数",
			args: []string{"a", "b"},
			want: "error: unexpected argument 'b' found\n\n" +
				"Usage: client.exe [OPTIONS] [FILE]\n\n" +
				"For more information, try '--help'.\n",
		},
	}

	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			_, _, parseErr := parseArgs("client.exe", test.args)
			if parseErr == nil {
				t.Fatalf("%v 应解析失败", test.args)
			}
			if parseErr.body != test.want {
				t.Fatalf("错误输出不一致\n--- got ---\n%s\n--- want ---\n%s", parseErr.body, test.want)
			}
		})
	}
}

func TestParseDefaults(t *testing.T) {
	opts, action, parseErr := parseArgs("client.exe", []string{"data.bin"})
	if parseErr != nil {
		t.Fatalf("解析失败: %v", parseErr)
	}
	if action != actionRun {
		t.Fatalf("动作应为 actionRun，实际 %v", action)
	}
	if opts.file != "data.bin" || !opts.hasFile {
		t.Fatalf("文件参数不对: %+v", opts)
	}
	if opts.modulePx != defaultModulePx || opts.blockBytes != defaultBlockBytes {
		t.Fatalf("默认版式参数不对: %+v", opts)
	}
	if opts.parity != defaultParity || opts.dwellMs != defaultDwellMs {
		t.Fatalf("默认播放参数不对: %+v", opts)
	}
	if opts.port != defaultPort || opts.process != defaultProcess {
		t.Fatalf("默认端口/进程不对: %+v", opts)
	}
	if opts.probe || opts.verbose || opts.hasExport {
		t.Fatalf("默认开关不对: %+v", opts)
	}
	if spec := opts.resolvedFrame(); spec.auto || spec.width != defaultFrameWidth || spec.height != defaultFrameHeight {
		t.Fatalf("默认帧尺寸不对: %+v", spec)
	}
}

func TestExportNoValueDefaults(t *testing.T) {
	opts, _, parseErr := parseArgs("client.exe", []string{"data.bin", "--export"})
	if parseErr != nil {
		t.Fatalf("解析失败: %v", parseErr)
	}
	if !opts.hasExport || opts.export != defaultExportDir {
		t.Fatalf("--export 省略值时应为 %q，实际 %+v", defaultExportDir, opts)
	}
}

func TestFrameSpecVariants(t *testing.T) {
	cases := []struct {
		value  string
		auto   bool
		width  uint32
		height uint32
	}{
		{"auto", true, 0, 0},
		{"AUTO", true, 0, 0},
		{" 800x600 ", false, 800, 600},
		{"800X600", false, 800, 600},
	}
	for _, test := range cases {
		spec, err := parseFrameSpec(test.value)
		if err != nil {
			t.Fatalf("%q 应解析成功: %v", test.value, err)
		}
		if spec.auto != test.auto || spec.width != test.width || spec.height != test.height {
			t.Fatalf("%q 解析结果不对: %+v", test.value, spec)
		}
	}
}

func TestResolveFrameFlags(t *testing.T) {
	// 隐藏的 --auto 优先于 --frame。
	opts, _, parseErr := parseArgs("client.exe", []string{"data.bin", "--frame", "800x600", "--auto"})
	if parseErr != nil {
		t.Fatalf("解析失败: %v", parseErr)
	}
	if spec := opts.resolvedFrame(); !spec.auto {
		t.Fatalf("--auto 应优先于 --frame，实际 %+v", spec)
	}
}
