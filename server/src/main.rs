//! 接收端：从 HSR 窗口的后台截图里识别单张高密度码，收集分片、纠删恢复、还原文件。
//!
//! 与旧版最大的区别：不再找二维码，而是按固定网格直接采样；丢的分片由文件级纠删码补回。

use anyhow::{anyhow, Result};
use screencode::detect::{decode_frame, to_gray, DecodeOptions, Rect};
use screencode::header::Header;
use screencode::PAD_SLOT;
use screenshots::Screen;
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

mod store;

use store::ShardStore;

const DEFAULT_INTERVAL_MS: u64 = 50;
const DEFAULT_RECHECK_MS: u64 = 100;
const DEFAULT_IDLE_SECONDS: u64 = 3;
const SAVE_INTERVAL_SECONDS: u64 = 1;
const LOG_DIR: &str = "logs";
const DEFAULT_PROCESS: &str = "HSR";

#[derive(Debug, Clone)]
struct Options {
    resume: bool,
    clean: bool,
    verbose: bool,
    interval_ms: u64,
    process: String,
    window: Option<String>,
    fullscreen: bool,
    list_windows: bool,
    idle_timeout: Option<Duration>,
    recheck_ms: u64,
    rect: Option<Rect>,
    out: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            resume: false,
            clean: false,
            verbose: false,
            interval_ms: DEFAULT_INTERVAL_MS,
            process: DEFAULT_PROCESS.to_string(),
            window: None,
            fullscreen: false,
            list_windows: false,
            idle_timeout: Some(Duration::from_secs(DEFAULT_IDLE_SECONDS)),
            recheck_ms: DEFAULT_RECHECK_MS,
            rect: None,
            out: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 日志：控制台只留关键信息，细节全部写入 logs/server-<时间戳>.log
// ---------------------------------------------------------------------------

static RUN_START: OnceLock<Instant> = OnceLock::new();
static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static CONSOLE_VERBOSE: AtomicBool = AtomicBool::new(false);

/// 本机时间 `YYYY-MM-DD HH:MM:SS.mmm`（按东八区，不引入额外平台依赖）
fn wall_clock() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64
        + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.000",
        year, month, day, hour, minute, second
    )
}

/// 把 Unix 天数换算成公历日期（Howard Hinnant 算法）
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn log_file_stamp() -> String {
    let digits: String = wall_clock()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    if digits.len() == 14 {
        format!("{}-{}", &digits[..8], &digits[8..])
    } else {
        digits
    }
}

fn init_log_file(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir).map_err(|e| anyhow!("创建日志目录 {} 失败: {}", dir.display(), e))?;
    let path = dir.join(format!("server-{}.log", log_file_stamp()));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow!("创建日志文件 {} 失败: {}", path.display(), e))?;
    let _ = LOG_FILE.set(Mutex::new(Some(file)));
    Ok(path)
}

fn append_log_file(line: &str) {
    let Some(lock) = LOG_FILE.get() else {
        return;
    };
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(file, "{}", line);
        let _ = file.flush();
    }
}

fn format_log_line(kind: &str, message: &str) -> String {
    format!(
        "[{}] [{:>8.3}s] [{}] {}",
        wall_clock(),
        uptime().as_secs_f64(),
        kind,
        message
    )
}

fn log(kind: &str, message: impl std::fmt::Display) {
    let line = format_log_line(kind, &message.to_string());
    println!("{}", line);
    append_log_file(&line);
}

fn detail(kind: &str, message: impl std::fmt::Display) {
    let line = format_log_line(kind, &message.to_string());
    if CONSOLE_VERBOSE.load(Ordering::Relaxed) {
        println!("{}", line);
    }
    append_log_file(&line);
}

fn say(message: impl std::fmt::Display) {
    let text = message.to_string();
    println!("{}", text);
    append_log_file(&text);
}

fn uptime() -> Duration {
    RUN_START.get_or_init(Instant::now).elapsed()
}

