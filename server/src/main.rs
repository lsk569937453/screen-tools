//! 接收端：从目标窗口的后台截图里识别单张高密度码，收集分片、纠删恢复、还原文件。
//!
//! 与旧版最大的区别：不再找二维码，而是按固定网格直接采样；丢的分片由文件级纠删码补回。

use anyhow::{anyhow, Result};
use clap::Parser;
use common::{clock, digest, format, logging};
use screencode::detect::{decode_frame, to_gray, DecodeOptions, DecodedFrame};
use screencode::header::Header;
use screencode::PAD_SLOT;
use screenshots::Screen;
use std::env;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

mod cli;
mod store;

use cli::{Cli, Options};
use store::ShardStore;

const SAVE_INTERVAL_SECONDS: u64 = 1;
const LOG_DIR: &str = "logs";

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
        uptime: Duration,
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
            uptime.as_secs_f64(),
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
    let args = cli::normalize_help_alias(env::args().collect());
    let options = Options::from(Cli::parse_from(args));

    let log = match logging::init("server", options.verbose, Some(Path::new(LOG_DIR))) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("初始化日志失败: {error}");
            std::process::exit(1);
        }
    };
    if let Some(error) = &log.file_error {
        warn!("日志文件不可用，仅输出到控制台: {error}");
    }
    if let Some(path) = &log.file {
        info!("日志文件: {}", path.display());
    }

    if let Err(error) = run(options) {
        error!("{error}");
        eprintln!("\n接收失败，详见上方日志与 logs/ 目录");
        std::process::exit(1);
    }
}

fn run(options: Options) -> Result<()> {
    if options.clean {
        let removed = store::clean(Path::new("."))?;
        if removed.is_empty() {
            info!("没有可清除的分片库");
        } else {
            for path in removed {
                info!("已删除 {}", path.display());
            }
        }
        return Ok(());
    }

    if options.list_windows {
        return list_windows_report();
    }

    println!("╔════════════════════════════════════════╗");
    println!("║       ScreenCode 接收器                ║");
    println!("╚════════════════════════════════════════╝");
    if let Ok(dir) = env::current_dir() {
        info!(
            "分片库: {}（{}）",
            dir.join(store::BLOCKS_FILE).display(),
            store::describe_store(&dir)
        );
    }

    let shard_store = open_store(&options)?;

    info!(
        "配置: 截屏间隔 {} ms | 空闲超时 {} | 重解间隔 {} ms | 纠正冗余 {}",
        options.interval_ms,
        match options.idle_timeout {
            Some(limit) => format!("{} s", limit.as_secs()),
            None => "关闭（--idle-timeout 0）".to_string(),
        },
        options.recheck_ms,
        shard_store
            .header
            .as_ref()
            .map(|header| format!("k={} m={}", header.k, header.m))
            .unwrap_or_else(|| "待定（读帧头后确定）".to_string())
    );

    let screens = Screen::all().map_err(|error| anyhow!("获取屏幕失败: {}", error))?;
    debug!("检测到 {} 块屏幕", screens.len());
    for (index, item) in screens.iter().enumerate() {
        let info = &item.display_info;
        debug!(
            "屏幕 #{} id={} 位置=({},{}) 尺寸={}x{} 缩放={:.2}",
            index + 1,
            info.id,
            info.x,
            info.y,
            info.width,
            info.height,
            info.scale_factor
        );
    }

    let mut source = open_capture_source(&options, screens)?;
    let mut session = Session::new(shard_store, options);
    session.run(&mut source)?;

    println!();
    let (received, total) = session.store.progress();
    info!(
        "汇总: 运行 {:.1}s | 截屏 {} 帧（失败 {}，未变化 {}）| 识别 {} 帧 | 码框 {} / 帧头 {} / 槽 {}（CRC 失败 {}）| 分片 {}/{} | 本次新增 {}",
        session.uptime().as_secs_f64(),
        session.stats.captured,
        session.stats.capture_errors,
        session.stats.frames_unchanged,
        session.stats.frames_scanned,
        session.stats.frames_with_code,
        session.stats.headers_read,
        session.stats.slots_seen,
        session.stats.crc_failures,
        received,
        total,
        session.stats.slots_new
    );
    Ok(())
}

