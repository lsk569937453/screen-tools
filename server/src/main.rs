use anyhow::{anyhow, Result};
use base64::Engine;
use image::GrayImage;
use screenshots::Screen;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

mod win_capture;

use win_capture::{Frame, WindowInfo};

const CAPTURE_INTERVAL_MS: u64 = 50; // 截屏间隔（毫秒）
const IDLE_TIMEOUT_SECONDS: u64 = 3; // 无新数据超时时间（秒）
const SAVE_FILE: &str = ".qr_session.json"; // 会话保存文件
const SAVE_INTERVAL_SECONDS: u64 = 1; // 内存会话定时落盘间隔（秒）
const LOG_DIR: &str = "logs"; // 日志目录
/// 默认抓取的进程名（部分匹配，能命中 HSRClient）；用 `--process` 或 `--fullscreen` 覆盖
const DEFAULT_PROCESS: &str = "HSR";

/// 带序号的数据块
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Chunk {
    seq: usize,
    total: usize,
    data: String,
}

/// 会话数据
#[derive(Debug, Serialize, Deserialize)]
struct Session {
    chunks: HashMap<usize, Chunk>,
    expected_total: Option<usize>,
}

/// 命令行选项
struct Options {
    resume: bool,
    clean: bool,
    verbose: bool,
    interval_ms: u64,
    /// 按进程名抓窗口（不含 .exe，大小写不敏感的部分匹配）；默认 `HSR`
    process: Option<String>,
    /// 按窗口标题抓窗口（大小写不敏感的部分匹配）
    window: Option<String>,
    /// 抓整块屏幕（默认按进程抓窗口）
    fullscreen: bool,
    /// 只列出所有可见窗口后退出
    list_windows: bool,
    /// 无新片段多久后结束；`None` 表示永不超时
    idle_timeout: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            resume: false,
            clean: false,
            verbose: false,
            interval_ms: CAPTURE_INTERVAL_MS,
            process: Some(DEFAULT_PROCESS.to_string()),
            window: None,
            fullscreen: false,
            list_windows: false,
            idle_timeout: Some(Duration::from_secs(IDLE_TIMEOUT_SECONDS)),
        }
    }
}

/// 一次截屏的检测结果
///
/// 注意：分块检测的瓦片互有重叠、同一张码可能被多条路径重复看到，
/// 所以这里分别记录各路径的数字，而不是简单相加——"失败次数"并不等于"漏了几张码"。
struct Batch {
    /// 成功解码出的载荷（多路取并集并去重）
    payloads: Vec<String>,
    /// 整图检测的网格数（主路径，最接近"这一屏有几张码"）
    full_grids: usize,
    /// 整图检测解码成功的张数
    full_decoded: usize,
    /// 分块检测汇总：网格数与解码成功次数（瓦片重叠，会重复计）
    tile_grids: usize,
    tile_decoded: usize,
    /// 放大重试汇总：网格数与解码成功张数
    retry_grids: usize,
    retry_decoded: usize,
    /// 分块 / 放大重试比整图多补回来的张数（已去重）
    extra: usize,
    /// 本帧是否做过放大重试
    retried: bool,
}

impl Batch {
    fn empty() -> Self {
        Self {
            payloads: Vec::new(),
            full_grids: 0,
            full_decoded: 0,
            tile_grids: 0,
            tile_decoded: 0,
            retry_grids: 0,
            retry_decoded: 0,
            extra: 0,
            retried: false,
        }
    }

    /// 整图里检测到、但没解出来的张数（理论上应该补回来的量）
    fn full_missed(&self) -> usize {
        self.full_grids.saturating_sub(self.full_decoded)
    }
}

/// 解码异常提示只打一次，避免每帧刷屏
static PANIC_WARNED: AtomicBool = AtomicBool::new(false);

/// 把 panic 收敛成一行提示（rqrr 的断言 panic 已在检测处隔离）
fn warn_panic_once(location: &str) {
    if !PANIC_WARNED.swap(true, Ordering::Relaxed) {
        log(
            "警告",
            format!(
                "内部解码异常（已跳过该区域）: {}",
                if location.is_empty() {
                    "未知位置"
                } else {
                    location
                }
            ),
        );
        log(
            "警告",
            "若频繁出现，通常是屏幕上的二维码太小，请减少每屏张数",
        );
    }
}

/// 解码并行度上限
///
/// 接收端通常和播放页面跑在同一台机器上，线程开太满会拖慢页面渲染。
const MAX_WORKER_THREADS: usize = 8;

/// 计算解码用的工作线程数
fn worker_threads() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, MAX_WORKER_THREADS)
}

/// 一次批处理的入库结果
struct BatchOutcome {
    /// 本次新增的片段序号（按出现顺序）
    new_seqs: Vec<usize>,
    /// 总数与预期不符的张数
    conflicts: usize,
    /// 解析失败的错误信息
    errors: Vec<String>,
}

/// 把新增序号列表压缩成一行日志
fn summarize_seqs(seqs: &[usize]) -> String {
    const LIMIT: usize = 10;
    let list: Vec<String> = seqs
        .iter()
        .take(LIMIT)
        .map(|seq| format!("#{}", seq))
        .collect();

    if seqs.len() <= LIMIT {
        format!("[{}]", list.join(","))
    } else {
        format!("[{},…] 共 {} 个", list.join(","), seqs.len())
    }
}

/// 缺失片段的预览：最多列 20 个，超出时用省略号表示（总数由调用方另行打印）
fn summarize_missing(missing: &[usize]) -> String {
    const LIMIT: usize = 20;
    let preview: Vec<String> = missing
        .iter()
        .take(LIMIT)
        .map(|seq| seq.to_string())
        .collect();

    if missing.len() <= LIMIT {
        format!("[{}]", preview.join(","))
    } else {
        format!("[{},…]", preview.join(","))
    }
}

/// 进程启动时刻：每行日志前缀打印相对运行时长，便于和轮播节奏对照
static RUN_START: OnceLock<Instant> = OnceLock::new();

/// 日志文件（`logs/server-<时间戳>.log`）；未初始化或写入失败时只走控制台
static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// 当前日志文件路径
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 本机时间 `YYYY-MM-DD HH:MM:SS.mmm`
#[cfg(windows)]
fn wall_clock() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut time: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut time) };

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

/// 非 Windows 平台没有零依赖的本地时间 API，退化成 Unix 秒数（仍可定位先后顺序）
#[cfg(not(windows))]
fn wall_clock() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{}.000", secs)
}

/// 日志文件名里的时间戳：`20261117-220634`
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

/// 创建 `logs/server-<时间戳>.log`，之后所有日志同时写入文件
fn init_log_file(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)
        .map_err(|e| anyhow!("创建日志目录 {} 失败: {}", dir.display(), e))?;

    let path = dir.join(format!("server-{}.log", log_file_stamp()));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow!("创建日志文件 {} 失败: {}", path.display(), e))?;

    let _ = LOG_FILE.set(Mutex::new(Some(file)));
    let _ = LOG_PATH.set(path.clone());
    Ok(path)
}

/// 控制台是否也输出细节（`-v` 时打开）；日志文件永远输出全部细节
static CONSOLE_VERBOSE: AtomicBool = AtomicBool::new(false);

/// 追加一行到日志文件；未启用文件日志时静默跳过
fn append_log_file(line: &str) {
    let Some(lock) = LOG_FILE.get() else {
        return;
    };
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.as_mut() {
        // 逐行 flush：进程被强杀时也能看到最后发生的事
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

/// 关键信息：控制台 + 日志文件双写（启动、配置、进度、超时、完成、错误、警告）
fn log(kind: &str, message: impl std::fmt::Display) {
    let line = format_log_line(kind, &message.to_string());
    println!("{}", line);
    append_log_file(&line);
}

/// 细节信息：只写日志文件；加 `-v` 时控制台也打印
///
/// 用于每秒心跳、每帧识别明细、调试计数、窗口几何变化、逐片段入库等
/// 「排查时才需要」的内容，避免把控制台刷满。
fn detail(kind: &str, message: impl std::fmt::Display) {
    let line = format_log_line(kind, &message.to_string());
    if CONSOLE_VERBOSE.load(Ordering::Relaxed) {
        println!("{}", line);
    }
    append_log_file(&line);
}

/// 普通输出（横幅、清单等）：同样写入日志文件
fn say(message: impl std::fmt::Display) {
    let text = message.to_string();
    println!("{}", text);
    append_log_file(&text);
}

/// 当前运行时长
fn uptime() -> Duration {
    RUN_START.get_or_init(Instant::now).elapsed()
}

/// 文件大小的可读描述（读不到时返回"大小未知"）
fn describe_file_size(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(meta) => format_file_size(meta.len() as usize),
        Err(_) => "大小未知".to_string(),
    }
}

/// 运行期统计（仅用于日志，不影响业务逻辑）
#[derive(Default)]
struct Stats {
    /// 主循环轮数
    loops: u64,
    /// 成功截屏帧数
    captured: u64,
    /// 截屏失败次数
    capture_errors: u64,
    /// 与上一帧完全相同、被跳过的帧数
    frames_unchanged: u64,
    /// 真正做过二维码识别的帧数
    frames_scanned: u64,
    /// 检测到至少一个二维码网格的帧数
    frames_with_grids: u64,
    /// 整图检测到的网格总数（最接近"扫过多少张码"）
    grids: u64,
    /// 累计解码成功的二维码张数（多路去重后）
    decoded: u64,
    /// 整图检测到但没解出的张数（可能被分块/放大补回）
    full_missed: u64,
    /// 分块 / 放大重试额外补回的张数
    extra: u64,
    /// 片段数据解析失败次数
    parse_errors: u64,
    /// 本次运行新入库的片段数
    new_seqs: u64,
    /// 屏幕上重复出现、但已经收集过的二维码张数
    duplicate_payloads: u64,
    /// 截屏累计耗时（毫秒）
    capture_ms: u128,
    /// 识别累计耗时（毫秒）
    detect_ms: u128,
}

impl Stats {
    fn avg_capture_ms(&self) -> f64 {
        if self.captured == 0 {
            0.0
        } else {
            self.capture_ms as f64 / self.captured as f64
        }
    }

    fn avg_detect_ms(&self) -> f64 {
        if self.frames_scanned == 0 {
            0.0
        } else {
            self.detect_ms as f64 / self.frames_scanned as f64
        }
    }

    /// 每秒一次的心跳日志
    fn report(
        &self,
        uptime: Duration,
        received: usize,
        total: Option<usize>,
        idle: Duration,
    ) -> String {
        format!(
            "运行 {:.1}s | 循环 {} 次 | 截屏 {} 帧（失败 {}，未变化 {}） | 识别 {} 帧: 整图网格 {} 解码 {}（漏解 {}，补回 {}） | 均值 截屏 {:.1}ms 识别 {:.1}ms | 片段 {}/{}（本次 +{}，重复 {}） | 空闲 {:.1}s",
            uptime.as_secs_f64(),
            self.loops,
            self.captured,
            self.capture_errors,
            self.frames_unchanged,
            self.frames_scanned,
            self.grids,
            self.decoded,
            self.full_missed,
            self.extra,
            self.avg_capture_ms(),
            self.avg_detect_ms(),
            received,
            total.map(|t| t.to_string()).unwrap_or_else(|| "?".to_string()),
            self.new_seqs,
            self.duplicate_payloads,
            idle.as_secs_f64(),
        )
    }
}

fn main() {
    // 尽早开启 DPI 感知：窗口坐标与截图像素必须处于同一坐标系
    let dpi_aware = win_capture::enable_dpi_awareness();

    // rqrr 的断言 panic 已在检测处隔离，这里把默认的 panic 输出收敛成一行提示，
    // 避免每帧刷屏
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()))
            .unwrap_or_default();
        warn_panic_once(&location);
    }));

    let args: Vec<String> = std::env::args().collect();

    if let Err(e) = main_with_error(&args, dpi_aware) {
        log("错误", &e);
        say("\n接收失败，详见上方日志与 logs/ 目录");
        std::process::exit(1);
    }
}