fn format_file_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// ---------------------------------------------------------------------------
// 运行统计
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    loops: u64,
    captured: u64,
    capture_errors: u64,
    frames_unchanged: u64,
    frames_scanned: u64,
    frames_with_code: u64,
    headers_read: u64,
    detect_errors: u64,
    slots_seen: u64,
    slots_new: u64,
    crc_failures: u64,
    capture_ms: u128,
    detect_ms: u128,
    new_bytes: u64,
}

impl Stats {
    fn report(
        &self,
        received: usize,
        total: usize,
        groups: (usize, usize),
        idle: Duration,
    ) -> String {
        let avg_capture = if self.captured == 0 {
            0.0
        } else {
            self.capture_ms as f64 / self.captured as f64
        };
        let avg_detect = if self.frames_scanned == 0 {
            0.0
        } else {
            self.detect_ms as f64 / self.frames_scanned as f64
        };
        let rate = if idle.as_secs_f64() > 0.0 {
            self.new_bytes as f64 / idle.as_secs_f64()
        } else {
            0.0
        };
        format!(
            "运行 {:.1}s | 循环 {} | 截屏 {}（失败 {}，未变化 {}）| 识别 {} 帧 | 码框 {} / 帧头 {} / 槽 {}（CRC 失败 {}，识别失败 {}）| 均值 截屏 {:.1}ms 识别 {:.1}ms | 分片 {}/{}（本次 +{}）| 组 {}/{} | {:.0} KB/s | 空闲 {:.1}s",
            uptime().as_secs_f64(),
            self.loops,
            self.captured,
            self.capture_errors,
            self.frames_unchanged,
            self.frames_scanned,
            self.frames_with_code,
            self.headers_read,
            self.slots_seen,
            self.crc_failures,
            self.detect_errors,
            avg_capture,
            avg_detect,
            received,
            total,
            self.slots_new,
            groups.0,
            groups.1,
            rate / 1024.0,
            idle.as_secs_f64()
        )
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn main() {
    RUN_START.get_or_init(Instant::now);
    let args: Vec<String> = env::args().collect();
    if let Err(error) = main_with_error(&args) {
        log("错误", &error);
        say("\n接收失败，详见上方日志与 logs/ 目录");
        std::process::exit(1);
    }
}

fn main_with_error(args: &[String]) -> Result<()> {
    let opts = parse_options(args).map_err(|e| anyhow!("参数错误: {}\n\n{}", e, usage_text()))?;

    if opts.verbose {
        CONSOLE_VERBOSE.store(true, Ordering::Relaxed);
    }

    match init_log_file(Path::new(LOG_DIR)) {
        Ok(path) => log("日志", format!("日志文件: {}", path.display())),
        Err(error) => println!("警告: {}，日志只会输出到控制台", error),
    }

    if opts.clean {
        let removed = store::clean(Path::new("."))?;
        if removed.is_empty() {
            log("会话", "没有可清除的分片库");
        } else {
            for path in removed {
                log("会话", format!("已删除 {}", path.display()));
            }
        }
        return Ok(());
    }

    if opts.list_windows {
        return list_windows_report();
    }

    say("╔════════════════════════════════════════╗");
    say("║       ScreenCode 接收器                ║");
    say("╚════════════════════════════════════════╝");
    if let Ok(dir) = env::current_dir() {
        say(format!(
            "分片库: {}（{}）",
            dir.join(store::BLOCKS_FILE).display(),
            store::describe_store(&dir)
        ));
    }
    say("");

    let mut shard_store = open_store(&opts)?;

    log(
        "配置",
        format!(
            "截屏间隔 {} ms | 空闲超时 {} | 重解间隔 {} ms | 纠正冗余 {}",
            opts.interval_ms,
            match opts.idle_timeout {
                Some(limit) => format!("{} s", limit.as_secs()),
                None => "关闭（--idle-timeout 0）".to_string(),
            },
            opts.recheck_ms,
            shard_store
                .header
                .as_ref()
                .map(|h| format!("k={} m={}", h.k, h.m))
                .unwrap_or_else(|| "待定（读帧头后确定）".to_string())
        ),
    );

    let screens = Screen::all().map_err(|e| anyhow!("获取屏幕失败: {}", e))?;
    detail("信息", format!("检测到 {} 块屏幕", screens.len()));
    for (index, item) in screens.iter().enumerate() {
        let info = &item.display_info;
        detail(
            "屏幕",
            format!(
                "#{} id={} 位置=({},{}) 尺寸={}x{} 缩放={:.2}",
                index + 1,
                info.id,
                info.x,
                info.y,
                info.width,
                info.height,
                info.scale_factor
            ),
        );
    }

    let mut source = open_capture_source(&opts, screens)?;

    let mut stats = Stats::default();
    let mut last_data_time = Instant::now();
    let mut last_report = Instant::now();
    let mut last_save = Instant::now();
    let mut last_recheck = Instant::now();
    let mut last_detect_warn: Option<Instant> = None;
    let mut last_frame: Vec<u8> = Vec::new();
    let mut cached_header: Option<Header> = shard_store.header.clone();
    let mut mismatch_warned = false;

    loop {
        stats.loops += 1;

        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let (received, total) = shard_store.progress();
            detail(
                "统计",
                stats.report(
                    received,
                    total,
                    shard_store.groups_ready(),
                    last_data_time.elapsed(),
                ),
            );
        }

        let capture_started = Instant::now();
        let screenshot = match source.capture() {
            Ok(screenshot) => screenshot,
            Err(error) => {
                stats.capture_errors += 1;
                if stats.capture_errors <= 3 || stats.capture_errors % 50 == 0 {
                    log(
                        "错误",
                        format!("截屏失败（累计 {} 次）: {}", stats.capture_errors, error),
                    );
                }
                if source.is_window() && stats.capture_errors >= 5 {
                    return Err(anyhow!(
                        "窗口截图连续失败 {} 次，已停止：{}",
                        stats.capture_errors,
                        error
                    ));
                }
                thread::sleep(Duration::from_millis(opts.interval_ms));
                continue;
            }
        };
        stats.captured += 1;
        let capture_elapsed = capture_started.elapsed();
        stats.capture_ms += capture_elapsed.as_millis();

        let width = screenshot.width() as usize;
        let height = screenshot.height() as usize;
        let rgba = screenshot.rgba();

        let frame_changed = rgba[..] != last_frame[..];
        if frame_changed {
            last_frame.clear();
            last_frame.extend_from_slice(rgba);
        } else {
            stats.frames_unchanged += 1;
        }

        let recheck_due = last_recheck.elapsed() >= Duration::from_millis(opts.recheck_ms);
        if frame_changed || recheck_due {
            last_recheck = Instant::now();
            let gray = to_gray(rgba, width, height);
            let detect_started = Instant::now();
            let result = decode_frame(
                &gray,
                width,
                height,
                cached_header.as_ref(),
                &DecodeOptions { rect: opts.rect },
            );
            let detect_elapsed = detect_started.elapsed();
            stats.detect_ms += detect_elapsed.as_millis();

            match result {
                Ok(decoded) => {
                    stats.frames_scanned += 1;
                    stats.frames_with_code += 1;
                    stats.headers_read += 1;
                    if cached_header.as_ref() != Some(&decoded.header) {
                        cached_header = Some(decoded.header.clone());
                    }

                    let same_session = match shard_store.header.as_ref() {
                        None => true,
                        Some(existing) => existing == &decoded.header,
                    };

                    if same_session {
                        if shard_store.ensure(&decoded.header)? {
                            let header = &decoded.header;
                            log(
                                "信息",
                                format!(
                                    "会话建立: {} | 分片 {} | 每组 k={} m={} | 码 {}x{} 模块（实测 {:.3}px）| 原始冗余 {:.0}%",
                                    format_file_size(header.file_len as usize),
                                    header.total_shards,
                                    header.k,
                                    header.m,
                                    header.grid_w,
                                    header.grid_h,
                                    decoded.module_x,
                                    if header.k > 0 {
                                        header.m as f64 * 100.0 / header.k as f64
                                    } else {
                                        0.0
                                    }
                                ),
                            );
                        }

                        let total = shard_store.total_shards();
                        let mut new_here = 0usize;
                        for rec in &decoded.slots {
                            stats.slots_seen += 1;
                            if rec.idx == PAD_SLOT {
                                continue;
                            }
                            let idx = rec.idx as usize;
                            if idx >= total {
                                continue;
                            }
                            if shard_store.ingest(idx, &rec.payload)? {
                                stats.slots_new += 1;
                                stats.new_bytes += rec.payload.len() as u64;
                                new_here += 1;
                                last_data_time = Instant::now();
                            }
                        }

                        if decoded.crc_failures > 0 {
                            stats.crc_failures += decoded.crc_failures as u64;
                        }

                        detail(
                            "帧",
                            format!(
                                "{}x{} | rect=({},{},{},{}) | m={:.3}/{:.3} | 槽 {}/{}（CRC 失败 {}，新增 {}）| 截屏 {}ms 识别 {}ms",
                                width,
                                height,
                                decoded.rect.x,
                                decoded.rect.y,
                                decoded.rect.w,
                                decoded.rect.h,
                                decoded.module_x,
                                decoded.module_y,
                                decoded.slots.len(),
                                decoded.slot_count,
                                decoded.crc_failures,
                                new_here,
                                capture_elapsed.as_millis(),
                                detect_elapsed.as_millis()
                            ),
                        );
                    } else {
                        stats.slots_seen += decoded.slots.len() as u64;
                        if !mismatch_warned {
                            mismatch_warned = true;
                            log(
                                "警告",
                                "屏幕上出现了另一个文件的码（帧头不一致），本帧已忽略；如需切换请先 --clean",
                            );
                        }
                    }
                }
                Err(error) => {
                    stats.detect_errors += 1;
                    if last_detect_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
                        last_detect_warn = Some(Instant::now());
                        log(
                            "警告",
                            format!(
                                "识别失败（累计 {} 次）: {}。确认播放页面在窗口内、未被最小化/缩放；必要时用 --rect x,y,w,h 指定码框",
                                stats.detect_errors, error
                            ),
                        );
                    }
                }
            }
        }

        // 每组都有 >= k 个有效分片：可以恢复了
        let (ready, groups) = shard_store.groups_ready();
        if groups > 0 && ready == groups {
            finish(&mut shard_store, &opts)?;
            break;
        }

        let idle_expired = opts
            .idle_timeout
            .is_some_and(|limit| last_data_time.elapsed() > limit);
        if idle_expired && !(shard_store.received_count() == 0) {
            let (received, total) = shard_store.progress();
            log(
                "超时",
                format!(
                    "{} 内没有收到新分片（本次新增 {}），已收集 {}/{}",
                    opts.idle_timeout
                        .map(|limit| format!("{:.1}s", limit.as_secs_f64()))
                        .unwrap_or_else(|| "-".to_string()),
                    stats.slots_new,
                    received,
                    total
                ),
            );
            report_shortfall(&shard_store);
            shard_store.save_state()?;
            return Err(anyhow!("空闲超时，未收满；分片库已保存，可 --resume 继续"));
        }

        if last_save.elapsed() >= Duration::from_secs(SAVE_INTERVAL_SECONDS) {
            last_save = Instant::now();
            shard_store.save_state()?;
        }

        thread::sleep(Duration::from_millis(opts.interval_ms));
    }

