use anyhow::{anyhow, Result};
use base64::Engine;
use image::GrayImage;
use screenshots::Screen;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CAPTURE_INTERVAL_MS: u64 = 50; // 截屏间隔（毫秒）
const IDLE_TIMEOUT_SECONDS: u64 = 3; // 无新数据超时时间（秒）
const SAVE_FILE: &str = ".qr_session.json"; // 会话保存文件
const SAVE_INTERVAL_SECONDS: u64 = 1; // 内存会话定时落盘间隔（秒）

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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            resume: false,
            clean: false,
            verbose: false,
            interval_ms: CAPTURE_INTERVAL_MS,
        }
    }
}

/// 一次截屏的检测结果
struct Batch {
    /// 成功解码出的载荷（可能来自同一屏的多张二维码）
    payloads: Vec<String>,
    /// 检测到的二维码网格数量
    grids: usize,
    /// 网格存在但解码失败的个数
    decode_failed: usize,
}

impl Batch {
    fn empty() -> Self {
        Self {
            payloads: Vec::new(),
            grids: 0,
            decode_failed: 0,
        }
    }
}

/// 解码异常提示只打一次，避免每帧刷屏
static PANIC_WARNED: AtomicBool = AtomicBool::new(false);

/// 把 panic 收敛成一行提示（rqrr 的断言 panic 已在检测处隔离）
fn warn_panic_once(location: &str) {
    if !PANIC_WARNED.swap(true, Ordering::Relaxed) {
        println!(
            "[警告] 内部解码异常（已跳过该区域）: {}",
            if location.is_empty() {
                "未知位置"
            } else {
                location
            }
        );
        println!("[警告] 若频繁出现，通常是屏幕上的二维码太小，请减少每屏张数");
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
fn main() {
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

    if let Err(e) = main_with_error(&args) {
        println!("\n错误: {}", e);
        std::process::exit(1);
    }
}

fn main_with_error(args: &[String]) -> Result<()> {
    // 解析命令行参数
    let opts = parse_options(args)?;
    let is_resume = opts.resume;

    // 清除保存的会话
    if opts.clean {
        if PathBuf::from(SAVE_FILE).exists() {
            fs::remove_file(SAVE_FILE)?;
            println!("已清除保存的会话");
        }
        return Ok(());
    }

    println!("╔════════════════════════════════════════╗");
    println!("║       QR 码文件接收器                  ║");
    println!("╚════════════════════════════════════════╝");

    // 打印会话文件的绝对路径，避免从不同工作目录启动时误以为没有保存
    if let Ok(dir) = std::env::current_dir() {
        println!("会话文件: {}", dir.join(SAVE_FILE).display());
    }
    println!();

    // 尝试加载之前的会话
    let (mut received_chunks, mut expected_total) = if is_resume {
        match load_session() {
            Ok(session) => {
                println!("✓ 已加载上次会话");
                println!("  已收集: {} 片段", session.chunks.len());
                if let Some(total) = session.expected_total {
                    println!("  总数: {} 片段", total);
                }
                (session.chunks, session.expected_total)
            }
            Err(e) => {
                println!("警告: 无法加载会话: {}，将重新开始", e);
                (HashMap::new(), None)
            }
        }
    } else {
        // 检查是否有旧会话
        if PathBuf::from(SAVE_FILE).exists() {
            println!("提示: 发现之前的会话，使用 --resume 参数可继续");
        }
        (HashMap::new(), None)
    };

    println!();
    println!("开始监听屏幕二维码...");

    // 获取主屏幕
    let screen = Screen::all()
        .map_err(|e| anyhow!("获取屏幕失败: {}", e))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("没有找到屏幕"))?;

    let mut last_data_time = Instant::now();
    let mut session_dirty = false;
    let mut last_save = Instant::now();
    let mut loop_count = 0;
    // 上一帧的原始像素，用于跳过"画面没变"的重复检测
    let mut last_frame: Vec<u8> = Vec::new();

    loop {
        loop_count += 1;
        if loop_count % 25 == 0 {
            // 每 5 秒输出一次状态
            if let Some(total) = expected_total {
                println!("[运行中] 已收集: {}/{} 片段", received_chunks.len(), total);
            } else {
                println!("[运行中] 已收集: {} 片段 (总数未知)", received_chunks.len());
            }
        }

        // 截屏
        let screenshot = match screen.capture() {
            Ok(screenshot) => screenshot,
            Err(e) => {
                println!("[错误] 截屏失败: {}", e);
                thread::sleep(Duration::from_millis(opts.interval_ms));
                continue;
            }
        };

        let width = screenshot.width() as usize;
        let height = screenshot.height() as usize;
        let rgba_data = screenshot.rgba();

        // 画面与上一帧完全相同时跳过检测：二维码轮播每 300ms 才翻一页，
        // 中间的重复帧占绝大多数，跳过它们能省下大量 CPU。
        let frame_changed = rgba_data[..] != last_frame[..];
        if frame_changed {
            last_frame.clear();
            last_frame.extend_from_slice(rgba_data);
        }

        if frame_changed {
            match detect_from_rgba(rgba_data, width, height, opts.verbose) {
                Ok(batch) if !batch.payloads.is_empty() => {
                    let had_total = expected_total.is_some();
                    let outcome = process_payloads(
                        &batch.payloads,
                        &mut received_chunks,
                        &mut expected_total,
                    );

                    // 首次确定总数
                    if !had_total {
                        if let Some(total) = expected_total {
                            println!("[信息] 总片段数: {}", total);
                            session_dirty = true;
                        }
                    }

                    for line in &outcome.errors {
                        println!("[警告] {}", line);
                    }

                    if outcome.conflicts > 0 {
                        println!(
                            "[警告] 本帧有 {} 张码的总数与预期 ({}) 不符",
                            outcome.conflicts,
                            expected_total.unwrap_or(0)
                        );
                    }

                    if !outcome.new_seqs.is_empty() {
                        last_data_time = Instant::now();
                        session_dirty = true;

                        if opts.verbose {
                            for &seq in &outcome.new_seqs {
                                if let Some(chunk) = received_chunks.get(&seq) {
                                    println!(
                                        "[收到] 片段 #{:>3} | 数据大小: {} bytes",
                                        seq,
                                        chunk.data.len()
                                    );
                                }
                            }
                        }

                        println!(
                            "[批次] 本帧 {} 张码 (解码失败 {}) → 新增 {} 个片段 {} | 已收集: {}/{}",
                            batch.payloads.len(),
                            batch.decode_failed,
                            outcome.new_seqs.len(),
                            summarize_seqs(&outcome.new_seqs),
                            received_chunks.len(),
                            expected_total.unwrap_or(0)
                        );
                    } else if is_resume {
                        // 续传时屏幕上仍停在已收集的片段，说明发送端仍在工作；
                        // 不能因为“没有新片段”而超时退出，继续等待缺失片段。
                        last_data_time = Instant::now();
                    }
                }
                Ok(_) => {
                    // 未检测到二维码，继续
                }
                Err(e) => {
                    println!("[错误] {}", e);
                }
            }
        }

        // 检查是否接收完成
        if let Some(total) = expected_total {
            if received_chunks.len() == total {
                println!();
                println!("所有片段已接收完成！({}/{})", received_chunks.len(), total);
                break;
            }
        }

        // 检查是否超时（没有新数据）
        if !received_chunks.is_empty()
            && last_data_time.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECONDS)
        {
            if let Some(total) = expected_total {
                println!();
                println!(
                    "数据传输超时，已收集 {}/{} 片段",
                    received_chunks.len(),
                    total
                );

                // 如果有缺失片段，保存会话并显示缺失信息
                if received_chunks.len() < total {
                    save_session(&received_chunks, expected_total)?;

                    let missing = find_missing_chunks(&received_chunks, total);
                    println!();
                    println!("═ 缺失片段详情 ═");
                    if missing.len() <= 20 {
                        println!("缺失: {:?}", missing);
                    } else {
                        println!("缺失: {} 个片段", missing.len());
                        println!("前20个: {:?}", &missing[..20]);
                    }
                    println!();
                    println!("会话已保存到: {}", SAVE_FILE);
                    println!("下次运行使用: ./server --resume  继续接收");
                    println!();
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
                    println!("[保存] 已写入 {} 个片段", received_chunks.len());
                }
                Err(e) => {
                    // 保留 dirty，下一秒再试；同时避免每个循环都刷屏
                    last_save = Instant::now();
                    println!("[警告] 定时保存会话失败: {}", e);
                }
            }
        }

        thread::sleep(Duration::from_millis(opts.interval_ms));
    }

    // 重组数据
    if received_chunks.is_empty() {
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
/// 用法: `server [--resume] [--clean] [--verbose] [--interval-ms N]`
fn parse_options(args: &[String]) -> Result<Options> {
    let mut opts = Options::default();

    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--resume" => opts.resume = true,
            "--clean" => opts.clean = true,
            "-v" | "--verbose" => opts.verbose = true,
            "--interval-ms" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow!("--interval-ms 缺少参数值"))?;
                opts.interval_ms = parse_interval_ms(value)?;
            }
            other if other.starts_with("--interval-ms=") => {
                opts.interval_ms = parse_interval_ms(&other["--interval-ms=".len()..])?;
            }
            "-h" | "--help" => {
                println!("用法: server [--resume] [--clean] [--verbose] [--interval-ms N]");
                println!("  --resume        继续上次未完成的会话");
                println!("  --clean         清除保存的会话");
                println!("  --verbose       输出每张二维码的解码细节");
                println!(
                    "  --interval-ms   截屏间隔毫秒数（默认 {}）",
                    CAPTURE_INTERVAL_MS
                );
                std::process::exit(0);
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(anyhow!("未知参数 '{}'", other));
            }
            other => return Err(anyhow!("未知位置参数 '{}'", other)),
        }
    }

    Ok(opts)
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

