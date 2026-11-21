# AGENTS.md — screen-tools

## 1. 本仓库是什么

- Rust workspace（edition 2021），四个 crate：
  - `screencode`：码制共享库。版式几何、帧头、分片、纠删、渲染、识别、Windows 抓窗的唯一实现。
  - `client`：发送端。读文件 → SHA-256 → 分片 + 纠删 → 规划版式 → 1-bit PNG 帧 → axum 播放服务；播放页 `client/web/index.html` 由 rust-embed 内嵌。
  - `server`：接收端。后台截取目标窗口客户区 → 定位黑边框 → 读帧头 → 按模块坐标采样 → 分片落盘 → 纠删恢复 → SHA-256 校验 → 写文件。
  - `common`：两端共享的应用层工具（tracing 日志初始化、本机时间戳、SHA-256/hex、体积格式化），不含协议知识。
- `cargo build --release` 产出 `target/release/client.exe`、`target/release/server.exe`。
- 码制契约只写在 `docs/screencode-v1.md` 与 `screencode`；本文件不重复字段表。
- 数据流：
  - 发送：`client/src/main.rs` → `screencode::{fec,layout,header,shard,render}` → `axum`（`/api/info`、`/api/frame/:n` + 内嵌页面）。
  - 接收：`server/src/main.rs`（`Session`）→ `screencode::detect::decode_frame` → `server/src/store.rs`（`ShardStore`）→ `finish`（RS 恢复 + SHA-256）→ 写 `received_file_<时间戳>`。
- 平台边界：`screencode::win` 与窗口截图路径只在 `cfg(windows)` 下编译；其它平台只能 `server --fullscreen`。改这两处必须保证非 Windows 仍能 `cargo check`。
- `client-go/`：发送端的 Go 复刻版（独立 Go module，`module screentools/client-go`），码制与 Rust 版逐字节一致，专门用于 Windows 7（必须用 Go **1.20.x** 构建，1.21+ 要求 Win10+）。改码制时必须同步改 `client-go/screencode/`，并跑跨实现对拍。
- 动手前先读 `README.md`（用法/容量/排查）和 `docs/screencode-v1.md`（字节布局）。

## 2. 质量门禁

三关全过才算改完：

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- clippy **零 warning**：`-D warnings` 已写进门禁，修 warning 属于改动范围内的工作，不接受"只是警告"。
- Go 版发送端的门禁（在 `client-go/` 下）：`go vet ./...`、`go test ./...`；改了 `client-go/screencode/` 或渲染逻辑还要跑 `SCREENCODE_INTEROP=1 go test -run TestInteropWithRust -v .`。
- 测试布局：单元测试内联 `#[cfg(test)]`；集成测试放所在 crate 的 `tests/`（目前 `screencode/tests/`、`common/tests/`）；不新建测试 crate。
- 改动码制、渲染或识别时，额外跑离线 e2e（不依赖游戏窗口）：

```sh
cargo run -p client -- ./test.bin --export tmp-e2e/frames --frame 800x600
cargo run -p screencode --example verify_frames -- tmp-e2e/frames
```

  `verify_frames` 会逐帧打印 CRC 失败数并输出最终 SHA-256，必须与源文件一致。
- 交付前自查 `git diff`：无越界改动、无调试残留、无注释掉的代码、无临时文件。

## 3. 依赖策略：有现成 crate 就不许手搓

- 通用能力优先用成熟 crate；新增工具函数前先查 crates.io 与现有依赖树，能复用就复用。
- 现有能力归属（新增代码按此选型，禁止引入第二套同类实现）：

| 能力 | crate | 版本 |
|---|---|---|
| 命令行解析 | `clap`（derive） | =4.6.7 |
| 日志 | `tracing` / `tracing-subscriber` / `tracing-appender` | =0.1.44 / =0.3.23 / =0.2.5 |
| 本机时间格式 | `chrono` | =0.4.45 |
| hex 编解码 | `hex` | =0.4.3 |
| 哈希 / CRC | `sha2` / `crc32fast` | =0.10.9 / =1.5.5 |
| 纠删码 | `reed-solomon-erasure` | =6.0.0 |
| PNG | `png` | =0.17.16 |
| HTTP / 内嵌资源 | `axum` / `axum-embed` / `rust-embed` | =0.7.9 / =0.1.0 / =8.13.0 |
| 截屏 / 窗口 | `screenshots` / `windows-sys` | =0.7.3 / =0.59.0 |
| 错误 / 序列化 / 异步 | `anyhow` / `serde` / `serde_json` / `tokio` | =1.0.104 / =1.0.229 / =1.0.154 / =1.54.1 |