    say("");
    let (received, total) = shard_store.progress();
    log(
        "汇总",
        format!(
            "运行 {:.1}s | 截屏 {} 帧（失败 {}，未变化 {}）| 识别 {} 帧 | 码框 {} / 帧头 {} / 槽 {}（CRC 失败 {}）| 分片 {}/{} | 本次新增 {}",
            uptime().as_secs_f64(),
            stats.captured,
            stats.capture_errors,
            stats.frames_unchanged,
            stats.frames_scanned,
            stats.frames_with_code,
            stats.headers_read,
            stats.slots_seen,
            stats.crc_failures,
            received,
            total,
            stats.slots_new
        ),
    );

    Ok(())
}

fn open_store(opts: &Options) -> Result<ShardStore> {
    let paths = store::ShardPaths::current();
    if opts.resume {
        match ShardStore::open(paths.clone()) {
            Ok(Some(mut store)) => {
                let (received, total) = store.progress();
                log(
                    "会话",
                    format!("续传: 已收集 {}/{} 个分片", received, total),
                );
                if let Some(header) = store.header.clone() {
                    let deficits = store.group_deficits();
                    if deficits.is_empty() {
                        log("会话", "缺口已全部可恢复，继续接收即可");
                    } else {
                        let missing: usize = deficits.iter().map(|(_, n)| n).sum();
                        log(
                            "会话",
                            format!(
                                "还有 {} 个组不满足 k，合计缺 {} 个分片（继续播放即可，纠删码会补）",
                                deficits.len(),
                                missing
                            ),
                        );
                    }
                    let _ = header;
                }
                store.save_state()?;
                return Ok(store);
            }
            Ok(None) => {
                log("会话", "没有可续传的分片库，重新开始");
            }
            Err(error) => {
                log("警告", format!("无法续传: {}，将重新开始", error));
            }
        }
    } else if paths.state.exists() {
        log(
            "提示",
            format!(
                "发现上次的分片库 {}；用 --resume 可继续接收",
                paths.state.display()
            ),
        );
    }

    let store = ShardStore::create(paths)?;
    Ok(store)
}