/// 抓屏循环的状态：分片库、计时器、上一次画面与缓存帧头
struct Session {
    store: ShardStore,
    options: Options,
    stats: Stats,
    started: Instant,
    last_data_time: Instant,
    last_report: Instant,
    last_save: Instant,
    last_recheck: Instant,
    last_detect_warn: Option<Instant>,
    last_frame: Vec<u8>,
    cached_header: Option<Header>,
    mismatch_warned: bool,
}

impl Session {
    fn new(store: ShardStore, options: Options) -> Self {
        let cached_header = store.header.clone();
        let now = Instant::now();
        Self {
            store,
            options,
            stats: Stats::default(),
            started: now,
            last_data_time: now,
            last_report: now,
            last_save: now,
            last_recheck: now,
            last_detect_warn: None,
            last_frame: Vec::new(),
            cached_header,
            mismatch_warned: false,
        }
    }

    fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    fn run(&mut self, source: &mut CaptureSource) -> Result<()> {
        loop {
            self.stats.loops += 1;

            if self.last_report.elapsed() >= Duration::from_secs(1) {
                self.last_report = Instant::now();
                self.report();
            }

            let capture_started = Instant::now();
            let screenshot = match source.capture() {
                Ok(screenshot) => screenshot,
                Err(error) => {
                    self.stats.capture_errors += 1;
                    if self.stats.capture_errors <= 3
                        || self.stats.capture_errors.is_multiple_of(50)
                    {
                        error!(
                            "截屏失败（累计 {} 次）: {}",
                            self.stats.capture_errors, error
                        );
                    }
                    if source.is_window() && self.stats.capture_errors >= 5 {
                        return Err(anyhow!(
                            "窗口截图连续失败 {} 次，已停止：{}",
                            self.stats.capture_errors,
                            error
                        ));
                    }
                    thread::sleep(Duration::from_millis(self.options.interval_ms));
                    continue;
                }
            };
            self.stats.captured += 1;
            let capture_elapsed = capture_started.elapsed();
            self.stats.capture_ms += capture_elapsed.as_millis();

            let width = screenshot.width() as usize;
            let height = screenshot.height() as usize;
            let rgba = screenshot.rgba();

            let frame_changed = rgba[..] != self.last_frame[..];
            if frame_changed {
                self.last_frame.clear();
                self.last_frame.extend_from_slice(rgba);
            } else {
                self.stats.frames_unchanged += 1;
            }

            let recheck_due =
                self.last_recheck.elapsed() >= Duration::from_millis(self.options.recheck_ms);
            if frame_changed || recheck_due {
                self.last_recheck = Instant::now();
                let gray = to_gray(rgba, width, height);
                let detect_started = Instant::now();
                let result = decode_frame(
                    &gray,
                    width,
                    height,
                    self.cached_header.as_ref(),
                    &DecodeOptions {
                        rect: self.options.rect,
                    },
                );
                let detect_elapsed = detect_started.elapsed();
                self.stats.detect_ms += detect_elapsed.as_millis();

                match result {
                    Ok(decoded) => self.handle_decoded(
                        decoded,
                        width,
                        height,
                        capture_elapsed,
                        detect_elapsed,
                    )?,
                    Err(error) => {
                        self.stats.detect_errors += 1;
                        if self
                            .last_detect_warn
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
                        {
                            self.last_detect_warn = Some(Instant::now());
                            warn!(
                                "识别失败（累计 {} 次）: {}。确认播放页面在窗口内、未被最小化/缩放；\
                                 画面里明明有码却一直失败，多半是码图大于页面可视区被裁了（浏览器工具栏/任务栏会吃掉可视区），\
                                 按页面面板打印的“页面可视区 WxH”重新生成；必要时用 --rect x,y,w,h 指定码框",
                                self.stats.detect_errors, error
                            );
                        }
                    }
                }
            }

            // 每组都有 >= k 个有效分片：可以恢复了
            let (ready, groups) = self.store.groups_ready();
            if groups > 0 && ready == groups {
                finish(&mut self.store, &self.options)?;
                return Ok(());
            }

            let idle_expired = self
                .options
                .idle_timeout
                .is_some_and(|limit| self.last_data_time.elapsed() > limit);
            if idle_expired && self.store.received_count() > 0 {
                let (received, total) = self.store.progress();
                info!(
                    "空闲超时: {} 内没有收到新分片（本次新增 {}），已收集 {}/{}",
                    self.options
                        .idle_timeout
                        .map(|limit| format!("{:.1}s", limit.as_secs_f64()))
                        .unwrap_or_else(|| "-".to_string()),
                    self.stats.slots_new,
                    received,
                    total
                );
                report_shortfall(&self.store);
                self.store.save_state()?;
                return Err(anyhow!("空闲超时，未收满；分片库已保存，可 --resume 继续"));
            }

            if self.last_save.elapsed() >= Duration::from_secs(SAVE_INTERVAL_SECONDS) {
                self.last_save = Instant::now();
                self.store.save_state()?;
            }

            thread::sleep(Duration::from_millis(self.options.interval_ms));
        }
    }