fn main_with_error(args: &[String], dpi_aware: bool) -> Result<()> {
    // 日志时间前缀从逻辑起点开始计算
    RUN_START.get_or_init(Instant::now);

    // 解析命令行参数
    let opts = parse_options(args)?;
    let is_resume = opts.resume;
    CONSOLE_VERBOSE.store(opts.verbose, Ordering::Relaxed);

    // 日志文件先就位：之后所有输出都是「控制台 + 文件」双写
    match init_log_file(Path::new(LOG_DIR)) {
        Ok(path) => {
            if let Ok(dir) = std::env::current_dir() {
                log("日志", format!("日志文件: {}", dir.join(&path).display()));
            } else {
                log("日志", format!("日志文件: {}", path.display()));
            }
        }
        Err(e) => {
            // 日志文件写不了不能影响接收本身，退回只输出到控制台
            println!("警告: {}，日志只会输出到控制台", e);
        }
    }

    detail(
        "信息",
        format!(
            "DPI 感知: {}（窗口捕获与截图坐标需要处于同一坐标系）",
            if dpi_aware { "已开启" } else { "已由系统设置" }
        ),
    );

    // 清除保存的会话
    if opts.clean {
        if PathBuf::from(SAVE_FILE).exists() {
            fs::remove_file(SAVE_FILE)?;
            log("会话", format!("已清除保存的会话 {}", SAVE_FILE));
        } else {
            log("会话", "没有可清除的会话文件");
        }
        return Ok(());
    }

    // 只列窗口：排查"到底该匹配哪个进程/标题"
    if opts.list_windows {
        return list_windows_report();
    }

    say("╔════════════════════════════════════════╗");
    say("║       QR 码文件接收器                  ║");
    say("╚════════════════════════════════════════╝");

    // 打印会话文件的绝对路径，避免从不同工作目录启动时误以为没有保存
    if let Ok(dir) = std::env::current_dir() {
        say(format!("会话文件: {}", dir.join(SAVE_FILE).display()));
    }
    say("");

    // 尝试加载之前的会话
    let (mut received_chunks, mut expected_total) = if is_resume {
        match load_session() {
            Ok(session) => {
                let count = session.chunks.len();
                // 缺失片段只算一次：首行统计和"前 20 个"都用它
                let missing = session
                    .expected_total
                    .map(|total| (total, find_missing_chunks(&session.chunks, total)));

                log(
                    "会话",
                    format!(
                        "续传: 已收集 {} 个片段{}",
                        count,
                        match &missing {
                            Some((total, list)) => {
                                format!("，预期 {}，缺失 {}", total, list.len())
                            }
                            None => "（上次没有记录总数）".to_string(),
                        }
                    ),
                );

                match &missing {
                    Some((total, list)) if list.is_empty() => log(
                        "会话",
                        format!("没有缺失片段（已收集 {}/{}），可以直接开始接收", count, total),
                    ),
                    Some((_, list)) if list.len() <= 20 => {
                        log("会话", format!("缺失片段: {}", summarize_missing(list)))
                    }
                    Some((_, list)) => log(
                        "会话",
                        format!(
                            "缺失片段(前20个): {}（共 {} 个）",
                            summarize_missing(list),
                            list.len()
                        ),
                    ),
                    None => log(
                        "会话",
                        "上次没有记录总数，暂时无法列出缺失片段（收到首个码后即可确定）",
                    ),
                }

                detail(
                    "会话",
                    format!(
                        "会话文件 {}（{}）",
                        SAVE_FILE,
                        describe_file_size(Path::new(SAVE_FILE))
                    ),
                );

                // 校验会话自身的一致性：总数/序号矛盾会让"怎么扫都补不齐"
                if !session.chunks.is_empty() {
                    let mut totals: Vec<usize> =
                        session.chunks.values().map(|chunk| chunk.total).collect();
                    totals.sort_unstable();
                    totals.dedup();
                    if totals.len() > 1 {
                        log("警告", format!("会话里的片段总数不一致: {:?}", totals));
                    }

                    let mut seqs: Vec<usize> = session.chunks.keys().copied().collect();
                    seqs.sort_unstable();
                    let max_seq = seqs.last().copied().unwrap_or(0);
                    let max_total = totals.iter().copied().max().unwrap_or(0);
                    detail(
                        "会话",
                        format!(
                            "片段序号范围: #{} ~ #{}；其中数据长度最小 {} / 最大 {} 字符",
                            seqs.first().copied().unwrap_or(0),
                            max_seq,
                            session
                                .chunks
                                .values()
                                .map(|chunk| chunk.data.len())
                                .min()
                                .unwrap_or(0),
                            session
                                .chunks
                                .values()
                                .map(|chunk| chunk.data.len())
                                .max()
                                .unwrap_or(0)
                        ),
                    );

                    if max_total > 0 && max_seq > max_total {
                        log(
                            "警告",
                            format!("会话中有序号超出总数的片段: seq={} > total={}", max_seq, max_total),
                        );
                    }
                }

                (session.chunks, session.expected_total)
            }
            Err(e) => {
                log(
                    "警告",
                    format!("无法加载会话 {}: {}，将重新开始", SAVE_FILE, e),
                );
                (HashMap::new(), None)
            }
        }
    } else {
        // 检查是否有旧会话
        if PathBuf::from(SAVE_FILE).exists() {
            log(
                "提示",
                format!(
                    "发现之前的会话 {}（{}），使用 --resume 参数可继续接收",
                    SAVE_FILE,
                    describe_file_size(Path::new(SAVE_FILE))
                ),
            );
        }
        (HashMap::new(), None)
    };

    say("");
    log("信息", "开始监听二维码");

    log(
        "配置",
        format!(
            "截屏间隔 {} ms | 空闲超时 {} | 会话落盘间隔 {} s | 详细日志 {}",
            opts.interval_ms,
            match opts.idle_timeout {
                Some(limit) => format!("{} s", limit.as_secs()),
                None => "关闭（--idle-timeout 0）".to_string(),
            },
            SAVE_INTERVAL_SECONDS,
            if opts.verbose {
                "开"
            } else {
                "关（加 -v 查看每帧明细）"
            }
        ),
    );

    // 列出所有屏幕：多屏场景下一眼看出窗口/整屏落在哪块屏
    let screens = Screen::all().map_err(|e| anyhow!("获取屏幕失败: {}", e))?;
    detail("信息", format!("检测到 {} 块屏幕", screens.len()));
    for (index, item) in screens.iter().enumerate() {
        let info = &item.display_info;
        detail(
            "屏幕",
            format!(
                "#{} id={} 位置=({},{}) 尺寸={}x{} 缩放={:.2} 主屏={}",
                index + 1,
                info.id,
                info.x,
                info.y,
                info.width,
                info.height,
                info.scale_factor,
                if info.is_primary { "是" } else { "否" }
            ),
        );
    }

    let mut source = open_capture_source(&opts, screens)?;

    let mut stats = Stats::default();
    let mut last_data_time = Instant::now();
    let mut last_frame_change = Instant::now();
    let mut session_dirty = false;
    let mut last_save = Instant::now();
    let mut last_report = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_no_grid_warn: Option<Instant> = None;
    let mut last_decode_fail_warn: Option<Instant> = None;
    let mut last_frozen_warn: Option<Instant> = None;
    let mut first_capture_logged = false;
    // 上一帧的原始像素，用于跳过"画面没变"的重复检测
    let mut last_frame: Vec<u8> = Vec::new();

    loop {
        stats.loops += 1;

        // 每秒一次的状态心跳：只进日志文件（-v 时也打到控制台）
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            detail(
                "统计",
                stats.report(
                    uptime(),
                    received_chunks.len(),
                    expected_total,
                    last_data_time.elapsed(),
                ),
            );

            // 控制台每 10 秒给一行进度，长时间传输时能看到还在动
            if last_progress.elapsed() >= Duration::from_secs(10) {
                last_progress = Instant::now();
                log(
                    "进度",
                    format!(
                        "已收集 {}/{}（本次 +{}，运行 {:.0}s）",
                        received_chunks.len(),
                        expected_total
                            .map(|total| total.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        stats.new_seqs,
                        uptime().as_secs_f64()
                    ),
                );
            }

            // 一张网格都没检测到：多半是页面没在播放、不在捕获范围内，或被遮挡
            if stats.frames_with_grids == 0
                && last_data_time.elapsed() >= Duration::from_secs(5)
                && last_no_grid_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(15))
            {
                last_no_grid_warn = Some(Instant::now());
                log(
                    "提示",
                    format!(
                        "已识别 {} 帧但没有任何二维码网格（画面变化 {} 次）：确认轮播页面正在播放、位于捕获范围内且未被遮挡",
                        stats.frames_scanned,
                        stats.captured.saturating_sub(stats.frames_unchanged)
                    ),
                );
            }

            // 画面长时间完全不变：轮播停了、暂停了，或者捕获到的根本不是页面
            if !last_frame.is_empty()
                && last_frame_change.elapsed() >= Duration::from_secs(5)
                && last_frozen_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
            {
                last_frozen_warn = Some(Instant::now());
                log(
                    "提示",
                    format!(
                        "画面已 {:.1}s 没有任何变化：轮播可能已停止/暂停，或捕获窗口被遮挡",
                        last_frame_change.elapsed().as_secs_f64()
                    ),
                );
            }
        }

        // 截屏（整屏或目标窗口客户区）
        let capture_started = Instant::now();
        let screenshot = match source.capture() {
            Ok(screenshot) => screenshot,
            Err(e) => {
                stats.capture_errors += 1;
                // 截屏失败可能每帧都发生，这里限流输出，避免刷屏掩盖其它日志
                if stats.capture_errors <= 3 || stats.capture_errors % 50 == 0 {
                    log(
                        "错误",
                        format!("截屏失败（累计 {} 次）: {}", stats.capture_errors, e),
                    );
                }
                // 窗口模式下连续失败基本意味着窗口已关闭/最小化，继续重试只会刷屏
                if source.is_window() && stats.capture_errors >= 5 {
                    return Err(anyhow!(
                        "窗口截图连续失败 {} 次，已停止：{}",
                        stats.capture_errors,
                        e
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
        let rgba_data = screenshot.rgba();

        // 首次截屏记录真实分辨率：缩放/多屏问题一看就知道
        if !first_capture_logged {
            first_capture_logged = true;
            log(
                "信息",
                format!(
                    "首次截屏: {}x{}（{} 字节 rgba，耗时 {}ms，来源 {}）",
                    width,
                    height,
                    rgba_data.len(),
                    capture_elapsed.as_millis(),
                    source.describe()
                ),
            );
        }

        // 画面与上一帧完全相同时跳过检测：二维码轮播每 300ms 才翻一页，
        // 中间的重复帧占绝大多数，跳过它们能省下大量 CPU。
        let frame_changed = rgba_data[..] != last_frame[..];
        if frame_changed {
            last_frame.clear();
            last_frame.extend_from_slice(rgba_data);
            last_frame_change = Instant::now();
        } else {
            stats.frames_unchanged += 1;
        }

        if frame_changed {
            let detect_started = Instant::now();
            let detected = detect_from_rgba(rgba_data, width, height, opts.verbose);
            let detect_elapsed = detect_started.elapsed();
            stats.detect_ms += detect_elapsed.as_millis();

            match detected {
                Ok(batch) => {
                    stats.frames_scanned += 1;
                    stats.grids += batch.full_grids as u64;
                    stats.decoded += batch.payloads.len() as u64;
                    stats.full_missed += batch.full_missed() as u64;
                    stats.extra += batch.extra as u64;
                    if batch.full_grids > 0 || !batch.payloads.is_empty() {
                        stats.frames_with_grids += 1;
                    }

                    // 各路径分别是多少：整图是主路径，"分块/放大"只在补漏时才有数字
                    detail(
                        "帧",
                        format!(
                            "{}x{} | 整图 网格 {} / 解码 {} | 分块 网格 {} / 解码 {} | 放大重试 {}（网格 {} / 解码 {}）| 合并解码 {}（额外补回 {}）| 截屏 {}ms 识别 {}ms",
                            width,
                            height,
                            batch.full_grids,
                            batch.full_decoded,
                            batch.tile_grids,
                            batch.tile_decoded,
                            if batch.retried { "是" } else { "否" },
                            batch.retry_grids,
                            batch.retry_decoded,
                            batch.payloads.len(),
                            batch.extra,
                            capture_elapsed.as_millis(),
                            detect_elapsed.as_millis()
                        ),
                    );

                    // 检测到网格却一张都解不出：这是"扫了但没数据"最典型的原因，必须留痕
                    if batch.full_grids > 0
                        && batch.payloads.is_empty()
                        && last_decode_fail_warn
                            .is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
                    {
                        last_decode_fail_warn = Some(Instant::now());
                        log(
                            "警告",
                            format!(
                                "本帧整图检测到 {} 个二维码网格但一张都没解出（累计: 网格 {} / 漏解 {}，补回 {}）：通常是二维码太小、被遮挡或分辨率不足",
                                batch.full_grids, stats.grids, stats.full_missed, stats.extra
                            ),
                        );
                    }

                    // 整图有漏，但分块/放大全补回来了：只是记录，不用告警
                    if batch.full_missed() > 0 && !batch.payloads.is_empty() {
                        detail(
                            "调试",
                            format!(
                                "整图漏解 {} 张，分块/放大补回 {} 张，本帧最终拿到 {} 张{}",
                                batch.full_missed(),
                                batch.extra,
                                batch.payloads.len(),
                                if batch.payloads.len() >= batch.full_grids {
                                    "（已补齐）"
                                } else {
                                    "（仍缺，可能二维码太小）"
                                }
                            ),
                        );
                    }

                    if !batch.payloads.is_empty() {
                        let had_total = expected_total.is_some();
                        let outcome = process_payloads(
                            &batch.payloads,
                            &mut received_chunks,
                            &mut expected_total,
                        );

                        stats.parse_errors += outcome.errors.len() as u64;

                        // 首次确定总数
                        if !had_total {
                            if let Some(total) = expected_total {
                                log("信息", format!("总片段数: {}", total));
                                session_dirty = true;
                            }
                        }

                        for line in &outcome.errors {
                            log("警告", line);
                        }

                        if outcome.conflicts > 0 {
                            log(
                                "警告",
                                format!(
                                    "本帧有 {} 张码的总数与预期 ({}) 不符",
                                    outcome.conflicts,
                                    expected_total.unwrap_or(0)
                                ),
                            );
                        }

                        if !outcome.new_seqs.is_empty() {
                            last_data_time = Instant::now();
                            session_dirty = true;
                            stats.new_seqs += outcome.new_seqs.len() as u64;

                            for &seq in &outcome.new_seqs {
                                if let Some(chunk) = received_chunks.get(&seq) {
                                    detail(
                                        "收到",
                                        format!(
                                            "#{:>5} | {} 字符 | 总数 {}",
                                            seq,
                                            chunk.data.len(),
                                            chunk.total
                                        ),
                                    );
                                }
                            }

                            detail(
                                "批次",
                                format!(
                                    "本帧 {} 张码 → 新增 {} 个片段 {} | 已收集: {}/{}",
                                    batch.payloads.len(),
                                    outcome.new_seqs.len(),
                                    summarize_seqs(&outcome.new_seqs),
                                    received_chunks.len(),
                                    expected_total.unwrap_or(0)
                                ),
                            );
                        } else {
                            stats.duplicate_payloads += batch.payloads.len() as u64;
                            if is_resume {
                                // 续传时屏幕上仍停在已收集的片段，说明发送端仍在工作；
                                // 不能因为“没有新片段”而超时退出，继续等待缺失片段。
                                last_data_time = Instant::now();
                            }
                            detail(
                                "批次",
                                format!(
                                    "本帧 {} 张码全部是已收集片段 | 已收集: {}/{}",
                                    batch.payloads.len(),
                                    received_chunks.len(),
                                    expected_total.unwrap_or(0)
                                ),
                            );
                        }
                    }
                }
                Err(e) => {
                    log("错误", format!("识别失败: {}", e));
                }
            }
        }

        // 检查是否接收完成
        if let Some(total) = expected_total {
            if received_chunks.len() == total {
                say("");
                log(
                    "完成",
                    format!("所有片段已接收完成！({}/{})", received_chunks.len(), total),
                );
                break;
            }
        }

        // 检查是否超时（没有新数据）
        let idle_timeout = opts.idle_timeout;
        let idle_expired = idle_timeout.is_some_and(|limit| last_data_time.elapsed() > limit);
        if !received_chunks.is_empty() && idle_expired {
            if let Some(total) = expected_total {
                say("");
                log(
                    "超时",
                    format!(
                        "{} 内没有收到新片段（本次新增 {} 个），已收集 {}/{}",
                        idle_timeout
                            .map(|limit| format!("{:.1}s", limit.as_secs_f64()))
                            .unwrap_or_else(|| "-".to_string()),
                        stats.new_seqs,
                        received_chunks.len(),
                        total
                    ),
                );

                // 把最可能的原因直接写出来，省得再去猜
                if stats.frames_with_grids == 0 {
                    log(
                        "提示",
                        "整个过程没有检测到任何二维码网格：轮播页面可能没在播放、不在捕获范围内或被遮挡",
                    );
                } else if stats.decoded == 0 {
                    log(
                        "提示",
                        format!(
                            "检测到 {} 个网格但一张都没解出：二维码太小，建议把每屏张数调到 1~2 张后重播",
                            stats.grids
                        ),
                    );
                } else if stats.new_seqs == 0 {
                    log(
                        "提示",
                        "屏幕上出现的都是已收集片段：请让发送端跳到缺失片段（或勾选循环播放）后继续",
                    );
                }

                // 如果有缺失片段，保存会话并显示缺失信息
                if received_chunks.len() < total {
                    save_session(&received_chunks, expected_total)?;
                    log(
                        "保存",
                        format!(
                            "会话已写入 {}（{}）",
                            SAVE_FILE,
                            describe_file_size(Path::new(SAVE_FILE))
                        ),
                    );

                    let missing = find_missing_chunks(&received_chunks, total);
                    say("");
                    say("═ 缺失片段详情 ═");
                    say(format!(
                        "缺失 {} 个片段: {}",
                        missing.len(),
                        summarize_missing(&missing)
                    ));
                    say("");
                    say(format!("会话已保存到: {}", SAVE_FILE));
                    say("继续接收: ./server --resume   （建议加 --idle-timeout 0 长时间等待）");
                    say("");
                }
            }
            break;
        }

        // 定时把内存中的会话写入磁盘，避免进程被强杀时丢失已收集片段
        if session_dirty && last_save.elapsed() >= Duration::from_secs(SAVE_INTERVAL_SECONDS) {
            match save_session(&received_chunks, expected_total) {
                Ok(()) => {
                    session_dirty = false;
                    last_save = Instant::now();
                    detail(
                        "保存",
                        format!(
                            "已写入 {} 个片段 → {}（{}）",
                            received_chunks.len(),
                            SAVE_FILE,
                            describe_file_size(Path::new(SAVE_FILE))
                        ),
                    );
                }
                Err(e) => {
                    // 保留 dirty，下一秒再试；同时避免每个循环都刷屏
                    last_save = Instant::now();
                    log("警告", format!("定时保存会话失败: {}", e));
                }
            }
        }

        thread::sleep(Duration::from_millis(opts.interval_ms));
    }

    say("");
    log(
        "汇总",
        format!(
            "运行 {:.1}s | 截屏 {} 帧（失败 {}，未变化 {}）| 识别 {} 帧 | 整图网格 {} / 解码 {} / 漏解 {}（补回 {}）/ 解析错误 {} | 本次新增 {} 个片段 | 当前 {}/{}",
            uptime().as_secs_f64(),
            stats.captured,
            stats.capture_errors,
            stats.frames_unchanged,
            stats.frames_scanned,
            stats.grids,
            stats.decoded,
            stats.full_missed,
            stats.extra,
            stats.parse_errors,
            stats.new_seqs,
            received_chunks.len(),
            expected_total
                .map(|t| t.to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
    );

    // 重组数据
    if received_chunks.is_empty() {
        log("错误", "没有收到任何片段，无法重组文件");
        return Err(anyhow!("未接收到任何数据"));
    }

    restore_file(&received_chunks, expected_total)?;

    // 成功接收后删除会话文件
    if PathBuf::from(SAVE_FILE).exists() {
        let _ = fs::remove_file(SAVE_FILE);
    }

    Ok(())
}

/// 解析命令行选项
///
/// 用法: `server [--resume] [--clean] [--process 名称] [--window 标题] [--list-windows]`
fn parse_options(args: &[String]) -> Result<Options> {
    let mut opts = Options::default();

    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--resume" => opts.resume = true,
            "--clean" => opts.clean = true,
            "-v" | "--verbose" => opts.verbose = true,
            "--list-windows" => opts.list_windows = true,
            "--interval-ms" => {
                let value = next_value(&mut iter, "--interval-ms")?;
                opts.interval_ms = parse_interval_ms(value)?;
            }
            other if other.starts_with("--interval-ms=") => {
                opts.interval_ms = parse_interval_ms(&other["--interval-ms=".len()..])?;
            }
            "--process" => opts.process = Some(next_value(&mut iter, "--process")?.to_string()),
            other if other.starts_with("--process=") => {
                opts.process = Some(other["--process=".len()..].to_string());
            }
            "--window" => opts.window = Some(next_value(&mut iter, "--window")?.to_string()),
            other if other.starts_with("--window=") => {
                opts.window = Some(other["--window=".len()..].to_string());
            }
            "--idle-timeout" => {
                let value = next_value(&mut iter, "--idle-timeout")?;
                opts.idle_timeout = parse_idle_timeout(value)?;
            }
            other if other.starts_with("--idle-timeout=") => {
                opts.idle_timeout = parse_idle_timeout(&other["--idle-timeout=".len()..])?;
            }
            "--fullscreen" | "--screen" => {
                opts.fullscreen = true;
                opts.process = None;
                opts.window = None;
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(anyhow!("未知参数 '{}'", other));
            }
            other => return Err(anyhow!("未知位置参数 '{}'", other)),
        }
    }

    if opts.process.as_deref() == Some("") || opts.window.as_deref() == Some("") {
        return Err(anyhow!("--process / --window 不能为空"));
    }
    // 指定 --fullscreen 之后又写目标窗口，多半是命令行写错了，直接报出来
    if opts.fullscreen && (opts.process.is_some() || opts.window.is_some()) {
        return Err(anyhow!("--fullscreen 不能与 --process / --window 同时使用"));
    }

    Ok(opts)
}

fn print_usage() {
    println!("用法: server [选项]");
    println!();
    println!("接收相关:");
    println!("  --resume             继续上次未完成的会话");
    println!("  --clean              清除保存的会话");
    println!(
        "  --interval-ms <N>    截屏间隔毫秒数（默认 {}）",
        CAPTURE_INTERVAL_MS
    );
    println!(
        "  --idle-timeout <S>   无新片段多少秒后结束（默认 {}，0 表示不超时）",
        IDLE_TIMEOUT_SECONDS
    );
    println!();
    println!(
        "抓取范围（默认按进程 {} 抓窗口，后台截图，不需要窗口在前台）:",
        DEFAULT_PROCESS
    );
    println!("  --process <名称>     按进程名抓窗口（不含 .exe，允许部分匹配）");
    println!("  --window <标题>      按窗口标题抓窗口（允许部分匹配，可与 --process 组合）");
    println!("  --fullscreen         抓整块屏幕");
    println!("  --list-windows       列出当前所有可见窗口后退出（用于确认真实的进程名/标题）");
    println!();
    println!("日志（控制台 + 日志文件双写）:");
    println!(
        "  日志目录固定为 ./{}，控制台只打印关键信息，日志文件记录全部细节",
        LOG_DIR
    );
    println!("  -v, --verbose        控制台也输出每帧识别细节");
}

fn next_value<'a, I>(iter: &mut I, name: &str) -> Result<&'a str>
where
    I: Iterator<Item = &'a String>,
{
    iter.next()
        .map(|value| value.as_str())
        .ok_or_else(|| anyhow!("{} 缺少参数值", name))
}

fn parse_interval_ms(value: &str) -> Result<u64> {
    let parsed: u64 = value
        .trim()
        .parse()
        .map_err(|_| anyhow!("--interval-ms 需要正整数，收到 '{}'", value))?;
    if parsed == 0 {
        return Err(anyhow!("--interval-ms 至少为 1"));
    }
    Ok(parsed)
}

/// `--idle-timeout`：秒数，0 表示永不因空闲结束
fn parse_idle_timeout(value: &str) -> Result<Option<Duration>> {
    let parsed: u64 = value
        .trim()
        .parse()
        .map_err(|_| anyhow!("--idle-timeout 需要非负整数秒，收到 '{}'", value))?;
    if parsed == 0 {
        Ok(None)
    } else {
        Ok(Some(Duration::from_secs(parsed)))
    }
}

/// 截屏来源：整块屏幕，或指定窗口的客户区
enum CaptureSource {
    Screen { screen: Screen },
    Window(WindowCapture),
}

/// 指定窗口的截图状态；每帧重新定位，窗口移动/缩放都能跟上
struct WindowCapture {
    target: WindowInfo,
    frame: Frame,
}

impl CaptureSource {
    fn is_window(&self) -> bool {
        matches!(self, CaptureSource::Window(_))
    }

    fn describe(&self) -> String {
        match self {
            CaptureSource::Screen { screen } => format!(
                "整屏 #{} {}x{}",
                screen.display_info.id, screen.display_info.width, screen.display_info.height
            ),
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
            CaptureSource::Window(state) => state.capture(),
        }
    }
}

impl WindowCapture {
    fn capture(&mut self) -> Result<screenshots::Image> {
        let current = win_capture::locate(self.target.handle)
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

        // 后台截图：窗口被其它窗口遮挡也能拿到真实内容，不改变窗口层级
        win_capture::capture_client(self.target.handle)
    }
}

/// 建立截屏来源：默认按进程抓窗口，`--fullscreen` 时抓整块屏幕
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

    let target = win_capture::find(opts.process.as_deref(), opts.window.as_deref()).map_err(|e| {
        let selector = match (&opts.process, &opts.window) {
            (Some(process), Some(window)) => format!("进程 {} 且标题含「{}」", process, window),
            (Some(process), None) => format!("进程 {}", process),
            (None, Some(window)) => format!("标题含「{}」", window),
            (None, None) => "未指定目标".to_string(),
        };
        anyhow!(
            "没有找到匹配的窗口（{}）：{}。可用 --list-windows 查看所有可见窗口，或加 --fullscreen 抓整屏",
            selector,
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

    Ok(CaptureSource::Window(WindowCapture {
        target: target.clone(),
        frame: target.frame,
    }))
}

/// `--list-windows`：把可见窗口打到控制台和日志里
fn list_windows_report() -> Result<()> {
    let windows = win_capture::list_windows()?;
    say(format!(
        "共 {} 个可见窗口（按客户区面积排序）：",
        windows.len()
    ));
    for window in &windows {
        let line = format!(
            "  {} | 进程 {} | 标题「{}」| 客户区 {}{}",
            if window.minimized { "最小化" } else { "正常  " },
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

/// 截屏并批量检测同一帧内的所有二维码
///
/// 发送端一屏可同时展示多张二维码（张数由前端按窗口尺寸自适应），
/// 这里必须把整帧内所有 grid 都解码出来，单张失败不影响同一帧的其它二维码。
fn detect_from_rgba(rgba: &[u8], width: usize, height: usize, verbose: bool) -> Result<Batch> {
    // 转换为灰度图像 (rqrr 需要灰度数据)
    let mut gray_data = vec![0u8; width * height];
    let (pixels, _) = rgba.as_chunks::<4>();
    for (gray, pixel) in gray_data.iter_mut().zip(pixels) {
        let r = pixel[0] as u32;
        let g = pixel[1] as u32;
        let b = pixel[2] as u32;
        // 灰度转换公式: 0.299*R + 0.587*G + 0.114*B
        *gray = ((299 * r + 587 * g + 114 * b) / 1000) as u8;
    }

    let gray_image = GrayImage::from_raw(width as u32, height as u32, gray_data)
        .ok_or_else(|| anyhow!("图像数据转换失败"))?;

    Ok(detect_multi(gray_image, verbose))
}

/// 整图检测 + 分块检测，多路并行，取并集
///
/// - 整图检测：对 1/2/4/6 宫格的大二维码最有效；
/// - 3×2 分块：小窗口下二维码排列紧凑时，rqrr 的定位图案分组会串味，
///   切块后每块最多一两张码，可以把漏掉的补回来；
/// - 2×2 分块：3×2 的瓦片边界正好切过某一列二维码时，这一列在两个瓦片里都残缺，
///   换一组更大的瓦片就能完整包住它（实测 4 宫格预览的左下角就是这样漏掉的）。
///
/// 都没拿全时再做一次整数倍放大重试（见 `detect_with_upscale`，大图会跳过）。
fn detect_multi(gray: GrayImage, verbose: bool) -> Batch {
    detect_multi_inner(gray, verbose, false)
}

/// `detect_multi` 的实现；`force_upscale` 供测试跳过限流直接验证放大重试
fn detect_multi_inner(gray: GrayImage, verbose: bool, force_upscale: bool) -> Batch {
    let full_gray = gray.clone();
    // 放大重试还要用原图，这里额外留一份
    let retry_gray = gray.clone();

    let (full, tiled, tiled2) = thread::scope(|scope| {
        let full_handle = scope.spawn(move || detect_from_gray(full_gray, verbose));
        let tile_handle = scope.spawn(|| detect_tiles(&gray, 3, 2, 0.25, verbose));
        // 3×2 的分块边界可能正好切过一列二维码（这一列在两个瓦片里都残缺），
        // 再叠一路 2×2 的大瓦片取并集：大瓦片能完整包住这一列，补回被切掉的码。
        let tile2_handle = scope.spawn(|| detect_tiles(&gray, 2, 2, 0.25, verbose));

        (
            full_handle.join().unwrap_or_else(|_| Batch::empty()),
            tile_handle.join().unwrap_or_else(|_| Batch::empty()),
            tile2_handle.join().unwrap_or_else(|_| Batch::empty()),
        )
    });

    let full_grids = full.full_grids;
    let full_decoded = full.full_decoded;
    let tile_grids = tiled.tile_grids + tiled2.tile_grids;
    let tile_decoded = tiled.tile_decoded + tiled2.tile_decoded;

    let mut payloads = full.payloads;
    let mut extra = 0usize;
    for payload in tiled.payloads.into_iter().chain(tiled2.payloads) {
        if !payloads.contains(&payload) {
            payloads.push(payload);
            extra += 1;
        }
    }

    // 一张都没拿到，或"检测到网格却解不出"时，多半是二维码在屏幕上渲染得太小、
    // 模块边界落在非整数像素上。整数倍最近邻放大能把模块重新对齐，实测能把整屏救回来。
    //
    // 但整帧放大很贵，而且 rqrr 在超大图上遇到假定位图案会长时间空转：2505x1329
    // 放大 2 倍是 1330 万像素，实测单帧 detect_grids 17~53 秒。超过像素上限就放弃
    // 整帧重试（--force_upscale 的测试路径不受限制），让分块检测与下一轮轮播补漏。
    let frame_pixels = retry_gray.width() as u64 * retry_gray.height() as u64;
    let upscale_too_big = frame_pixels * 4 > UPSCALE_MAX_PIXELS;
    if upscale_too_big && !UPSCALE_TOO_BIG_WARNED.swap(true, Ordering::Relaxed) {
        detail(
            "提示",
            format!(
                "整帧放大重试已跳过：{}x{} 放大 2 倍后约 {} 万像素，超过上限 {} 万；改由分块检测与下一轮轮播补漏",
                retry_gray.width(),
                retry_gray.height(),
                frame_pixels * 4 / 10_000,
                UPSCALE_MAX_PIXELS / 10_000
            ),
        );
    }
    let need_retry =
        (force_upscale || !upscale_too_big) && (payloads.is_empty() || full_decoded < full_grids);
    let retried = need_retry && (force_upscale || upscale_retry_allowed());
    let mut retry_grids = 0usize;
    let mut retry_decoded = 0usize;
    if retried {
        let retry = detect_with_upscale(&retry_gray, verbose);
        retry_grids = retry.retry_grids;
        retry_decoded = retry.retry_decoded;
        for payload in retry.payloads {
            if !payloads.contains(&payload) {
                payloads.push(payload);
                extra += 1;
            }
        }
    }

    Batch {
        payloads,
        full_grids,
        full_decoded,
        tile_grids,
        tile_decoded,
        retry_grids,
        retry_decoded,
        extra,
        retried,
    }
}

/// 放大重试的倍率：2 倍能救回绝大多数"太小/非整数像素"的帧，3 倍作为兜底
const UPSCALE_FACTORS: [u32; 2] = [2, 3];

/// 两次放大重试之间的最小间隔（毫秒）
///
/// 整帧放大重跑一次的成本和整帧检测本身同量级，而"屏幕上根本没有二维码"的帧很常见，
/// 不设闸门会白白浪费 CPU。
const UPSCALE_RETRY_GAP_MS: u64 = 500;

/// 整帧放大重试的像素上限（放大 2 倍后的像素数）
///
/// rqrr 0.7 在整帧 2 倍放大后遇到假定位图案时，可能进入近乎无界的对齐图案搜索：
/// 实测 2505x1329（放大后 1330 万像素）单帧 detect_grids 17~53 秒，期间完全不截屏，
/// 1 秒一页的轮播会白翻十几页。大屏原生尺寸布局直接放弃整帧重试，
/// 由分块检测和下一轮轮播兜底；1080p 及以下仍保留原来的放大兜底。
const UPSCALE_MAX_PIXELS: u64 = 10_000_000;

/// 只提示一次"因像素过大跳过放大重试"
static UPSCALE_TOO_BIG_WARNED: AtomicBool = AtomicBool::new(false);

/// 上一次放大重试的时刻（相对进程启动的毫秒数）
static LAST_UPSCALE_RETRY_MS: AtomicU64 = AtomicU64::new(0);
static PROCESS_EPOCH: OnceLock<Instant> = OnceLock::new();

/// 判断本次是否允许做放大重试（限流）
fn upscale_retry_allowed() -> bool {
    let epoch = PROCESS_EPOCH.get_or_init(Instant::now);
    let now = epoch.elapsed().as_millis() as u64;
    let last = LAST_UPSCALE_RETRY_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < UPSCALE_RETRY_GAP_MS {
        return false;
    }
    LAST_UPSCALE_RETRY_MS.store(now, Ordering::Relaxed);
    true
}

/// 把整帧按整数倍最近邻放大后重新识别
///
/// 为什么是最近邻：浏览器渲染 SVG 用的是 crispEdges（无抗锯齿），模块边界本来就
/// 吸附在像素上；最近邻放大只会把每个模块复制成 n×n 的方块，不会引入新的灰阶。
/// 实测同一帧：原图只能解出 1/6，2 倍最近邻后 6/6；用 Triangle/CatmullRom
/// 插值反而只剩 3/6 甚至 0/6。
fn detect_with_upscale(gray: &GrayImage, verbose: bool) -> Batch {
    use image::imageops;

    let mut merged = Batch::empty();

    for factor in UPSCALE_FACTORS {
        let upscaled = imageops::resize(
            gray,
            gray.width() * factor,
            gray.height() * factor,
            imageops::FilterType::Nearest,
        );
        let batch = detect_from_gray(upscaled, verbose);
        merged.retried = true;
        merged.retry_grids = merged.retry_grids.max(batch.full_grids);
        merged.retry_decoded = merged.retry_decoded.max(batch.full_decoded);

        for payload in batch.payloads {
            if !merged.payloads.contains(&payload) {
                merged.payloads.push(payload);
            }
        }

        if !merged.payloads.is_empty() {
            detail(
                "调试",
                format!(
                    "放大 {} 倍后识别出 {} 张二维码",
                    factor,
                    merged.payloads.len()
                ),
            );
            break;
        }
    }

    merged
}

/// 从灰度图中批量识别所有二维码
///
/// rqrr 在二维码过小/过密时会 assert panic（timing_scan 的 scan >= 1），
/// 这里做整段隔离：本帧该区域直接放弃，绝不让接收进程崩掉。
fn detect_from_gray(gray_image: GrayImage, verbose: bool) -> Batch {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        detect_from_gray_inner(gray_image, verbose)
    })) {
        Ok(batch) => batch,
        Err(_) => {
            warn_panic_once("");
            Batch::empty()
        }
    }
}

/// 从灰度图中批量识别所有二维码（内部实现）
///
/// 单张码解码（version 40 约 20ms）按网格分片并行，6 张码可压到 1 张的时间。
fn detect_from_gray_inner(gray_image: GrayImage, _verbose: bool) -> Batch {
    // 准备图像用于检测
    let mut prepared = rqrr::PreparedImage::prepare(gray_image);

    // 搜索二维码（一屏可能有多张）
    let grids = prepared.detect_grids();
    let grid_count = grids.len();

    let threads = worker_threads().min(grid_count).max(1);
    let per = grid_count.div_ceil(threads).max(1);

    let parts: Vec<Vec<String>> = thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut start = 0usize;

        while start < grid_count {
            let end = (start + per).min(grid_count);
            let slice = &grids[start..end];

            handles.push(scope.spawn(move || {
                let mut payloads = Vec::new();
                for grid in slice {
                    // 单张码解码失败（含 rqrr 内部断言 panic）只跳过，不影响同屏其它码
                    if let Ok(Ok((_metadata, payload))) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| grid.decode()))
                    {
                        payloads.push(payload);
                    }
                }
                payloads
            }));

            start = end;
        }

        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });

    let mut payloads = Vec::with_capacity(grid_count);
    for part in parts {
        payloads.extend(part);
    }

    let decoded = payloads.len();
    Batch {
        payloads,
        full_grids: grid_count,
        full_decoded: decoded,
        ..Batch::empty()
    }
}

/// 把灰度图切成 `cols × rows` 个带重叠的瓦片，逐块并行检测后合并结果
///
/// rqrr 的整图检测在"一屏多码"时会把相邻二维码的定位图案错误分组，
/// 实测一屏 6 张只能识别出 3 张；分块后每块最多一两张码，分组不再串味。
fn detect_tiles(gray: &GrayImage, cols: u32, rows: u32, overlap: f32, verbose: bool) -> Batch {
    use image::imageops;

    let width = gray.width();
    let height = gray.height();
    if cols == 0 || rows == 0 || width == 0 || height == 0 {
        return Batch::empty();
    }

    let tile_w = (((width as f32) / cols as f32) * (1.0 + overlap)).ceil() as u32;
    let tile_h = (((height as f32) / rows as f32) * (1.0 + overlap)).ceil() as u32;
    let step_x = width / cols;
    let step_y = height / rows;

    // 先算出所有瓦片区域，再并行检测
    let mut regions: Vec<(u32, u32, u32, u32)> = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let x = (col * step_x).min(width.saturating_sub(1));
            let y = (row * step_y).min(height.saturating_sub(1));
            let w = tile_w.min(width - x);
            let h = tile_h.min(height - y);
            if w > 0 && h > 0 {
                regions.push((x, y, w, h));
            }
        }
    }

    let parts: Vec<Batch> = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(regions.len());

        for (x, y, w, h) in regions {
            handles.push(scope.spawn(move || {
                let tile = imageops::crop_imm(gray, x, y, w, h).to_image();
                detect_from_gray(tile, verbose)
            }));
        }

        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });

    let mut payloads: Vec<String> = Vec::new();
    let mut tile_grids = 0usize;
    let mut tile_decoded = 0usize;

    for part in parts {
        tile_grids += part.full_grids;
        tile_decoded += part.full_decoded;
        for payload in part.payloads {
            if !payloads.contains(&payload) {
                payloads.push(payload);
            }
        }
    }

    Batch {
        payloads,
        tile_grids,
        tile_decoded,
        ..Batch::empty()
    }
}