fn finish(store: &mut ShardStore, opts: &Options) -> Result<()> {
    let Some(header) = store.header.clone() else {
        return Err(anyhow!("没有帧头，无法恢复"));
    };

    let (received, total) = store.progress();
    log(
        "完成",
        format!(
            "分片足够（{}/{}，k={} m={}），开始纠删恢复…",
            received, total, header.k, header.m
        ),
    );
    let started = Instant::now();
    let bytes = store.assemble()?;
    let digest = Sha256::digest(&bytes);
    let hash_hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    let prefix_ok = digest[..8] == header.hash_prefix;

    log(
        "完成",
        format!(
            "恢复完成: {} 字节 | 耗时 {:.2}s | SHA-256 {} | 前缀校验 {}",
            format_file_size(bytes.len()),
            started.elapsed().as_secs_f64(),
            hash_hex,
            if prefix_ok { "通过" } else { "不匹配" }
        ),
    );

    if !prefix_ok {
        return Err(anyhow!(
            "SHA-256 前缀与帧头不一致，文件可能损坏；分片库已保留，可 --resume 继续收"
        ));
    }

    let name = opts.out.clone().unwrap_or_else(|| {
        let digits: String = log_file_stamp()
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        format!("received_file_{}", digits)
    });
    fs::write(&name, &bytes)?;
    log("保存", format!("已写入 {}", name));

    // 成功后才清分片库；Windows 上必须先关句柄
    store.close();
    let _ = store::clean(Path::new("."));

    say("");
    say("═ 传输完成 ═");
    say(format!("文件: {}", name));
    say(format!("大小: {}", format_file_size(bytes.len())));
    say(format!("SHA-256: {}", hash_hex));
    say("");
    Ok(())
}

