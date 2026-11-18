//! 日志集成测试：一次初始化全局订阅者，验证文件里同时落下 INFO 与 DEBUG。

use common::logging;
use std::fs;
use tracing::{debug, info};

#[test]
fn writes_info_and_debug_to_file() {
    let dir = std::env::temp_dir().join(format!("screentools-log-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);

    let log = logging::init("test", false, Some(&dir)).expect("init logging");
    assert_eq!(log.file_error, None, "file error: {:?}", log.file_error);

    info!("info line");
    debug!("debug line");

    let path = log.file.clone().expect("log file path");
    let text = fs::read_to_string(&path).expect("read log file");
    assert!(text.contains("INFO"), "log: {text}");
    assert!(text.contains("DEBUG"), "log: {text}");
    assert!(text.contains("info line"), "log: {text}");
    assert!(text.contains("debug line"), "log: {text}");

    let _ = fs::remove_dir_all(&dir);
}