/// 批量解析载荷并入库，返回本次新增的片段
///
/// 同一帧内的重复片段只入库一次；总数以首个片段为准，后续不一致只计数告警。
fn process_payloads(
    payloads: &[String],
    received_chunks: &mut HashMap<usize, Chunk>,
    expected_total: &mut Option<usize>,
) -> BatchOutcome {
    let mut new_seqs = Vec::new();
    let mut conflicts = 0usize;
    let mut errors = Vec::new();

    for raw in payloads {
        match parse_chunk_data(raw) {
            Ok(chunk) => {
                let seq = chunk.seq;
                let total = chunk.total;

                match *expected_total {
                    None => *expected_total = Some(total),
                    Some(expected) if expected != total => conflicts += 1,
                    _ => {}
                }

                if let std::collections::hash_map::Entry::Vacant(entry) =
                    received_chunks.entry(seq)
                {
                    entry.insert(chunk);
                    new_seqs.push(seq);
                }
            }
            Err(e) => {
                // 按字符截断，避免多字节字符边界 panic
                let preview: String = raw.chars().take(30).collect();
                errors.push(format!(
                    "解析片段数据失败: {} (原始数据: {}...)",
                    e, preview
                ));
            }
        }
    }

    BatchOutcome {
        new_seqs,
        conflicts,
        errors,
    }
}

