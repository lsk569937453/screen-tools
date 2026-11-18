//! 接收端命令行参数：只用 clap derive 声明，不在业务代码里手写解析循环。

use clap::Parser;
use screencode::detect::Rect;
use std::time::Duration;

const LONG_ABOUT: &str = "\
默认后台截取进程 HSR 的窗口客户区（PrintWindow，被遮挡也能截，不能最小化）→
定位黑边框 → 读帧头 → 按模块坐标采样 → 分片落盘（.qr_blocks.bin / .qr_state.json）→
每组凑够 k 个分片就纠删恢复 → 全部收满后校验 SHA-256，写出 received_file_<时间戳>。

画面在变但一直报「没有找到码框」，多半是码图比播放页可视区大被裁了：
按播放页面板打印的「页面可视区 WxH」，用 client --frame 那个尺寸重新生成。";

const EXAMPLES: &str = r#"示例:
  # 1) 最常用：抓 HSR 窗口，收满后自动纠删恢复 + 校验 SHA-256
  server

  # 2) 上次没收到 k 个分片就停了：接着收（分片库会保留）
  server --resume

  # 3) 不知道抓哪个窗口：先列一遍，再按进程或标题抓
  server --list-windows
  server --window Chrome

  # 4) 播放页不在普通窗口里（比如全屏/直接贴屏幕）：抓整屏
  server --fullscreen

  # 5) 自动定位失败：手工给码框，并让控制台输出每帧细节
  server -v --window Chrome --rect 8,111,1884,804

  # 6) 机器慢 / 大窗口识别吃力：放宽间隔，少做重复识别
  server --interval-ms 80 --recheck-ms 200

  # 7) 个别分片一直补不上：不要因为空闲 3 秒就退出，一直等下去
  server --idle-timeout 0

  # 8) 换文件重来：先清库
  server --clean"#;

/// ScreenCode 接收端
#[derive(Debug, Parser)]
#[command(
    name = "server",
    version,
    about = "ScreenCode 接收端：后台截取目标窗口，识别码图并还原文件",
    long_about = LONG_ABOUT,
    after_help = EXAMPLES
)]
pub struct Cli {
    /// 继续上次的分片库（中断/超时后用）
    #[arg(long, help_heading = "会话")]
    pub resume: bool,

    /// 清空分片库后退出
    #[arg(long, help_heading = "会话")]
    pub clean: bool,

    /// 按进程名匹配窗口（部分匹配即可命中 HSRClient）
    #[arg(
        long,
        value_name = "NAME",
        default_value = "HSR",
        value_parser = clap::builder::NonEmptyStringValueParser::new(),
        help_heading = "窗口"
    )]
    pub process: String,

    /// 按窗口标题片段匹配（与 --process 可叠加）
    #[arg(
        long,
        value_name = "TITLE",
        value_parser = clap::builder::NonEmptyStringValueParser::new(),
        help_heading = "窗口"
    )]
    pub window: Option<String>,

    /// 抓整块屏幕（播放页不在窗口里时用）
    #[arg(long, help_heading = "窗口")]
    pub fullscreen: bool,

    /// 手工指定码框（含黑边框，例如 8,111,1884,804）
    #[arg(
        long,
        value_name = "x,y,w,h",
        value_parser = parse_rect,
        help_heading = "窗口"
    )]
    pub rect: Option<Rect>,

    /// 列出所有可见窗口及其客户区尺寸后退出
    #[arg(long, help_heading = "窗口")]
    pub list_windows: bool,

    /// 截屏间隔毫秒（机器慢可放大）
    #[arg(
        long,
        value_name = "N",
        default_value_t = 50,
        value_parser = parse_positive_u64,
        help_heading = "时序"
    )]
    pub interval_ms: u64,

    /// 画面不变时也重新识别的间隔毫秒
    #[arg(
        long,
        value_name = "N",
        default_value_t = 100,
        value_parser = parse_positive_u64,
        help_heading = "时序"
    )]
    pub recheck_ms: u64,

    /// 无新分片多少秒后停止（0 表示永不停止）
    #[arg(
        long,
        value_name = "N",
        default_value_t = 3.0,
        value_parser = parse_idle_seconds,
        help_heading = "输出"
    )]
    pub idle_timeout: f64,

    /// 输出文件名（默认 received_file_<时间戳>）
    #[arg(
        long,
        value_name = "NAME",
        value_parser = clap::builder::NonEmptyStringValueParser::new(),
        help_heading = "输出"
    )]
    pub out: Option<String>,

    /// 控制台也打印每帧细节（默认细节只写 logs/）
    #[arg(short, long, help_heading = "诊断")]
    pub verbose: bool,
}

/// 运行期配置（`--idle-timeout` 等已转换成业务类型）
#[derive(Debug, Clone)]
pub struct Options {
    pub resume: bool,
    pub clean: bool,
    pub verbose: bool,
    pub interval_ms: u64,
    pub process: String,
    pub window: Option<String>,
    pub fullscreen: bool,
    pub list_windows: bool,
    pub idle_timeout: Option<Duration>,
    pub recheck_ms: u64,
    pub rect: Option<Rect>,
    pub out: Option<String>,
}