- 新增或升级依赖：必须在 `[workspace.dependencies]` 声明，版本写 `=x.y.z`（禁止 `^`、`~`、裸版本），并说明为什么现有依赖不够、考虑过哪些替代、代价是什么。
- 允许的例外：10 行以内的纯展示格式化（如 `common::format::format_bytes`）可以自写，但只能有一份实现，放 `common`。

## 4. 代码规范

- **语言**：注释、日志、错误消息、UI 文案、commit subject 用中文，与 `README.md`、`docs/` 一致；标识符、crate 名、类型名用英文。
- **禁止 `unwrap()` / `expect()`**：业务代码用 `?`、`context`、`ok_or_else`、`checked_*`、默认值兜底；测试代码不受限。外部输入禁止走会 panic 的 `Duration::from_secs_f64`，改用 `try_from_secs_f64`。
- **规模**：单函数 ≤200 行（不计空行与纯注释行），单文件 ≤1500 行。超限文件不再堆逻辑，先拆模块。
- **模块布局**：二进制入口保留在 `main.rs`（参数 → 初始化 → 运行 → 收尾），CLI 必须放 `cli.rs`（只用 clap derive）；可复用或需要独立测试的逻辑放具名模块（现有 `store.rs`、`common`）。新增逻辑不要继续堆进 `main.rs`，优先拆模块。本仓库不新建 `mod.rs`；若必须用，只放子模块声明与 `pub use`。
- **错误**：库层（`screencode`）用具体错误类型/`Option` 表达可恢复失败，应用层用 `anyhow::Context` 补充路径与参数；不支持的能力必须显式报错，禁止静默失败或假装成功。
- **unsafe**：只允许出现在 `screencode/src/win.rs` 的 Windows FFI；每个 `unsafe` 块写 `// SAFETY:` 说明前提，每个 `unsafe fn` 写 `# Safety` 文档。其它地方出现 `unsafe` 视为设计错误。
- **`#[allow(...)]`**：能不写就不写；必须写时在同一行或上一行给出理由，禁止整文件 blanket allow。
- **库的边界**：`screencode` 是纯码制库，不得依赖 `clap`/`tracing`/`axum` 等应用层 crate，也不得向 stdout 打印。

## 5. CLI 与日志约定

- 参数只允许用 `clap` derive 定义，禁止手写 `env::args` 匹配循环；兼容别名（`client help`、`server help`）通过参数归一化后交给 clap。
- 帮助文本放 `about` / `long_about` / `after_help`，用 `help_heading` 分组；示例必须覆盖 README 里的主用法。非法输入在 clap 层以 exit code 2 拒绝，禁止静默纠正。
- 日志只用 `tracing`，初始化在 `common::logging::init`：
  - `info!`：关键事件，控制台与文件都记录；
  - `debug!`：每帧/每次请求的细节，`-v` 时才进控制台，文件始终记录；
  - `warn!` / `error!`：异常与降级。
- `println!` 只用于展示性输出：表格/清单（`--probe`、`--list-windows`）、装饰横幅、交互式进度（`\r`），以及给用户直接复制的最终结果行（播放地址、输出文件路径）。
- 日志文件：`logs/<client|server>-<YYYYMMDD-HHMMSS>.log`（本机时区），两个二进制都写；文件不可写时降级为仅控制台并 `warn!`，不得 panic。
- 时间格式化用 `chrono`，单调计时用 `std::time::Instant`；禁止手算公历或时区。

## 6. 码制不变量（`screencode`）