/// 解析片段数据: "序号/总数|数据"
fn parse_chunk_data(data: &str) -> Result<Chunk> {
    let parts: Vec<&str> = data.splitn(2, '|').collect();
    if parts.len() != 2 {
        return Err(anyhow!("无效的数据格式，缺少 '|' 分隔符"));
    }

    let header = parts[0];
    let chunk_data = parts[1].to_string();

    let seq_total: Vec<&str> = header.split('/').collect();
    if seq_total.len() != 2 {
        return Err(anyhow!("无效的头部格式，应为 '序号/总数'"));
    }

    let seq: usize = seq_total[0]
        .parse()
        .map_err(|e| anyhow!("序号解析失败: {}", e))?;
    let total: usize = seq_total[1]
        .parse()
        .map_err(|e| anyhow!("总数解析失败: {}", e))?;

    if seq == 0 || seq > total {
        return Err(anyhow!("无效的序号: {}/{}", seq, total));
    }

    Ok(Chunk {
        seq,
        total,
        data: chunk_data,
    })
}

/// 重组并还原文件
fn restore_file(chunks: &HashMap<usize, Chunk>, expected_total: Option<usize>) -> Result<()> {
    let received_count = chunks.len();

    // 检查是否有缺失的片段
    if let Some(total) = expected_total {
        if received_count < total {
            say("");
            say("╔════════════════════════════════════════╗");
            say("║           警告：片段缺失!             ║");
            say("╠════════════════════════════════════════╣");
            say(format!("║  预期片段数: {:26} ║", total));
            say(format!("║  实际接收数: {:26} ║", received_count));
            say("╚════════════════════════════════════════╝");
            say("");

            // 列出缺失的片段
            let mut missing: Vec<usize> = Vec::new();
            for i in 1..=total {
                if !chunks.contains_key(&i) {
                    missing.push(i);
                }
            }

            if missing.len() <= 20 {
                say(format!("缺失的片段: {:?}", missing));
            } else {
                say(format!("缺失的片段: {} 个 (太多无法列出)", missing.len()));
            }
            say("");

            return Err(anyhow!(
                "接收不完整: 缺失 {}/{} 片段，请重新扫描",
                missing.len(),
                total
            ));
        }

        // 检查是否有额外或不连续的片段
        for i in 1..=total {
            if !chunks.contains_key(&i) {
                return Err(anyhow!("数据不完整: 缺少片段 #{}", i));
            }
        }
    }

    say("");
    log(
        "重组",
        format!("按序号排序并拼接 {} 个数据片段", received_count),
    );

    // 按序号排序并拼接数据
    let mut sorted_chunks: Vec<&Chunk> = chunks.values().collect();
    sorted_chunks.sort_by_key(|c| c.seq);

    let combined: String = sorted_chunks
        .iter()
        .map(|c| c.data.as_str())
        .collect::<Vec<&str>>()
        .join("");

    detail("重组", format!("Base64 数据总长度: {} bytes", combined.len()));

    // Base64 解码
    log("重组", "解码 Base64...");
    let file_data = base64::engine::general_purpose::STANDARD
        .decode(combined.trim())
        .map_err(|e| anyhow!("Base64 解码失败: {}", e))?;

    detail("重组", format!("解码后文件大小: {} bytes", file_data.len()));

    // 生成输出文件名
    let output_filename = generate_output_filename();

    // 保存文件
    let mut file = File::create(&output_filename).map_err(|e| anyhow!("创建文件失败: {}", e))?;
    file.write_all(&file_data)
        .map_err(|e| anyhow!("写入文件失败: {}", e))?;

    say("");
    log("完成", "文件已重组成功");
    say("╔════════════════════════════════════════╗");
    say("║           文件接收成功!                ║");
    say("╠════════════════════════════════════════╣");
    say(format!("║  文件名: {:30} ║", output_filename));
    say(format!(
        "║  大小:   {:30} ║",
        format_file_size(file_data.len())
    ));
    say(format!("║  片段数: {:30} ║", received_count));
    say("╚════════════════════════════════════════╝");

    Ok(())
}