fn report_shortfall(store: &ShardStore) {
    if store.header.is_none() {
        log(
            "提示",
            "整个过程都没有读出帧头：确认播放页面在 HSR 窗口内、窗口未最小化；或先用 client --probe 确认窗口尺寸",
        );
        return;
    }
    let deficits = store.group_deficits();
    if deficits.is_empty() {
        log(
            "提示",
            "所有组其实都已满足 k，可能只是刚好在空闲窗口内没触发完成",
        );
        return;
    }
    let missing: usize = deficits.iter().map(|(_, n)| n).sum();
    let preview: Vec<String> = deficits
        .iter()
        .take(10)
        .map(|(group, need)| format!("组{}缺{}", group, need))
        .collect();
    log(
        "提示",
        format!(
            "还有 {} 个组不满足 k，合计缺 {} 个分片（{}）。把发送端的 --parity 提高（例如 0.2）重新生成一轮即可",
            deficits.len(),
            missing,
            preview.join("，")
        ),
    );
}

// ---------------------------------------------------------------------------
// 捕获来源（整屏 / 指定窗口客户区）
// ---------------------------------------------------------------------------

enum CaptureSource {
    Screen {
        screen: Screen,
    },
    #[cfg(windows)]
    Window(WindowCapture),
}

impl CaptureSource {
    fn is_window(&self) -> bool {
        #[cfg(windows)]
        if matches!(self, CaptureSource::Window(_)) {
            return true;
        }
        false
    }