- 生成端与识别端必须共用同一份 `Geometry` / `Header` / `encode_slot` / `decode_frame`；禁止任一侧另起一套换算。
- 改 layout / header / slot / fec 的任何一个字节，必须同时：更新 `docs/screencode-v1.md`、评估 `FORMAT_VERSION`、跑 `cargo test` 与离线 e2e，并说明兼容性。
- 固定约束：帧头 40 字节、3 份副本、CRC 覆盖 0..34；`hash_prefix` = SHA-256 前 8 字节；slot = `idx(4B) + payload + crc32(4B)`，CRC 覆盖 idx+payload；`PAD_SLOT = u32::MAX` 跳过；接收端按 `slot_bytes` 定长切分，不能依赖帧同步。
- 构造参数超过 7 个必须用结构体（已有 `HeaderSpec`、`SampleSpec`），禁止 `#[allow(clippy::too_many_arguments)]`。
- 渲染必须确定性：同一 `Geometry` + `Header` + slots 必须产生逐字节相同的 PNG。

## 7. 抓屏与显示不变量

- 通道前提：同机窗口、设备像素 1:1。码图必须 ≤ 播放页可视区（不是窗口客户区），否则黑边框被裁，接收端只会报「没有找到码框」。
- 抓窗走 `PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)`：窗口可被遮挡，但不能最小化；客户区为空或窗口最小化必须显式报错。
- 连续抓屏失败 5 次必须失败退出，不允许静默空转；`--rect x,y,w,h` 是自动定位失败时的人工兜底（含黑边框）。
- 播放页只显示码图；任何覆盖像素（进度条、鼠标、文字）都会变成 CRC 失败。

## 8. 分片库与恢复不变量

- 落盘布局固定在 `server/src/store.rs`：`.qr_blocks.bin`（按全局下标定位的稀疏文件）+ `.qr_state.json`（帧头 + 分片位图）。
- 状态文件必须 tmp + rename 原子写；每秒保存一次，强杀最多丢 1 秒。
- `--resume` 必须校验 `FORMAT` 与帧头 CRC；格式不兼容要显式报错，不允许猜。
- 只有 SHA-256 前缀校验通过后才写输出文件并清库；Windows 上先关文件句柄再删除。
- `--clean` 只删分片库相关文件，不碰输出文件与日志。

## 9. 工作方式

**准则**

1. 以暗猜接口为耻，以认真查阅为荣
2. 以模糊执行为耻，以寻求确认为荣
3. 以盲想业务为耻，以人类确认为荣
4. 以创造接口为耻，以复用现有为荣
5. 以跳过验证为耻，以主动测试为荣
6. 以破坏架构为耻，以遵循规范为荣
7. 以假装理解为耻，以诚实无知为荣
8. 以盲目修改为耻，以谨慎重构为荣

**执行**

- 先查证再提问：能自己读代码、文档、配置确认的事实不要占用提问轮次。
- 需要用户决策时，一次性提出所有互不依赖的问题，每条说明要什么信息与为什么需要，能给候选或默认值的直接给。
- 动手前用 1~3 句说明修改思路；存在多种可行方案时列出优缺点让用户选。
- 只改任务范围内的代码，最小必要修改；不主动重构无关代码，不因个人偏好改动既有实现，不删除已有功能除非明确要求。
- 保持现有代码风格与项目结构；简单机械的改动可以直接做，涉及多文件或行为变化先给方案。
- 不主动升级依赖；修改 CI、构建配置或系统配置前先说明影响。
- 行为、参数、日志格式变化时同步更新 `README.md` 与 `docs/`；没有行为变化时不要顺手改文档。
- 不执行 `git reset --hard`、递归删除目录等危险命令；删文件、清目录前确认目标路径。
- 不在代码、日志、错误信息中输出密钥、token、密码。

**回复**

- 中文回复，先给结论与可执行步骤；简单问题简洁答，复杂问题再展开。
- 改完说明：改了什么、改了哪些文件、测试执行情况、需要注意的问题。
- 不贴完整 diff，除非明确要求。

## 10. 已知债务

- `client/web/index.html` 是手写原生 JS：无构建、无前端测试；改播放逻辑只能靠手动验证 + `verify_frames` 兜底。
- 窗口抓取只有 Windows 实现；非 Windows 下 `client --probe` 不可用、`server` 只能 `--fullscreen`。
- 没有 CI，三关与 e2e 靠本地手动执行。
- 日志目录固定在工作目录下的 `logs/`，不支持自定义路径。
- 依赖已钉 `=版本`；升级必须手动改 `Cargo.toml` 并重新验证。