/// 生成输出文件名
fn generate_output_filename() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    format!("received_file_{}", now)
}

/// 格式化文件大小
fn format_file_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;

    if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

/// 保存会话到默认文件
fn save_session(chunks: &HashMap<usize, Chunk>, expected_total: Option<usize>) -> Result<()> {
    save_session_to(Path::new(SAVE_FILE), chunks, expected_total)
}

/// 原子地保存会话到指定文件
fn save_session_to(
    path: &Path,
    chunks: &HashMap<usize, Chunk>,
    expected_total: Option<usize>,
) -> Result<()> {
    let session = Session {
        chunks: chunks.clone(),
        expected_total,
    };

    let json = serde_json::to_string_pretty(&session)?;

    // 先写同目录临时文件，再 rename，避免进程在写入中途被杀导致会话损坏
    let tmp_path = {
        let mut os = path.as_os_str().to_owned();
        os.push(".tmp");
        PathBuf::from(os)
    };
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.flush()?;
    }
    fs::rename(&tmp_path, path)?;

    Ok(())
}

/// 从默认文件加载会话
fn load_session() -> Result<Session> {
    load_session_from(Path::new(SAVE_FILE))
}

/// 从指定文件加载会话
fn load_session_from(path: &Path) -> Result<Session> {
    let mut file = File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let session: Session = serde_json::from_str(&contents)?;
    Ok(session)
}

