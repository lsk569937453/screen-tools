//! 发送端命令行参数：只用 clap derive 声明，不在业务代码里手写解析循环。

use clap::{CommandFactory, Parser};
use std::str::FromStr;

const LONG_ABOUT: &str = "\
ScreenCode 发送端：把文件编码成单张高密度网格码（ScreenCode v1），逐帧显示给同机接收端抓屏识别。

先读这段：
  码图是 frame_w × frame_h 的 1-bit PNG，播放页按设备像素 1:1 摆放，
  所以 --frame 必须 <= 播放页面的「可视区」（页面面板会打印，例如 页面可视区 1912x857）。
  比可视区大 → 右侧/底部被裁 → 黑边框不闭合 → 接收端一直报「没有找到码框」。
  可视区 = 窗口去掉浏览器标签栏/地址栏、再减去任务栏；F11 全屏 + 任务栏自动隐藏最大。";

const EXAMPLES: &str = r#"示例:
  # 1) 最常用：按页面可视区生成并播放（尺寸抄页面面板里的「页面可视区 WxH」）
  client .\big.zip --frame 1912x856

  # 2) 先探窗口，再让发送端自动取窗口客户区尺寸
  client --probe
  client .\big.zip --frame auto

  # 3) 更密：约 4 倍容量，但要求严格 1:1（必须是设备像素 1:1、不能有任何缩放）
  client .\big.zip --frame 1900x820 --module-px 1

  # 4) 遮挡/反光多：把冗余提到 20%（收满仍自动纠删，只是帧数变多）
  client .\big.zip --frame 1912x856 --parity 0.2

  # 5) 离线导出 + 自检整条链路（不依赖窗口和播放页）
  client .\big.zip --export qr_output --frame 1900x820
  cargo run -p screencode --example verify_frames -- qr_output"#;

/// 默认帧尺寸（同时用于 `--frame` 的默认值与 `auto` 探测失败时的回退）
pub const DEFAULT_FRAME_WIDTH: u32 = 1920;
pub const DEFAULT_FRAME_HEIGHT: u32 = 1080;

/// 目标帧尺寸
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSpec {
    /// 自动读 `--process` 匹配窗口的客户区尺寸
    Auto,
    /// 固定尺寸
    Fixed { width: u32, height: u32 },
}

impl Default for FrameSpec {
    fn default() -> Self {
        Self::Fixed {
            width: DEFAULT_FRAME_WIDTH,
            height: DEFAULT_FRAME_HEIGHT,
        }
    }
}

impl std::fmt::Display for FrameSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::Fixed { width, height } => write!(formatter, "{width}x{height}"),
        }
    }
}

impl FromStr for FrameSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        let Some((width, height)) = value.split_once('x').or_else(|| value.split_once('X')) else {
            return Err(format!(
                "--frame 需要 WxH 或 auto，例如 2505x1329；收到 '{value}'"
            ));
        };
        let width: u32 = width
            .trim()
            .parse()
            .map_err(|_| format!("--frame 宽度不是数字: '{}'", width.trim()))?;
        let height: u32 = height
            .trim()
            .parse()
            .map_err(|_| format!("--frame 高度不是数字: '{}'", height.trim()))?;
        if !(320..=8192).contains(&width) || !(240..=8192).contains(&height) {
            return Err(format!(
                "--frame 超出范围（宽 320..=8192、高 240..=8192）；收到 {width}x{height}"
            ));
        }
        Ok(Self::Fixed { width, height })
    }
}

fn parse_parity(value: &str) -> Result<f64, String> {
    let parity: f64 = value
        .trim()
        .parse()
        .map_err(|_| format!("--parity 需要数字，收到 '{value}'"))?;
    if !(0.0..=1.0).contains(&parity) {
        return Err(format!("--parity 必须在 0.0..=1.0 之间，收到 {value}"));
    }
    Ok(parity)
}

fn parse_positive_u32(value: &str) -> Result<u32, String> {
    let parsed: u32 = value
        .trim()
        .parse()
        .map_err(|_| format!("需要正整数，收到 '{value}'"))?;
    if parsed == 0 {
        return Err(format!("必须 >= 1，收到 '{value}'"));
    }
    Ok(parsed)
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed: usize = value
        .trim()
        .parse()
        .map_err(|_| format!("需要正整数，收到 '{value}'"))?;
    if parsed == 0 {
        return Err(format!("必须 >= 1，收到 '{value}'"));
    }
    Ok(parsed)
}

/// ScreenCode 发送端
#[derive(Debug, Parser)]
#[command(
    name = "client",
    version,
    about = "ScreenCode 发送端：把文件编码成单张高密度码并起 HTTP 播放服务",
    long_about = LONG_ABOUT,
    after_help = EXAMPLES
)]
pub struct Cli {
    /// 要发送的文件；传 help 显示帮助
    #[arg(value_name = "FILE")]
    pub file: Option<String>,