    fn describe(&self) -> String {
        match self {
            CaptureSource::Screen { screen } => format!(
                "整屏 #{} {}x{}",
                screen.display_info.id, screen.display_info.width, screen.display_info.height
            ),
            #[cfg(windows)]
            CaptureSource::Window(state) => format!(
                "窗口 {}「{}」{}",
                state.target.process,
                state.target.title,
                state.frame.describe()
            ),
        }
    }

    fn capture(&mut self) -> Result<screenshots::Image> {
        match self {
            CaptureSource::Screen { screen } => screen.capture(),
            #[cfg(windows)]
            CaptureSource::Window(state) => state.capture(),
        }
    }
}

#[cfg(windows)]
struct WindowCapture {
    target: screencode::win::WindowInfo,
    frame: screencode::win::Frame,
}

#[cfg(windows)]
impl WindowCapture {
    fn capture(&mut self) -> Result<screenshots::Image> {
        let current = screencode::win::locate(self.target.handle)
            .ok_or_else(|| anyhow!("目标窗口已关闭: {}", self.target.describe()))?;
        if current.minimized {
            return Err(anyhow!(
                "目标窗口已最小化；后台截图要求窗口处于非最小化状态: {}",
                current.describe()
            ));
        }
        if current.frame.is_empty() {
            return Err(anyhow!(
                "目标窗口客户区为空（可能被隐藏）: {}",
                current.describe()
            ));
        }
        if current.frame != self.frame {
            self.frame = current.frame;
            detail(
                "窗口",
                format!(
                    "客户区变化 → {}（标题「{}」）",
                    self.frame.describe(),
                    current.title
                ),
            );
        }
        screencode::win::capture_client(self.target.handle)
    }
}

fn open_capture_source(opts: &Options, screens: Vec<Screen>) -> Result<CaptureSource> {
    if opts.fullscreen {
        let screen = screens
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("没有找到屏幕"))?;
        log(
            "捕获",
            format!(
                "来源: 整屏 #{}（{}x{}）",
                screen.display_info.id, screen.display_info.width, screen.display_info.height
            ),
        );
        return Ok(CaptureSource::Screen { screen });
    }

    #[cfg(windows)]
    {
        let target = screencode::win::find(Some(&opts.process), opts.window.as_deref()).map_err(|e| {
            anyhow!(
                "没有找到匹配的窗口（进程 {}）：{}。可用 --list-windows 查看，或加 --fullscreen 抓整屏",
                opts.process,
                e
            )
        })?;
        detail("窗口", format!("匹配到: {}", target.describe()));

        if target.minimized {
            return Err(anyhow!(
                "目标窗口处于最小化状态，后台截图要求窗口非最小化，请先还原窗口: {}",
                target.describe()
            ));
        }
        if target.frame.is_empty() {
            return Err(anyhow!(
                "目标窗口客户区为空（可能被隐藏），无法截图: {}",
                target.describe()
            ));
        }

        log(
            "捕获",
            format!(
                "来源: 进程 {}「{}」客户区 {}（后台截图，窗口在前台或后台都可以）",
                target.process,
                target.title,
                target.frame.describe()
            ),
        );
        log(
            "提示",
            format!(
                "发送端请用 --frame {}x{}（与客户区一致）生成，才能 1:1 播放",
                target.frame.width, target.frame.height
            ),
        );

        Ok(CaptureSource::Window(WindowCapture {
            frame: target.frame,
            target,
        }))
    }

    #[cfg(not(windows))]
    {
        let _ = screens;
        Err(anyhow!(
            "窗口捕获只支持 Windows；其它平台请用 --fullscreen 抓整屏"
        ))
    }
}