/// 找出缺失的片段
fn find_missing_chunks(chunks: &HashMap<usize, Chunk>, total: usize) -> Vec<usize> {
    let mut missing = Vec::new();
    for i in 1..=total {
        if !chunks.contains_key(&i) {
            missing.push(i);
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_session_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qr_session_test_{}_{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir.join("session.json")
    }

    #[test]
    fn session_round_trip_is_atomic() {
        let path = temp_session_path();

        let mut chunks = HashMap::new();
        chunks.insert(
            1,
            Chunk {
                seq: 1,
                total: 2,
                data: "abc".to_string(),
            },
        );

        save_session_to(&path, &chunks, Some(2)).expect("保存会话失败");

        // 再保存一次，覆盖已存在的会话文件，验证 rename 的替换行为
        let mut updated_chunks = chunks.clone();
        updated_chunks.insert(
            2,
            Chunk {
                seq: 2,
                total: 2,
                data: "def".to_string(),
            },
        );
        save_session_to(&path, &updated_chunks, Some(2)).expect("覆盖保存会话失败");

        let tmp_path = {
            let mut os = path.as_os_str().to_owned();
            os.push(".tmp");
            PathBuf::from(os)
        };
        assert!(!tmp_path.exists(), "临时文件应在 rename 后消失");

        let loaded = load_session_from(&path).expect("加载会话失败");
        assert_eq!(loaded.chunks.len(), 2);
        assert_eq!(loaded.chunks.get(&1).map(|c| c.data.as_str()), Some("abc"));
        assert_eq!(loaded.chunks.get(&2).map(|c| c.data.as_str()), Some("def"));
        assert_eq!(loaded.expected_total, Some(2));

        let _ = fs::remove_dir_all(path.parent().expect("临时目录不存在"));
    }

    #[test]
    fn parse_chunk_data_accepts_valid_payload() {
        let chunk = parse_chunk_data("3/10|QUJDRA==").expect("解析失败");
        assert_eq!(chunk.seq, 3);
        assert_eq!(chunk.total, 10);
        assert_eq!(chunk.data, "QUJDRA==");
    }

    #[test]
    fn parse_chunk_data_rejects_invalid_payloads() {
        // 缺少 '|'
        assert!(parse_chunk_data("3/10").is_err());
        // 缺少 '/'
        assert!(parse_chunk_data("3|data").is_err());
        // 序号为 0
        assert!(parse_chunk_data("0/10|data").is_err());
        // 序号大于总数
        assert!(parse_chunk_data("11/10|data").is_err());
        // 非数字
        assert!(parse_chunk_data("a/10|data").is_err());
    }

    #[test]
    fn process_payloads_batches_and_dedupes() {
        let mut chunks: HashMap<usize, Chunk> = HashMap::new();
        let mut expected_total: Option<usize> = None;

        // 同一帧 6 张码，其中 #2 重复出现一次
        let payloads: Vec<String> = vec![
            "1/6|AAAA".to_string(),
            "2/6|BBBB".to_string(),
            "2/6|BBBB".to_string(),
            "3/6|CCCC".to_string(),
            "4/6|DDDD".to_string(),
            "5/6|EEEE".to_string(),
            "6/6|FFFF".to_string(),
        ];

        let outcome = process_payloads(&payloads, &mut chunks, &mut expected_total);

        assert_eq!(chunks.len(), 6, "重复片段只能入库一次");
        assert_eq!(outcome.new_seqs, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(outcome.conflicts, 0);
        assert!(outcome.errors.is_empty());
        assert_eq!(expected_total, Some(6));

        // 第二帧全部是已收集片段：没有新增，也没有冲突
        let outcome = process_payloads(&payloads, &mut chunks, &mut expected_total);
        assert!(outcome.new_seqs.is_empty());
        assert_eq!(outcome.conflicts, 0);
        assert_eq!(chunks.len(), 6);
    }

    #[test]
    fn process_payloads_reports_conflict_and_parse_error() {
        let mut chunks: HashMap<usize, Chunk> = HashMap::new();
        let mut expected_total: Option<usize> = None;

        let payloads: Vec<String> = vec![
            "1/6|AAAA".to_string(),
            // 总数与预期不一致
            "2/7|BBBB".to_string(),
            // 无法解析
            "坏数据".to_string(),
        ];

        let outcome = process_payloads(&payloads, &mut chunks, &mut expected_total);

        assert_eq!(outcome.conflicts, 1);
        assert_eq!(outcome.errors.len(), 1);
        // 冲突片段仍然入库（按首个片段的总数为准）
        assert_eq!(outcome.new_seqs, vec![1, 2]);
        assert_eq!(expected_total, Some(6));
    }

    #[test]
    fn summarize_seqs_truncates_long_lists() {
        assert_eq!(summarize_seqs(&[1, 2, 3]), "[#1,#2,#3]");
        let long: Vec<usize> = (1..=20).collect();
        let summary = summarize_seqs(&long);
        assert!(summary.contains("共 20 个"));
    }

    #[test]
    fn missing_list_preview_truncates() {
        assert_eq!(summarize_missing(&[]), "[]");
        assert_eq!(summarize_missing(&[2, 4, 6]), "[2,4,6]");

        // 正好 20 个：完整列出，不带省略号
        let exact: Vec<usize> = (1..=20).collect();
        let summary = summarize_missing(&exact);
        assert!(summary.ends_with("20]"), "20 个应完整列出: {}", summary);

        // 超过 20 个：只列前 20，并以省略号结尾（总数由调用方打印）
        let long: Vec<usize> = (1..=25).collect();
        let summary = summarize_missing(&long);
        assert!(summary.contains("20,"), "应包含第 20 个: {}", summary);
        assert!(!summary.contains("21,"), "不应列出第 21 个: {}", summary);
        assert!(summary.ends_with("…]"), "超过 20 个应有省略号: {}", summary);
    }

    #[test]
    fn find_missing_chunks_reports_gaps() {
        let mut chunks: HashMap<usize, Chunk> = HashMap::new();
        for seq in [1usize, 3, 5] {
            chunks.insert(
                seq,
                Chunk {
                    seq,
                    total: 5,
                    data: "x".to_string(),
                },
            );
        }
        assert_eq!(find_missing_chunks(&chunks, 5), vec![2, 4]);

        // 一个片段都没有时，缺失的就是整个 1..=total
        let empty: HashMap<usize, Chunk> = HashMap::new();
        assert_eq!(find_missing_chunks(&empty, 3), vec![1, 2, 3]);
    }

    #[test]
    fn parse_options_reads_flags() {
        let args: Vec<String> = ["server", "--resume", "--verbose", "--interval-ms", "20"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_options(&args).expect("解析失败");
        assert!(opts.resume);
        assert!(opts.verbose);
        assert!(!opts.clean);
        assert_eq!(opts.interval_ms, 20);

        // 默认值
        let args: Vec<String> = vec!["server".to_string()];
        let opts = parse_options(&args).expect("解析失败");
        assert_eq!(opts.interval_ms, CAPTURE_INTERVAL_MS);
        assert!(!opts.resume);

        // 非法间隔
        let args: Vec<String> = ["server", "--interval-ms", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_options(&args).is_err());
    }

    #[test]
    fn parse_options_reads_window_and_timeout() {
        let args: Vec<String> = [
            "server",
            "--process",
            "HSR",
            "--window=liushikai",
            "--idle-timeout",
            "0",
            "--fullscreen",
            "--process=chrome",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // --fullscreen 与 --process 同时给 = 参数冲突
        assert!(parse_options(&args).is_err());

        let args: Vec<String> = ["server", "--process", "HSR", "--window=liushikai"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_options(&args).expect("解析失败");
        assert_eq!(opts.process.as_deref(), Some("HSR"));
        assert_eq!(opts.window.as_deref(), Some("liushikai"));

        // 默认按进程 HSR 抓窗口
        let args: Vec<String> = vec!["server".to_string()];
        let opts = parse_options(&args).expect("解析失败");
        assert_eq!(opts.process.as_deref(), Some(DEFAULT_PROCESS));
        assert!(!opts.fullscreen);

        // --fullscreen 退回整屏
        let args: Vec<String> = ["server", "--fullscreen"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_options(&args).expect("解析失败");
        assert!(opts.fullscreen);
        assert!(opts.process.is_none());
        assert!(opts.window.is_none());

        let args: Vec<String> = ["server", "--idle-timeout", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_options(&args).expect("解析失败");
        // 0 表示永不超时
        assert!(opts.idle_timeout.is_none());

        let args: Vec<String> = ["server", "--idle-timeout=30"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_options(&args).expect("解析失败");
        assert_eq!(opts.idle_timeout, Some(Duration::from_secs(30)));

        // 非数字的时长必须报错，而不是被静默忽略
        let args: Vec<String> = ["server", "--idle-timeout", "abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_options(&args).is_err());

        // 空的进程名/标题没有意义
        let args: Vec<String> = ["server", "--process="]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_options(&args).is_err());
    }

    /// 把若干二维码按 3×2 宫格合成一张灰度图，模拟发送端的一屏多码
    fn compose_qr_grid(payloads: &[String], cell: u32) -> GrayImage {
        use image::imageops;
        use qrcode::QrCode;

        const COLS: u32 = 3;
        let rows = (payloads.len() as u32).div_ceil(COLS).max(1);
        let mut canvas = GrayImage::from_pixel(COLS * cell, rows * cell, image::Luma([255u8]));

        let inner = cell - 40; // 留白，模拟宫格间隙
        for (i, payload) in payloads.iter().enumerate() {
            let qr = QrCode::new(payload.as_bytes()).expect("生成二维码失败");
            let rendered = qr
                .render::<image::Luma<u8>>()
                .quiet_zone(true)
                .min_dimensions(inner, inner)
                .max_dimensions(inner, inner)
                .build();

            let x = (i as u32 % COLS) * cell + 20;
            let y = (i as u32 / COLS) * cell + 20;
            imageops::replace(&mut canvas, &rendered, x as i64, y as i64);
        }

        canvas
    }

    /// 合成一张 1920×1080 的模拟屏幕：`cols × rows` 宫格，每格一张二维码
    fn compose_screen(cols: u32, rows: u32) -> (GrayImage, Vec<String>) {
        use image::imageops;
        use qrcode::QrCode;

        let total = (cols * rows) as usize;
        let payloads: Vec<String> = (1..=total)
            .map(|i| format!("{}/{}|PAYLOAD-{}", i, total, i))
            .collect();

        let (sw, sh) = (1920u32, 1080u32);
        let mut canvas = GrayImage::from_pixel(sw, sh, image::Luma([250u8]));

        let cw = sw / cols;
        let ch = sh / rows;
        let side = cw.min(ch).saturating_sub(40);

        for (i, payload) in payloads.iter().enumerate() {
            let qr = QrCode::new(payload.as_bytes()).expect("生成二维码失败");
            let rendered = qr
                .render::<image::Luma<u8>>()
                .quiet_zone(true)
                .min_dimensions(side, side)
                .max_dimensions(side, side)
                .build();

            let x = (i as u32 % cols) * cw + (cw - side) / 2;
            let y = (i as u32 / cols) * ch + (ch - side) / 2;
            imageops::replace(&mut canvas, &rendered, x as i64, y as i64);
        }

        (canvas, payloads)
    }

    /// 四种宫格布局都必须能一次性识别出全部二维码
    #[test]
    fn detects_all_grid_layouts() {
        for (name, cols, rows) in [
            ("1-up", 1u32, 1u32),
            ("2-up", 2, 1),
            ("4-up", 2, 2),
            ("6-up", 3, 2),
        ] {
            let (canvas, expected) = compose_screen(cols, rows);
            let batch = detect_multi(canvas, false);

            let mut got = batch.payloads.clone();
            got.sort();
            let mut want = expected.clone();
            want.sort();

            assert_eq!(got, want, "{} 布局未能识别出全部二维码", name);
        }
    }

    #[test]
    fn detects_multiple_qr_codes_in_one_image() {
        // 小窗口 + 紧凑排列的极端场景（3×2 宫格，二维码之间只有 20px 间隙）
        let payloads: Vec<String> = (1..=6).map(|i| format!("{}/6|PAYLOAD-{}", i, i)).collect();
        let canvas = compose_qr_grid(&payloads, 320);

        let batch = detect_multi(canvas, false);

        let mut got = batch.payloads.clone();
        got.sort();
        let mut want = payloads.clone();
        want.sort();
        assert_eq!(got, want, "紧凑排列下 6 张码应全部解码成功");

        // 批量入库：一次拿满 6 个片段
        let mut chunks: HashMap<usize, Chunk> = HashMap::new();
        let mut expected_total: Option<usize> = None;
        let outcome = process_payloads(&batch.payloads, &mut chunks, &mut expected_total);

        assert_eq!(outcome.new_seqs.len(), 6);
        let mut seqs = outcome.new_seqs.clone();
        seqs.sort_unstable();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(expected_total, Some(6));
    }

    /// 真实切片密度下的 6 宫格识别
    ///
    /// client 的切片是 2000 个 base64 字符，二维码接近 version 39-40（177 模块），
    /// 在 1920×1080 上每格约 500px，只有 2~3 像素/模块——这是最难的场景。
    #[test]
    fn detects_dense_six_up_screen() {
        use image::imageops;
        use qrcode::QrCode;

        const CHUNK: usize = 2000;
        let alphabet: Vec<u8> =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".to_vec();
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next_char = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            alphabet[(seed >> 33) as usize % alphabet.len()] as char
        };

        let payloads: Vec<String> = (1..=6)
            .map(|i| {
                let body: String = (0..CHUNK).map(|_| next_char()).collect();
                format!("{}/6|{}", i, body)
            })
            .collect();

        // 1920×1080 屏幕上按 3×2 铺开，每张码约 500px
        let (sw, sh) = (1920u32, 1080u32);
        let (cols, rows) = (3u32, 2u32);
        let mut canvas = GrayImage::from_pixel(sw, sh, image::Luma([255u8]));
        let (cw, ch) = (sw / cols, sh / rows);
        let side = cw.min(ch).saturating_sub(40);

        for (i, payload) in payloads.iter().enumerate() {
            let qr = QrCode::new(payload.as_bytes()).expect("生成二维码失败");
            let rendered = qr
                .render::<image::Luma<u8>>()
                .quiet_zone(true)
                .min_dimensions(side, side)
                .max_dimensions(side, side)
                .build();

            let x = (i as u32 % cols) * cw + (cw - side) / 2;
            let y = (i as u32 / cols) * ch + (ch - side) / 2;
            imageops::replace(&mut canvas, &rendered, x as i64, y as i64);
        }

        let batch = detect_multi(canvas, false);

        let mut got = batch.payloads.clone();
        got.sort();
        let mut want = payloads.clone();
        want.sort();

        assert_eq!(
            got.len(),
            6,
            "高密度 6 宫格只识别出 {} 张（每格 {}px）",
            got.len(),
            side
        );
        assert_eq!(got, want, "识别出的载荷与原始载荷不一致");
    }

    /// 二维码太小导致整屏一张都解不出时，放大重试必须把整屏救回来
    ///
    /// 复现的是"小窗口 + 每屏 6 张"：每张只有 1 像素/模块，rqrr 连定位图案都扫不出来
    /// （会断言 panic，已被隔离）。最近邻放大 2 倍后每模块变成 2×2 像素，应当全中。
    #[test]
    fn upscale_retry_recovers_too_small_screen() {
        use image::imageops;
        use qrcode::QrCode;

        const CHUNK: usize = 2000;
        let alphabet: Vec<u8> =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".to_vec();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next_char = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            alphabet[(seed >> 33) as usize % alphabet.len()] as char
        };

        let payloads: Vec<String> = (1..=6)
            .map(|i| {
                let body: String = (0..CHUNK).map(|_| next_char()).collect();
                format!("{}/6|{}", i, body)
            })
            .collect();

        // 177 模块 -> 每张 177px，即 1 像素/模块
        let (sw, sh, cols, rows) = (1363u32, 964u32, 3u32, 2u32);
        let side = 177u32;
        let (cw, ch) = (sw / cols, sh / rows);
        let mut canvas = GrayImage::from_pixel(sw, sh, image::Luma([255u8]));

        for (i, payload) in payloads.iter().enumerate() {
            let qr = QrCode::new(payload.as_bytes()).expect("生成二维码失败");
            let rendered = qr
                .render::<image::Luma<u8>>()
                .quiet_zone(true)
                .min_dimensions(side, side)
                .max_dimensions(side, side)
                .build();
            let x = (i as u32 % cols) * cw + (cw - side) / 2;
            let y = (i as u32 / cols) * ch + (ch - side) / 2;
            imageops::replace(&mut canvas, &rendered, x as i64, y as i64);
        }

        // 原尺寸：一张都拿不到
        let plain = detect_from_gray(canvas.clone(), false);
        assert!(
            plain.payloads.is_empty(),
            "1 像素/模块的帧不应能直接解出，实际解出 {} 张",
            plain.payloads.len()
        );

        // 放大重试：6 张全部回来
        let batch = detect_multi_inner(canvas, false, true);
        let mut got = batch.payloads.clone();
        got.sort();
        let mut want = payloads.clone();
        want.sort();
        assert_eq!(got, want, "放大重试应把 6 张全部救回来");
    }

    /// 二维码过小/过密时 rqrr 会 assert panic，必须被隔离而不是让进程崩掉
    #[test]
    fn too_dense_screen_does_not_panic() {
        use image::imageops;
        use qrcode::QrCode;

        // 1366×768 上强行塞 6 张 version 40 的码：每张只有 ~1.8 像素/模块
        let (sw, sh, cols, rows) = (1366u32, 768u32, 3u32, 2u32);
        let body: String = "QUJD".repeat(500); // 2000 字符
        let payloads: Vec<String> = (1..=6).map(|i| format!("{}/6|{}", i, body)).collect();

        let mut canvas = GrayImage::from_pixel(sw, sh, image::Luma([255u8]));
        let (cw, ch) = (sw / cols, sh / rows);
        let side = cw.min(ch).saturating_sub(40);

        for (i, payload) in payloads.iter().enumerate() {
            let qr = QrCode::new(payload.as_bytes()).expect("生成二维码失败");
            let rendered = qr
                .render::<image::Luma<u8>>()
                .quiet_zone(true)
                .min_dimensions(side, side)
                .max_dimensions(side, side)
                .build();
            let x = (i as u32 % cols) * cw + (cw - side) / 2;
            let y = (i as u32 / cols) * ch + (ch - side) / 2;
            imageops::replace(&mut canvas, &rendered, x as i64, y as i64);
        }

        // 关键断言：不 panic（识别出几张都可以，识别不出来会走超时重试）
        let batch = detect_multi(canvas, false);
        assert!(batch.payloads.len() <= 6);
    }

    /// 临时探针：模拟"鼠标光标压在某张码上"，看是不是只有那一张解不出来
    #[test]
    #[ignore]
    fn probe_cursor_overlay() {
        use image::imageops;
        use qrcode::QrCode;

        // 1920×1080 的 6 宫格，每格 640×540，码 500px 居中（贴近真实页面）
        let (sw, sh, cols, rows, side) = (1920u32, 1080u32, 3u32, 2u32, 500u32);
        let total = (cols * rows) as usize;
        let body: String = "QUJD".repeat(500);
        let payloads: Vec<String> = (1..=total)
            .map(|i| format!("{}/{}|{}", i, total, body))
            .collect();

        let (cw, ch) = (sw / cols, sh / rows);
        let mut canvas = GrayImage::from_pixel(sw, sh, image::Luma([255u8]));

        for (i, payload) in payloads.iter().enumerate() {
            let qr = QrCode::new(payload.as_bytes()).expect("生成二维码失败");
            let rendered = qr
                .render::<image::Luma<u8>>()
                .quiet_zone(true)
                .min_dimensions(side, side)
                .max_dimensions(side, side)
                .build();
            let x = (i as u32 % cols) * cw + (cw - side) / 2;
            let y = (i as u32 / cols) * ch + (ch - side) / 2;
            imageops::replace(&mut canvas, &rendered, x as i64, y as i64);
        }

        let clean = detect_multi(canvas.clone(), false);
        let mut clean_seqs: Vec<usize> = clean
            .payloads
            .iter()
            .filter_map(|p| parse_chunk_data(p).ok().map(|c| c.seq))
            .collect();
        clean_seqs.sort_unstable();
        println!("无光标: {:?}", clean_seqs);

        // 在"下中"那张码的左上角画一个 28×36 的光标块（模拟鼠标指针压在定位图案上）
        let cursor_x = cw + (cw - side) / 2 + 6;
        let cursor_y = ch + (ch - side) / 2 + 6;
        for dy in 0..36u32 {
            for dx in 0..28u32 {
                let px = cursor_x + dx;
                let py = cursor_y + dy;
                if px < sw && py < sh {
                    // 箭头形状：左侧实心、右侧透明
                    let inside = dx < 12 || (dx < 24 && dy > dx);
                    if inside {
                        canvas.put_pixel(px, py, image::Luma([0u8]));
                    }
                }
            }
        }

        let dirty = detect_multi(canvas, false);
        let mut dirty_seqs: Vec<usize> = dirty
            .payloads
            .iter()
            .filter_map(|p| parse_chunk_data(p).ok().map(|c| c.seq))
            .collect();
        dirty_seqs.sort_unstable();
        println!("有光标: {:?}", dirty_seqs);
    }

    #[test]
    fn single_qr_image_still_works() {
        let payloads = vec!["1/1|ONLY".to_string()];
        let canvas = compose_qr_grid(&payloads, 320);

        let batch = detect_from_gray(canvas, false);

        assert_eq!(batch.full_grids, 1);
        assert_eq!(batch.payloads, vec!["1/1|ONLY".to_string()]);
    }
}
