# client-go - ScreenCode 发送端的 Go 复刻版

Rust 版 `client` 的 Go 复刻实现，用于在 **Windows 7** 上运行发送端。
码制（ScreenCode v1）与 Rust 版完全一致：同一份文件、同样的参数，两边生成的帧**逐像素相同**，
接收端（Rust 版 `server`）与离线工具 `verify_frames` 都能正常识别。

## 平台要求

| 用途 | 要求 |
|---|---|
| 编译 | Go **1.20.x**（1.21 起官方要求 Windows 10+，编出来的程序无法在 Win7 运行） |
| 运行 | Windows 7 SP1 及以上（64 位）；纯 Go 实现，`CGO_ENABLED=0`，无运行时依赖 |
| 依赖 | 只有 `golang.org/x/sys/windows`（窗口探测用），没有第三方 CLI/日志库 |

在 Windows 11 上编译、产物拷到 Windows 7 上运行是受支持的方式：

```bat
REM 把 Go 1.20.x 的 bin 目录加进 PATH 后执行
build-win7.bat
```

等价的手工命令：

```bash
CGO_ENABLED=0 GOOS=windows GOARCH=amd64 \
  go build -trimpath -ldflags "-s -w" -o client-go.exe .
```

## 用法

命令行与 Rust 版逐字节一致（帮助文案、默认值、错误信息、退出码都对齐）：

```bash
# 查看 HSR 窗口客户区尺寸
client-go.exe --probe

# 生成并启动播放服务（默认端口 9090）
client-go.exe <文件路径> --frame auto

# 只导出 PNG 帧
client-go.exe <文件路径> --export qr_output --frame 2505x1329
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `FILE` | 无 | 位置参数；值 `help` 等价 `--help`（大小写不敏感） |
| `--frame WxH` / `--frame auto` | 1920x1080 | 目标帧尺寸；`auto` 读 `--process` 窗口客户区 |
| `--module-px N` | 2 | 每模块像素 |
| `--block-bytes N` | 4096 | 分片净荷 |
| `--parity F` | 0.10 | 纠删码冗余比例 |
| `--dwell-ms N` | 300 | 每帧停留毫秒 |
| `--port N` | 9090 | HTTP 端口 |
| `--process NAME` | HSR | `--frame auto` / `--probe` 匹配的进程名 |
| `--export [DIR]` | 省略值 = `qr_output` | 只导出帧，不起服务 |
| `--probe` | 关 | 列出可见窗口客户区 |
| `-v, --verbose` | 关 | 控制台也输出 DEBUG 细节 |

日志写在 `logs/client-<YYYYMMDD-HHMMSS>.log`（与 Rust 版同格式、同目录），文件里始终记 DEBUG，
控制台默认只输出 INFO。HTTP 端点同样是 `/api/info`、`/api/frame/:n`、`/`。

## 与 Rust 版的已知差异

这些差异不影响接收端识别，已在实现时逐条确认：

1. **PNG 文件字节不同**：两边都是灰度 + 位深 1 + 每行 filter 0 的合法 PNG，解码后像素完全一致；
   Go 的 deflate 压缩率更高（800x600 的密集帧约 15 KB，Rust 约 60 KB）。接收端与 `verify_frames`
   都是解码后按位读，因此不影响识别。
2. **CLI 解析是手写的 clap 兼容层**：帮助/版本/校验错误/退出码已用 65 个用例（含各分组帮助、`help`
   位置参数、未知参数、缺值、重复、非法值、`--export` 可选值、`--probe`、导出流程与 `-v`）与 Rust 版
   逐字节对拍通过；未覆盖的极端输入（例如非 UTF-8 命令行参数）未做验证。
3. 可执行文件名是 `client-go.exe`；`--version` 输出仍是 `client 0.1.0`。

## 验证

```bash
# 单元测试（不需要 Rust 工具链）
go vet ./... && go test ./...

# 与 Rust 版对拍：逐帧像素一致 + verify_frames 能还原源文件
SCREENCODE_INTEROP=1 go test -run TestInteropWithRust -v .

# 手工对拍（同一份文件、同样参数）
client.exe     <文件> --export /tmp/rust-frames --frame 800x600 --parity 0.5
client-go.exe  <文件> --export /tmp/go-frames   --frame 800x600 --parity 0.5
cargo run -p screencode --example verify_frames -- /tmp/go-frames
```

`interop_test.go` 默认跳过（`SCREENCODE_INTEROP=1` 才运行），没有 `cargo` 时自动 Skip。

## 代码结构

| 文件 | 职责 |
|---|---|
| `main.go` | 参数 → 日志 → 读取文件 → 编码 → 导出或起服务 |
| `cli.go` | clap 兼容的参数解析与错误文案 |
| `clitext.go` | 帮助正文（由 Rust 版输出抽取，勿手工编辑） |
| `logger.go` | `logs/client-<时间戳>.log` 与控制台双写 |
| `player.go` | 持有分片，按帧号渲染 PNG（4 帧缓存） |
| `export.go` | 导出 `frame_00001.png` |
| `server.go` | `/api/info`、`/api/frame/:n`、内嵌播放页 |
| `window_windows.go` | `--probe` 与 `--frame auto` 的窗口探测（Win32） |
| `window_other.go` | 非 Windows 平台的显式报错 |
| `screencode/` | 码制共享库：版式、帧头、slot、GF(256)、纠删、1-bit PNG 渲染 |

`screencode` 里的 FEC 是按 Rust `reed-solomon-erasure 6.0.0` 的矩阵算法重写的：GF(256) 多项式 `0x11D`，
Vandermonde 矩阵后左乘单位阵得到系统码。不能用 `klauspost/reedsolomon` 的默认矩阵，
否则校验分片字节不同，接收端丢数据分片时将无法恢复。