fn list_windows_report() -> Result<()> {
    #[cfg(windows)]
    {
        let windows = screencode::win::list_windows()?;
        say(format!(
            "共 {} 个可见窗口（按客户区面积排序）：",
            windows.len()
        ));
        for window in &windows {
            let line = format!(
                "  {} | 进程 {} | 标题「{}」| 客户区 {}{}",
                if window.minimized {
                    "最小化"
                } else {
                    "正常  "
                },
                window.process,
                window.title,
                window.frame.describe(),
                if !window.visible { " | 不可见" } else { "" }
            );
            println!("{}", line);
            append_log_file(&line);
        }
        say("");
        say("按进程抓: ./server --process <进程名>   按标题抓: ./server --window <标题片段>");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(anyhow!("窗口捕获只支持 Windows；其它平台请用 --fullscreen"))
    }
}

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

fn parse_options(args: &[String]) -> Result<Options> {
    let mut opts = Options::default();
    let mut iter = args.iter().skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", usage_text());
                std::process::exit(0);
            }
            "--resume" => opts.resume = true,
            "--clean" => opts.clean = true,
            "-v" | "--verbose" => opts.verbose = true,
            "--list-windows" => opts.list_windows = true,
            "--fullscreen" => opts.fullscreen = true,
            "--interval-ms" => opts.interval_ms = parse_u64(iter.next(), "--interval-ms")?,
            other if other.starts_with("--interval-ms=") => {
                opts.interval_ms = parse_u64(
                    Some(&other["--interval-ms=".len()..].to_string()),
                    "--interval-ms",
                )?
            }
            "--recheck-ms" => opts.recheck_ms = parse_u64(iter.next(), "--recheck-ms")?,
            other if other.starts_with("--recheck-ms=") => {
                opts.recheck_ms = parse_u64(
                    Some(&other["--recheck-ms=".len()..].to_string()),
                    "--recheck-ms",
                )?
            }
            "--process" => {
                opts.process = non_empty(iter.next(), "--process")?.to_string();
            }
            other if other.starts_with("--process=") => {
                opts.process =
                    non_empty_str(&other["--process=".len()..], "--process")?.to_string();
            }
            "--window" => {
                opts.window = Some(non_empty(iter.next(), "--window")?.to_string());
            }
            other if other.starts_with("--window=") => {
                opts.window =
                    Some(non_empty_str(&other["--window=".len()..], "--window")?.to_string());
            }
            "--idle-timeout" => {
                opts.idle_timeout =
                    parse_idle(Some(&non_empty(iter.next(), "--idle-timeout")?.to_string()))?;
            }
            other if other.starts_with("--idle-timeout=") => {
                opts.idle_timeout =
                    parse_idle(Some(&other["--idle-timeout=".len()..].to_string()))?;
            }
            "--rect" => {
                opts.rect = Some(parse_rect(non_empty(iter.next(), "--rect")?)?);
            }
            other if other.starts_with("--rect=") => {
                opts.rect = Some(parse_rect(&other["--rect=".len()..])?);
            }
            "--out" => {
                opts.out = Some(non_empty(iter.next(), "--out")?.to_string());
            }
            other if other.starts_with("--out=") => {
                opts.out = Some(non_empty_str(&other["--out=".len()..], "--out")?.to_string());
            }
            other if other.starts_with('-') => {
                return Err(anyhow!("未知参数 '{}'", other));
            }
            other => {
                return Err(anyhow!("多余的参数 '{}'（server 不接受位置参数）", other));
            }
        }
    }

    if opts.idle_timeout == Some(Duration::ZERO) {
        opts.idle_timeout = None;
    }
    Ok(opts)
}

fn parse_u64(value: Option<&String>, name: &str) -> Result<u64> {
    let value = value.ok_or_else(|| anyhow!("{} 缺少参数值", name))?;
    value
        .trim()
        .parse()
        .map_err(|_| anyhow!("{} 需要数字，收到 '{}'", name, value))
}

fn non_empty<'a>(value: Option<&'a String>, name: &str) -> Result<&'a str> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value.as_str()),
        _ => Err(anyhow!("{} 不能为空", name)),
    }
}

fn non_empty_str<'a>(value: &'a str, name: &str) -> Result<&'a str> {
    if value.trim().is_empty() {
        Err(anyhow!("{} 不能为空", name))
    } else {
        Ok(value)
    }
}