impl From<Cli> for Options {
    fn from(cli: Cli) -> Self {
        Self {
            resume: cli.resume,
            clean: cli.clean,
            verbose: cli.verbose,
            interval_ms: cli.interval_ms,
            process: cli.process,
            window: cli.window,
            fullscreen: cli.fullscreen,
            list_windows: cli.list_windows,
            idle_timeout: idle_timeout(cli.idle_timeout),
            recheck_ms: cli.recheck_ms,
            rect: cli.rect,
            out: cli.out,
        }
    }
}

fn idle_timeout(seconds: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(seconds.max(0.0))
        .ok()
        .filter(|timeout| !timeout.is_zero())
}

fn parse_positive_u64(value: &str) -> Result<u64, String> {
    let parsed: u64 = value
        .trim()
        .parse()
        .map_err(|_| format!("需要正整数，收到 '{value}'"))?;
    if parsed == 0 {
        return Err(format!("必须 >= 1，收到 '{value}'"));
    }
    Ok(parsed)
}

fn parse_idle_seconds(value: &str) -> Result<f64, String> {
    let seconds: f64 = value
        .trim()
        .parse()
        .map_err(|_| format!("--idle-timeout 需要数字（秒），收到 '{value}'"))?;
    if !seconds.is_finite() {
        return Err(format!("--idle-timeout 需要有限数字（秒），收到 '{value}'"));
    }
    if seconds < 0.0 {
        return Err(format!("--idle-timeout 不能为负，收到 '{value}'"));
    }
    if seconds > 0.0 && Duration::try_from_secs_f64(seconds).is_err() {
        return Err(format!("--idle-timeout 太大，收到 '{value}'"));
    }
    Ok(seconds)
}

fn parse_rect(value: &str) -> Result<Rect, String> {
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!("--rect 需要 x,y,w,h，收到 '{value}'"));
    }
    let mut nums = [0u32; 4];
    for (index, part) in parts.iter().enumerate() {
        nums[index] = part
            .parse()
            .map_err(|_| format!("--rect 第 {} 项不是数字: '{}'", index + 1, part))?;
    }
    if nums[2] == 0 || nums[3] == 0 {
        return Err("--rect 的宽高必须大于 0".to_string());
    }
    Ok(Rect {
        x: nums[0],
        y: nums[1],
        w: nums[2],
        h: nums[3],
    })
}

/// `server help` 与 `server --help` 等价：归一化后交给 clap
pub fn normalize_help_alias(args: Vec<String>) -> Vec<String> {
    if args
        .get(1)
        .is_some_and(|value| value.eq_ignore_ascii_case("help"))
    {
        vec![args[0].clone(), "--help".to_string()]
    } else {
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(values: &[&str]) -> Cli {
        Cli::try_parse_from(values).expect("parse cli")
    }

    #[test]
    fn parses_defaults_and_flags() {
        let options = Options::from(parse(&["server", "--resume", "-v"]));
        assert!(options.resume);
        assert!(options.verbose);
        assert_eq!(options.process, "HSR");
        assert_eq!(options.interval_ms, 50);
        assert_eq!(options.idle_timeout, Some(Duration::from_secs(3)));
    }

    #[test]
    fn parses_rect_and_timeout() {
        let options = Options::from(parse(&[
            "server",
            "--rect=10,20,300,400",
            "--idle-timeout",
            "10",
            "--process=game",
        ]));
        assert_eq!(
            options.rect,
            Some(Rect {
                x: 10,
                y: 20,
                w: 300,
                h: 400
            })
        );
        assert_eq!(options.idle_timeout, Some(Duration::from_secs(10)));
        assert_eq!(options.process, "game");
    }

    #[test]
    fn idle_zero_means_never() {
        let options = Options::from(parse(&["server", "--idle-timeout", "0"]));
        assert_eq!(options.idle_timeout, None);
    }

    #[test]
    fn rejects_bad_rect() {
        assert!(Cli::try_parse_from(["server", "--rect", "1,2,3"]).is_err());
        assert!(Cli::try_parse_from(["server", "--rect", "1,2,0,4"]).is_err());
        assert!(Cli::try_parse_from(["server", "--rect", "a,2,3,4"]).is_err());
    }

    #[test]
    fn rejects_empty_process() {
        assert!(Cli::try_parse_from(["server", "--process="]).is_err());
        assert!(Cli::try_parse_from(["server", "--window="]).is_err());
    }

    #[test]
    fn rejects_positional_arguments() {
        assert!(Cli::try_parse_from(["server", "extra"]).is_err());
    }

    #[test]
    fn help_alias_is_normalized() {
        assert_eq!(
            normalize_help_alias(vec!["server".into(), "help".into()]),
            vec!["server".to_string(), "--help".to_string()]
        );
        assert_eq!(
            normalize_help_alias(vec!["server".into(), "--resume".into()]),
            vec!["server".to_string(), "--resume".to_string()]
        );
    }

    #[test]
    fn help_keeps_sections_and_examples() {
        let text = Cli::command().render_long_help().to_string();
        for needle in [
            "Usage",
            "会话",
            "窗口",
            "时序",
            "输出",
            "诊断",
            "示例",
            "--resume",
            "--rect",
            "--list-windows",
            "页面可视区",
        ] {
            assert!(text.contains(needle), "帮助文本缺少「{needle}」");
        }
    }
}