    fn report(&mut self) {
        let (received, total) = self.store.progress();
        let groups = self.store.groups_ready();
        let idle = self.last_data_time.elapsed();
        let uptime = self.uptime();
        debug!(
            "{}",
            self.stats.report(received, total, groups, idle, uptime)
        );
    }

    fn handle_decoded(
        &mut self,
        decoded: DecodedFrame,
        width: usize,
        height: usize,
        capture_elapsed: Duration,
        detect_elapsed: Duration,
    ) -> Result<()> {
        self.stats.frames_scanned += 1;
        self.stats.frames_with_code += 1;
        self.stats.headers_read += 1;
        if self.cached_header.as_ref() != Some(&decoded.header) {
            self.cached_header = Some(decoded.header.clone());
        }

        let same_session = match self.store.header.as_ref() {
            None => true,
            Some(existing) => existing == &decoded.header,
        };

        if same_session {
            if self.store.ensure(&decoded.header)? {
                let header = &decoded.header;
                info!(
                    "会话建立: {} | 分片 {} | 每组 k={} m={} | 码 {}x{} 模块（实测 {:.3}px）| 原始冗余 {:.0}%",
                    format::format_bytes(header.file_len),
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
                );
            }

            let total = self.store.total_shards();
            let mut new_here = 0usize;
            for rec in &decoded.slots {
                self.stats.slots_seen += 1;
                if rec.idx == PAD_SLOT {
                    continue;
                }
                let idx = rec.idx as usize;
                if idx >= total {
                    continue;
                }
                if self.store.ingest(idx, &rec.payload)? {
                    self.stats.slots_new += 1;
                    self.stats.new_bytes += rec.payload.len() as u64;
                    new_here += 1;
                    self.last_data_time = Instant::now();
                }
            }

            if decoded.crc_failures > 0 {
                self.stats.crc_failures += decoded.crc_failures as u64;
            }

            debug!(
                "帧: {}x{} | rect=({},{},{},{}) | m={:.3}/{:.3} | 槽 {}/{}（CRC 失败 {}，新增 {}）| 截屏 {}ms 识别 {}ms",
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
            );
        } else {
            self.stats.slots_seen += decoded.slots.len() as u64;
            if !self.mismatch_warned {
                self.mismatch_warned = true;
                warn!("屏幕上出现了另一个文件的码（帧头不一致），本帧已忽略；如需切换请先 --clean");
            }
        }

        Ok(())
    }
}

fn open_store(options: &Options) -> Result<ShardStore> {
    let paths = store::ShardPaths::current();
    if options.resume {
        match ShardStore::open(paths.clone()) {
            Ok(Some(mut store)) => {
                let (received, total) = store.progress();
                info!("续传: 已收集 {}/{} 个分片", received, total);
                if store.header.is_some() {
                    let deficits = store.group_deficits();
                    if deficits.is_empty() {
                        info!("缺口已全部可恢复，继续接收即可");
                    } else {
                        let missing: usize = deficits.iter().map(|(_, count)| count).sum();
                        info!(
                            "还有 {} 个组不满足 k，合计缺 {} 个分片（继续播放即可，纠删码会补）",
                            deficits.len(),
                            missing
                        );
                    }
                }
                store.save_state()?;
                return Ok(store);
            }
            Ok(None) => {
                info!("没有可续传的分片库，重新开始");
            }
            Err(error) => {
                warn!("无法续传: {}，将重新开始", error);
            }
        }
    } else if paths.state.exists() {
        info!(
            "发现上次的分片库 {}；用 --resume 可继续接收",
            paths.state.display()
        );
    }

    let store = ShardStore::create(paths)?;
    Ok(store)
}

