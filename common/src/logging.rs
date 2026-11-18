//! tracing 日志初始化：控制台 + 可选文件，全项目唯一的日志入口。
//!
//! 控制台默认 `INFO`（`-v` 时 `DEBUG`），文件始终记录 `DEBUG` 及以上。
//! 文件写入是同步的，保持"逐行落盘"语义；日志文件不可写时降级为仅控制台。

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// 控制台与文件统一的时间格式（本机时区）
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// 日志初始化结果
#[derive(Debug)]
pub struct LogInfo {
    /// 已创建的日志文件；`None` 表示只输出到控制台
    pub file: Option<PathBuf>,
    /// 日志文件初始化失败的原因；`None` 表示未请求文件或文件正常
    pub file_error: Option<String>,
}

/// 初始化全局日志订阅者
///
/// `verbose` 为真时控制台也输出 `DEBUG`；`log_dir` 为 `None` 时不写文件。
pub fn init(program: &str, verbose: bool, log_dir: Option<&Path>) -> Result<LogInfo> {
    let console_level = if verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .with_target(false)
        .with_timer(ChronoLocal::new(TIME_FORMAT.to_string()))
        .with_filter(console_level);

    let mut info = LogInfo {
        file: None,
        file_error: None,
    };
    let file_layer = match log_dir {
        None => None,
        Some(dir) => match build_file_appender(dir, program) {
            Ok((appender, path)) => {
                info.file = Some(path);
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_writer(appender)
                        .with_ansi(false)
                        .with_target(false)
                        .with_timer(ChronoLocal::new(TIME_FORMAT.to_string()))
                        .with_filter(LevelFilter::DEBUG),
                )
            }
            Err(error) => {
                info.file_error = Some(error.to_string());
                None
            }
        },
    };

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()
        .map_err(|error| anyhow!("初始化日志订阅者失败: {error}"))?;
    Ok(info)
}

/// 创建"一次运行一个文件"的日志 appender（`logs/<program>-<YYYYMMDD-HHMMSS>.log`）
fn build_file_appender(dir: &Path, program: &str) -> Result<(RollingFileAppender, PathBuf)> {
    fs::create_dir_all(dir).with_context(|| format!("创建日志目录 {} 失败", dir.display()))?;
    let name = format!("{program}-{}", crate::clock::timestamp_file());
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::NEVER)
        .filename_prefix(name.clone())
        .filename_suffix("log")
        .build(dir)
        .with_context(|| format!("创建日志文件 {}/{}.log 失败", dir.display(), name))?;
    Ok((appender, dir.join(format!("{name}.log"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("screentools-common-{tag}-{}", std::process::id()))
    }

    #[test]
    fn log_file_name_follows_documented_pattern() {
        let dir = temp_dir("name");
        let (_, path) = build_file_appender(&dir, "server").expect("appender");
        assert!(path.exists(), "path missing: {}", path.display());

        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .expect("file name");
        assert!(name.starts_with("server-"), "name={name}");
        assert!(name.ends_with(".log"), "name={name}");

        let _ = fs::remove_dir_all(&dir);
    }
}