    /// 目标帧尺寸：WxH 或 auto（auto = 读 --process 窗口客户区）
    #[arg(
        long,
        value_name = "WxH|auto",
        default_value_t = FrameSpec::default(),
        help_heading = "版式"
    )]
    pub frame: FrameSpec,

    /// 兼容旧写法：等价于 --frame auto
    #[arg(long, hide = true)]
    pub auto: bool,

    /// 每个模块的像素（1 更密，3 更稳）
    #[arg(
        long,
        value_name = "N",
        default_value_t = 2,
        value_parser = parse_positive_u32,
        help_heading = "版式"
    )]
    pub module_px: u32,

    /// 分片净荷字节
    #[arg(
        long,
        value_name = "N",
        default_value_t = 4096,
        value_parser = parse_positive_usize,
        help_heading = "版式"
    )]
    pub block_bytes: usize,

    /// 纠删码冗余比例（0 表示关闭，不推荐）
    #[arg(
        long,
        value_name = "F",
        default_value_t = 0.10,
        value_parser = parse_parity,
        help_heading = "版式"
    )]
    pub parity: f64,

    /// 每帧停留毫秒
    #[arg(long, value_name = "N", default_value_t = 300, help_heading = "播放")]
    pub dwell_ms: u64,

    /// HTTP 端口
    #[arg(long, value_name = "N", default_value_t = 9090, help_heading = "播放")]
    pub port: u16,

    /// --frame auto / --probe 匹配的进程名
    #[arg(
        long,
        value_name = "NAME",
        default_value = "HSR",
        value_parser = clap::builder::NonEmptyStringValueParser::new(),
        help_heading = "窗口"
    )]
    pub process: String,

    /// 只导出 PNG 帧到 DIR，不起服务；省略 DIR 时用 qr_output
    #[arg(
        long,
        value_name = "DIR",
        num_args = 0..=1,
        default_missing_value = "qr_output",
        help_heading = "导出"
    )]
    pub export: Option<String>,

    /// 打印目标窗口客户区尺寸后退出
    #[arg(long, help_heading = "窗口")]
    pub probe: bool,

    /// 控制台也输出每帧细节
    #[arg(short, long, help_heading = "其它")]
    pub verbose: bool,
}

impl Cli {
    /// `client help` 与 `client --help` 等价
    pub fn wants_help(&self) -> bool {
        self.file
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("help"))
    }

    /// 处理隐藏的 `--auto` 兼容写法
    pub fn resolved_frame(&self) -> FrameSpec {
        if self.auto {
            FrameSpec::Auto
        } else {
            self.frame
        }
    }
}

/// 打印 clap 生成的完整帮助
pub fn print_help() -> Result<(), std::io::Error> {
    let mut command = Cli::command();
    command.print_long_help()?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> Cli {
        Cli::try_parse_from(values).expect("parse cli")
    }

    #[test]
    fn parses_file_and_frame() {
        let cli = parse(&["client", "a.bin", "--frame", "2505x1329"]);
        assert_eq!(cli.file.as_deref(), Some("a.bin"));
        assert_eq!(
            cli.frame,
            FrameSpec::Fixed {
                width: 2505,
                height: 1329
            }
        );
    }

    #[test]
    fn parses_auto_frame_in_both_forms() {
        assert_eq!(
            parse(&["client", "a.bin", "--frame", "auto"]).frame,
            FrameSpec::Auto
        );
        assert_eq!(
            parse(&["client", "a.bin", "--frame=auto"]).frame,
            FrameSpec::Auto
        );
    }

    #[test]
    fn legacy_auto_flag_maps_to_auto_frame() {
        let cli = parse(&["client", "a.bin", "--auto"]);
        assert!(cli.auto);
        assert_eq!(cli.resolved_frame(), FrameSpec::Auto);
    }

    #[test]
    fn export_without_value_uses_default_dir() {
        let cli = parse(&["client", "a.bin", "--export"]);
        assert_eq!(cli.export.as_deref(), Some("qr_output"));

        let cli = parse(&["client", "a.bin", "--export", "frames"]);
        assert_eq!(cli.export.as_deref(), Some("frames"));
    }

    #[test]
    fn rejects_bad_frame_and_parity() {
        assert!(Cli::try_parse_from(["client", "a.bin", "--frame", "100x100"]).is_err());
        assert!(Cli::try_parse_from(["client", "a.bin", "--frame", "abc"]).is_err());
        assert!(Cli::try_parse_from(["client", "a.bin", "--parity", "2"]).is_err());
        assert!(Cli::try_parse_from(["client", "a.bin", "--module-px", "0"]).is_err());
    }

    #[test]
    fn help_requested_by_positional_alias() {
        assert!(parse(&["client", "help"]).wants_help());
        assert!(!parse(&["client", "a.bin"]).wants_help());
    }

    #[test]
    fn help_keeps_sections_and_examples() {
        let text = Cli::command().render_long_help().to_string();
        for needle in [
            "Usage",
            "版式",
            "播放",
            "窗口",
            "导出",
            "示例",
            "--frame",
            "--probe",
            "--export",
            "页面可视区",
        ] {
            assert!(text.contains(needle), "帮助文本缺少「{needle}」");
        }
    }
}