fn finish(store: &mut ShardStore, options: &Options) -> Result<()> {
    let Some(header) = store.header.clone() else {
        return Err(anyhow!("没有帧头，无法恢复"));
    };

    let (received, total) = store.progress();
    info!(
        "分片足够（{}/{}，k={} m={}），开始纠删恢复…",
        received, total, header.k, header.m
    );
    let started = Instant::now();
    let bytes = store.assemble()?;
    let digest_bytes = digest::sha256(&bytes);
    let hash_hex = hex::encode(digest_bytes);
    let prefix_ok = digest_bytes[..8] == header.hash_prefix;

    info!(
        "恢复完成: {} 字节 | 耗时 {:.2}s | SHA-256 {} | 前缀校验 {}",
        format::format_bytes(bytes.len() as u64),
        started.elapsed().as_secs_f64(),
        hash_hex,
        if prefix_ok { "通过" } else { "不匹配" }
    );

    if !prefix_ok {
        return Err(anyhow!(
            "SHA-256 前缀与帧头不一致，文件可能损坏；分片库已保留，可 --resume 继续收"
        ));
    }

    let name = options
        .out
        .clone()
        .unwrap_or_else(|| format!("received_file_{}", clock::timestamp_compact()));
    fs::write(&name, &bytes)?;
    info!("已写入 {}", name);

    // 成功后才清分片库；Windows 上必须先关句柄
    store.close();
    let _ = store::clean(Path::new("."));

    println!();
    println!("═ 传输完成 ═");
    println!("文件: {}", name);
    println!("大小: {}", format::format_bytes(bytes.len() as u64));
    println!("SHA-256: {}", hash_hex);
    println!();
    Ok(())
}

fn report_shortfall(store: &ShardStore) {
    if store.header.is_none() {
        info!(
            "整个过程都没有读出帧头：确认播放页面在 HSR 窗口内、窗口未最小化；或先用 client --probe 确认窗口尺寸"
        );
        return;
    }
    let deficits = store.group_deficits();
    if deficits.is_empty() {
        info!("所有组其实都已满足 k，可能只是刚好在空闲窗口内没触发完成");
        return;
    }
    let missing: usize = deficits.iter().map(|(_, count)| count).sum();
    let preview: Vec<String> = deficits
        .iter()
        .take(10)
        .map(|(group, need)| format!("组{}缺{}", group, need))
        .collect();
    info!(
        "还有 {} 个组不满足 k，合计缺 {} 个分片（{}）。把发送端的 --parity 提高（例如 0.2）重新生成一轮即可",
        deficits.len(),
        missing,
        preview.join("，")
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
        {
            matches!(self, CaptureSource::Window(_))
        }
        #[cfg(not(windows))]
        {
            false
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
            debug!(
                "客户区变化 → {}（标题「{}」）",
                self.frame.describe(),
                current.title
            );
        }
        screencode::win::capture_client(self.target.handle)
    }
}

fn open_capture_source(options: &Options, screens: Vec<Screen>) -> Result<CaptureSource> {
    if options.fullscreen {
        let screen = screens
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("没有找到屏幕"))?;
        info!(
            "来源: 整屏 #{}（{}x{}）",
            screen.display_info.id, screen.display_info.width, screen.display_info.height
        );
        return Ok(CaptureSource::Screen { screen });
    }

    #[cfg(windows)]
    {
        let target =
            screencode::win::find(Some(&options.process), options.window.as_deref()).map_err(
                |error| {
                    anyhow!(
                        "没有找到匹配的窗口（进程 {}）：{}。可用 --list-windows 查看，或加 --fullscreen 抓整屏",
                        options.process,
                        error
                    )
                },
            )?;
        debug!("匹配到: {}", target.describe());

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

        info!(
            "来源: 进程 {}「{}」客户区 {}（后台截图，窗口在前台或后台都可以）",
            target.process,
            target.title,
            target.frame.describe()
        );
        info!(
            "窗口客户区 {}x{}（含浏览器标签栏/地址栏/任务栏）。码图必须 <= 播放页面可视区才能 1:1 放下，\
             页面面板会打印“页面可视区 WxH”，请用那个尺寸生成；否则码图被裁、这里会一直报“没有找到码框”",
            target.frame.width, target.frame.height
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
        println!("共 {} 个可见窗口（按客户区面积排序）：", windows.len());
        for window in &windows {
            println!(
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
        }
        println!();
        println!("按进程抓: ./server --process <进程名>   按标题抓: ./server --window <标题片段>");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(anyhow!("窗口捕获只支持 Windows；其它平台请用 --fullscreen"))
    }
}