fn parse_idle(value: Option<&String>) -> Result<Option<Duration>> {
    let value = value.ok_or_else(|| anyhow!("--idle-timeout 缺少参数值"))?;
    let seconds: f64 = value
        .trim()
        .parse()
        .map_err(|_| anyhow!("--idle-timeout 需要数字（秒），收到 '{}'", value))?;
    if seconds <= 0.0 {
        Ok(None)
    } else {
        Ok(Some(Duration::from_secs_f64(seconds)))
    }
}

fn parse_rect(value: &str) -> Result<Rect> {
    let parts: Vec<&str> = value.split(',').map(|s| s.trim()).collect();
    if parts.len() != 4 {
        return Err(anyhow!("--rect 需要 x,y,w,h，收到 '{}'", value));
    }
    let mut nums = [0u32; 4];
    for (i, part) in parts.iter().enumerate() {
        nums[i] = part
            .parse()
            .map_err(|_| anyhow!("--rect 第 {} 项不是数字: '{}'", i + 1, part))?;
    }
    if nums[2] == 0 || nums[3] == 0 {
        return Err(anyhow!("--rect 的宽高必须大于 0"));
    }
    Ok(Rect {
        x: nums[0],
        y: nums[1],
        w: nums[2],
        h: nums[3],
    })
}

fn usage_text() -> String {
    [
        "ScreenCode 接收端（单张高密度网格码）",
        "",
        "用法:",
        "  server [选项]",
        "",
        "选项:",
        "  --resume                继续上次的分片库",
        "  --clean                 清除分片库后退出",
        "  --process NAME          按进程名抓窗口（默认 HSR）",
        "  --window TITLE          按窗口标题片段抓窗口",
        "  --fullscreen            抓整块屏幕",
        "  --interval-ms N         截屏间隔（默认 50）",
        "  --recheck-ms N          画面不变时也重解的间隔（默认 100）",
        "  --idle-timeout N        无新分片多少秒后停止（0 表示永不）",
        "  --rect x,y,w,h          手工指定码框（自动定位失败时用）",
        "  --out NAME              输出文件名",
        "  --list-windows          列出所有可见窗口后退出",
        "  -v, --verbose           控制台也输出每帧细节",
        "  -h, --help              显示帮助",
        "",
        "示例:",
        "  server",
        "  server --resume",
        "  server --fullscreen --rect 8,8,2488,1312",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_defaults_and_flags() {
        let opts = parse_options(&args(&["server", "--resume", "-v"])).expect("parse");
        assert!(opts.resume);
        assert!(opts.verbose);
        assert_eq!(opts.process, "HSR");
        assert_eq!(opts.interval_ms, 50);
    }

    #[test]
    fn parses_rect_and_timeout() {
        let opts = parse_options(&args(&[
            "server",
            "--rect=10,20,300,400",
            "--idle-timeout",
            "10",
            "--process=game",
        ]))
        .expect("parse");
        assert_eq!(
            opts.rect,
            Some(Rect {
                x: 10,
                y: 20,
                w: 300,
                h: 400
            })
        );
        assert_eq!(opts.idle_timeout, Some(Duration::from_secs(10)));
        assert_eq!(opts.process, "game");
    }

    #[test]
    fn idle_zero_means_never() {
        let opts = parse_options(&args(&["server", "--idle-timeout", "0"])).expect("parse");
        assert_eq!(opts.idle_timeout, None);
    }

    #[test]
    fn rejects_bad_rect() {
        assert!(parse_options(&args(&["server", "--rect", "1,2,3"])).is_err());
        assert!(parse_options(&args(&["server", "--rect", "1,2,0,4"])).is_err());
        assert!(parse_options(&args(&["server", "--rect", "a,2,3,4"])).is_err());
    }

    #[test]
    fn rejects_empty_process() {
        assert!(parse_options(&args(&["server", "--process="])).is_err());
        assert!(parse_options(&args(&["server", "--window="])).is_err());
    }

    #[test]
    fn civil_dates_are_sane() {
        let (y, m, d) = civil_from_days(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }
}