/// 截屏并批量检测同一帧内的所有二维码
///
/// 发送端一屏最多展示 6 张二维码，这里必须把整帧内所有 grid 都解码出来，
/// 单张失败不影响同一帧的其它二维码。
fn detect_from_rgba(rgba: &[u8], width: usize, height: usize, verbose: bool) -> Result<Batch> {
    // 转换为灰度图像 (rqrr 需要灰度数据)
    let mut gray_data = vec![0u8; width * height];
    for (gray, pixel) in gray_data.iter_mut().zip(rgba.chunks_exact(4)) {
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

/// 整图检测 + 分块检测，两路并行，取并集
///
/// - 整图检测：对 1/2/4/6 宫格的大二维码最有效；
/// - 分块检测：小窗口下二维码排列紧凑时，rqrr 的定位图案分组会串味，
///   切块后每块最多一两张码，可以把漏掉的补回来。
fn detect_multi(gray: GrayImage, verbose: bool) -> Batch {
    let full_gray = gray.clone();

    let (full, tiled) = thread::scope(|scope| {
        let full_handle = scope.spawn(move || detect_from_gray(full_gray, verbose));
        let tile_handle = scope.spawn(|| detect_tiles(&gray, 3, 2, 0.25, verbose));

        (
            full_handle.join().unwrap_or_else(|_| Batch::empty()),
            tile_handle.join().unwrap_or_else(|_| Batch::empty()),
        )
    });

    let mut payloads = full.payloads;
    for payload in tiled.payloads {
        if !payloads.contains(&payload) {
            payloads.push(payload);
        }
    }

    Batch {
        payloads,
        grids: full.grids.max(tiled.grids),
        decode_failed: full.decode_failed + tiled.decode_failed,
    }
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
fn detect_from_gray_inner(gray_image: GrayImage, verbose: bool) -> Batch {
    // 准备图像用于检测
    let mut prepared = rqrr::PreparedImage::prepare(gray_image);

    // 搜索二维码（一屏可能有多张）
    let grids = prepared.detect_grids();
    let grid_count = grids.len();

    let threads = worker_threads().min(grid_count).max(1);
    let per = grid_count.div_ceil(threads).max(1);

    let parts: Vec<(Vec<String>, usize)> = thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut start = 0usize;

        while start < grid_count {
            let end = (start + per).min(grid_count);
            let slice = &grids[start..end];

            handles.push(scope.spawn(move || {
                let mut payloads = Vec::new();
                let mut failed = 0usize;
                for grid in slice {
                    // 单张码解码失败（含 rqrr 内部断言 panic）只算失败，不影响同屏其它码
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| grid.decode())) {
                        Ok(Ok((_metadata, payload))) => payloads.push(payload),
                        _ => failed += 1,
                    }
                }
                (payloads, failed)
            }));

            start = end;
        }

        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });

    let mut payloads = Vec::with_capacity(grid_count);
    let mut decode_failed = 0usize;
    for (part, failed) in parts {
        payloads.extend(part);
        decode_failed += failed;
    }

    if verbose && grid_count > 0 {
        println!(
            "[调试] 检测到 {} 个二维码网格，解码成功 {} 个",
            grid_count,
            payloads.len()
        );
    }

    Batch {
        payloads,
        grids: grid_count,
        decode_failed,
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
    let mut grids = 0usize;
    let mut decode_failed = 0usize;

    for part in parts {
        grids += part.grids;
        decode_failed += part.decode_failed;
        for payload in part.payloads {
            if !payloads.contains(&payload) {
                payloads.push(payload);
            }
        }
    }

    Batch {
        payloads,
        grids,
        decode_failed,
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

                if !received_chunks.contains_key(&seq) {
                    received_chunks.insert(seq, chunk);
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
            println!();
            println!("╔════════════════════════════════════════╗");
            println!("║           警告：片段缺失!             ║");
            println!("╠════════════════════════════════════════╣");
            println!("║  预期片段数: {:26} ║", total);
            println!("║  实际接收数: {:26} ║", received_count);
            println!("╚════════════════════════════════════════╝");
            println!();

            // 列出缺失的片段
            let mut missing: Vec<usize> = Vec::new();
            for i in 1..=total {
                if !chunks.contains_key(&i) {
                    missing.push(i);
                }
            }

            if missing.len() <= 20 {
                println!("缺失的片段: {:?}", missing);
            } else {
                println!("缺失的片段: {} 个 (太多无法列出)", missing.len());
            }
            println!();

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

    println!();
    println!("按序号排序并拼接 {} 个数据片段...", received_count);

    // 按序号排序并拼接数据
    let mut sorted_chunks: Vec<&Chunk> = chunks.values().collect();
    sorted_chunks.sort_by_key(|c| c.seq);

    let combined: String = sorted_chunks
        .iter()
        .map(|c| c.data.as_str())
        .collect::<Vec<&str>>()
        .join("");

    println!("Base64 数据总长度: {} bytes", combined.len());

    // Base64 解码
    println!("解码 Base64...");
    let file_data = base64::engine::general_purpose::STANDARD
        .decode(combined.trim())
        .map_err(|e| anyhow!("Base64 解码失败: {}", e))?;

    println!("解码后文件大小: {} bytes", file_data.len());

    // 生成输出文件名
    let output_filename = generate_output_filename();

    // 保存文件
    let mut file = File::create(&output_filename).map_err(|e| anyhow!("创建文件失败: {}", e))?;
    file.write_all(&file_data)
        .map_err(|e| anyhow!("写入文件失败: {}", e))?;

    println!();
    println!("╔════════════════════════════════════════╗");
    println!("║           文件接收成功!                ║");
    println!("╠════════════════════════════════════════╣");
    println!("║  文件名: {:30} ║", output_filename);
    println!("║  大小:   {:30} ║", format_file_size(file_data.len()));
    println!("║  片段数: {:30} ║", received_count);
    println!("╚════════════════════════════════════════╝");

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

        assert_eq!(batch.grids, 1);
        assert_eq!(batch.payloads, vec!["1/1|ONLY".to_string()]);
    }
}
