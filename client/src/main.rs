//! 发送端：把文件切成高密度单张码，起 HTTP 服务让页面逐帧轮播。
//!
//! 一帧 = 一张 `frame_w × frame_h` 的 1-bit PNG（ScreenCode v1）。
//! 页面只显示这一张图，按设备像素 1:1 摆放，循环播放。

use anyhow::{anyhow, Result};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_embed::ServeEmbed;
use rust_embed::RustEmbed;
use screencode::fec;
use screencode::header::Header;
use screencode::layout::Geometry;
use screencode::render::render_png;
use screencode::shard::encode_slot;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

const DEFAULT_PORT: u16 = 9090;
const DEFAULT_PROCESS: &str = "HSR";
const DEFAULT_FRAME: (u32, u32) = (1920, 1080);
const DEFAULT_MODULE_PX: u32 = 2;
const DEFAULT_DWELL_MS: u64 = 300;
const MAX_CACHED_FRAMES: usize = 4;

#[derive(RustEmbed, Clone)]
#[folder = "web/"]
struct Assets;

#[derive(Debug, Clone)]
struct Args {
    file: Option<String>,
    frame: Option<(u32, u32)>,
    auto_frame: bool,
    module_px: u32,
    block_bytes: usize,
    parity: f64,
    dwell_ms: u64,
    export: Option<String>,
    port: u16,
    process: String,
    probe: bool,
    help: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            file: None,
            frame: None,
            auto_frame: false,
            module_px: DEFAULT_MODULE_PX,
            block_bytes: screencode::DEFAULT_BLOCK_BYTES,
            parity: 0.10,
            dwell_ms: DEFAULT_DWELL_MS,
            export: None,
            port: DEFAULT_PORT,
            process: DEFAULT_PROCESS.to_string(),
            probe: false,
            help: false,
        }
    }
}

/// 一帧的播放状态
struct Player {
    header: Header,
    geometry: Geometry,
    shards: Vec<Vec<u8>>,
    slots_per_frame: usize,
    frame_count: usize,
    source: String,
    file_len: u64,
    dwell_ms: u64,
    hash_hex: String,
    cache: Mutex<HashMap<usize, Arc<Vec<u8>>>>,
}

impl Player {
    fn new(
        header: Header,
        geometry: Geometry,
        shards: Vec<Vec<u8>>,
        source: String,
        file_len: u64,
        dwell_ms: u64,
        hash_hex: String,
    ) -> Self {
        let slots_per_frame = geometry.slots_per_frame();
        let frame_count = header.total_shards.div_ceil(slots_per_frame as u32) as usize;
        Self {
            header,
            geometry,
            shards,
            slots_per_frame,
            frame_count,
            source,
            file_len,
            dwell_ms,
            hash_hex,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 生成第 `index` 帧（0 基）的 PNG
    fn frame_png(&self, index: usize) -> Result<Vec<u8>> {
        if index >= self.frame_count {
            return Err(anyhow!(
                "帧号 {} 超出范围 1..={}",
                index + 1,
                self.frame_count
            ));
        }

        if let Some(cached) = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&index)
            .cloned()
        {
            return Ok((*cached).clone());
        }

        let start = index * self.slots_per_frame;
        let end = (start + self.slots_per_frame).min(self.shards.len());
        let slots: Vec<Vec<u8>> = (start..end)
            .map(|i| encode_slot(i as u32, &self.shards[i]))
            .collect();

        let png = render_png(&self.geometry, &self.header, &slots)?;

        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= MAX_CACHED_FRAMES {
            cache.clear();
        }
        cache.insert(index, Arc::new(png.clone()));
        Ok(png)
    }

    fn bytes_per_frame(&self) -> usize {
        self.slots_per_frame * self.header.slot_bytes()
    }
}

#[derive(Serialize)]
struct Info {
    format: &'static str,
    frame_w: u32,
    frame_h: u32,
    module_px: u32,
    grid_w: u32,
    grid_h: u32,
    frame_count: usize,
    slots_per_frame: usize,
    block_bytes: usize,
    k: usize,
    m: usize,
    total_shards: usize,
    file_len: u64,
    dwell_ms: u64,
    source: String,
    hash: String,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("错误: {}", error);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = parse_args(&env::args().collect::<Vec<_>>());

    if args.help {
        print_usage();
        return Ok(());
    }

    if args.probe {
        return probe_window(&args.process);
    }

    let file_path = args
        .file
        .as_deref()
        .ok_or_else(|| anyhow!("缺少文件参数。用法: client <文件路径> [选项]，或 client --help"))?;

    if !FsPath::new(file_path).is_file() {
        return Err(anyhow!("文件不存在或不是普通文件: {}", file_path));
    }

    let file = fs::read(file_path).map_err(|e| anyhow!("读取 {} 失败: {}", file_path, e))?;
    if file.is_empty() {
        return Err(anyhow!("文件为空，无法生成"));
    }

    let (frame_w, frame_h) = resolve_frame(&args)?;
    let geometry = Geometry::plan(frame_w, frame_h, args.module_px, args.block_bytes)?;
    let plan = fec::plan(file.len(), args.block_bytes, args.parity)?;

    let mut hasher = Sha256::new();
    hasher.update(&file);
    let digest = hasher.finalize();
    let mut hash_prefix = [0u8; 8];
    hash_prefix.copy_from_slice(&digest[..8]);
    let hash_hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();

    let started = std::time::Instant::now();
    let shards = fec::encode_shards(&file, &plan, args.block_bytes)?;
    let header = Header::new(
        geometry.module_px,
        geometry.grid_w,
        geometry.grid_h,
        geometry.block_bytes,
        plan.k,
        plan.m,
        plan.total_shards,
        file.len() as u64,
        hash_prefix,
    );

    let player = Arc::new(Player::new(
        header.clone(),
        geometry,
        shards,
        file_path.to_string(),
        file.len() as u64,
        args.dwell_ms,
        hash_hex.clone(),
    ));

    println!("文件: {}", file_path);
    println!(
        "大小: {}（{} 字节）| SHA-256: {}",
        format_file_size(file.len()),
        file.len(),
        hash_hex
    );
    println!(
        "版式: {}x{} px | 模块 {}px | 网格 {}x{} | 每帧 {} 个分片 / {}",
        geometry.frame_w,
        geometry.frame_h,
        geometry.module_px,
        geometry.grid_w,
        geometry.grid_h,
        player.slots_per_frame,
        format_file_size(player.bytes_per_frame())
    );
    println!(
        "码制: {} KB/帧({}) | k={} m={} | 分片 {} | 总帧数 {} | 停留 {} ms | 编码 {:.1}s",
        player.bytes_per_frame() / 1024,
        format_file_size(player.bytes_per_frame()),
        plan.k,
        plan.m,
        plan.total_shards,
        player.frame_count,
        args.dwell_ms,
        started.elapsed().as_secs_f64()
    );

    if let Some(dir) = args.export {
        return export_all(&player, &dir);
    }

    serve(player, args.port).await
}

fn resolve_frame(args: &Args) -> Result<(u32, u32)> {
    if let Some(frame) = args.frame {
        return Ok(frame);
    }
    if args.auto_frame {
        if let Some(frame) = detect_frame(&args.process) {
            return Ok(frame);
        }
        println!(
            "警告: 没有找到进程 {} 的窗口，回退到 {}x{}；可用 --frame WxH 指定",
            args.process, DEFAULT_FRAME.0, DEFAULT_FRAME.1
        );
        return Ok(DEFAULT_FRAME);
    }
    Ok(DEFAULT_FRAME)
}

#[cfg(windows)]
fn detect_frame(process: &str) -> Option<(u32, u32)> {
    let window = screencode::win::find(Some(process), None).ok()?;
    if window.frame.is_empty() {
        None
    } else {
        Some((window.frame.width, window.frame.height))
    }
}

#[cfg(not(windows))]
fn detect_frame(_process: &str) -> Option<(u32, u32)> {
    None
}

fn export_all(player: &Player, dir: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    for index in 0..player.frame_count {
        let png = player.frame_png(index)?;
        let path = FsPath::new(dir).join(format!("frame_{:05}.png", index + 1));
        fs::write(&path, png).map_err(|e| anyhow!("写 {} 失败: {}", path.display(), e))?;
        if (index + 1) % 10 == 0 || index + 1 == player.frame_count {
            print!("\r导出 {}/{}", index + 1, player.frame_count);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
    println!();
    println!(
        "✓ 已写出 {} 帧到 {}/（每帧 {}）",
        player.frame_count,
        dir,
        format_file_size(player.bytes_per_frame())
    );
    Ok(())
}

async fn serve(player: Arc<Player>, port: u16) -> Result<()> {
    println!();
    println!("播放地址: http://localhost:{}/", port);
    println!("提示: 页面默认自动循环播放；按 Esc 退出播放模式，F 全屏");

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    axum::serve(listener, router(player)).await?;
    Ok(())
}

fn router(player: Arc<Player>) -> Router {
    Router::new()
        .route("/api/info", get(info))
        .route("/api/frame/:n", get(frame_png))
        .nest_service("/", ServeEmbed::<Assets>::new())
        .layer(middleware::from_fn(no_cache))
        .with_state(player)
}

async fn info(State(player): State<Arc<Player>>) -> Json<Info> {
    Json(Info {
        format: "screencode/1",
        frame_w: player.geometry.frame_w,
        frame_h: player.geometry.frame_h,
        module_px: player.geometry.module_px,
        grid_w: player.geometry.grid_w,
        grid_h: player.geometry.grid_h,
        frame_count: player.frame_count,
        slots_per_frame: player.slots_per_frame,
        block_bytes: player.header.block_bytes as usize,
        k: player.header.k as usize,
        m: player.header.m as usize,
        total_shards: player.header.total_shards as usize,
        file_len: player.file_len,
        dwell_ms: player.dwell_ms,
        source: player.source.clone(),
        hash: player.hash_hex.clone(),
    })
}

async fn frame_png(State(player): State<Arc<Player>>, Path(name): Path<String>) -> Response {
    let name = name.trim_end_matches(".png");
    let index: usize = match name.parse::<usize>() {
        Ok(value) if value >= 1 && value <= player.frame_count => value - 1,
        _ => return (StatusCode::NOT_FOUND, "帧号无效").into_response(),
    };

    let player = player.clone();
    match tokio::task::spawn_blocking(move || player.frame_png(index)).await {
        Ok(Ok(png)) => {
            let mut response = png.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("image/png"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            response
        }
        Ok(Err(error)) => {
            eprintln!("渲染第 {} 帧失败: {}", index + 1, error);
            (StatusCode::INTERNAL_SERVER_ERROR, "渲染失败").into_response()
        }
        Err(error) => {
            eprintln!("渲染任务失败: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, "渲染任务失败").into_response()
        }
    }
}

/// 页面与图片都不许缓存：每次换文件后必须拿到新图
async fn no_cache(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, header::HeaderValue::from_static("no-cache"));
    response
}

fn parse_args(raw: &[String]) -> Args {
    let mut args = Args::default();
    let mut iter = raw.iter().skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => args.help = true,
            "--probe" => args.probe = true,
            "--auto" | "--frame=auto" => args.auto_frame = true,
            "--frame" => {
                let value = iter.next().map(|v| v.as_str()).unwrap_or("");
                match parse_frame(value) {
                    Some(frame) => args.frame = Some(frame),
                    None => {
                        eprintln!("--frame 需要 WxH，例如 2505x1329；收到 '{}'", value);
                        std::process::exit(2);
                    }
                }
            }
            other if other.starts_with("--frame=") => {
                let value = &other["--frame=".len()..];
                if value == "auto" {
                    args.auto_frame = true;
                } else if let Some(frame) = parse_frame(value) {
                    args.frame = Some(frame);
                } else {
                    eprintln!("--frame 需要 WxH，例如 2505x1329；收到 '{}'", value);
                    std::process::exit(2);
                }
            }
            "--module-px" => {
                args.module_px = parse_num(&mut iter, "--module-px", 1u32);
            }
            other if other.starts_with("--module-px=") => {
                args.module_px = other["--module-px=".len()..].parse().unwrap_or(0).max(1);
            }
            "--block-bytes" => {
                args.block_bytes = parse_num(&mut iter, "--block-bytes", 4096usize);
            }
            other if other.starts_with("--block-bytes=") => {
                args.block_bytes = other["--block-bytes=".len()..].parse().unwrap_or(0);
            }
            "--parity" => {
                args.parity = iter.next().and_then(|v| v.parse().ok()).unwrap_or(0.10);
            }
            other if other.starts_with("--parity=") => {
                args.parity = other["--parity=".len()..].parse().unwrap_or(0.10);
            }
            "--dwell-ms" => {
                args.dwell_ms = parse_num(&mut iter, "--dwell-ms", DEFAULT_DWELL_MS);
            }
            other if other.starts_with("--dwell-ms=") => {
                args.dwell_ms = other["--dwell-ms=".len()..]
                    .parse()
                    .unwrap_or(DEFAULT_DWELL_MS);
            }
            "--export" => {
                args.export = Some(
                    iter.next()
                        .map(|v| v.as_str())
                        .unwrap_or("qr_output")
                        .to_string(),
                );
            }
            other if other.starts_with("--export=") => {
                args.export = Some(other["--export=".len()..].to_string());
            }
            "--port" => {
                args.port = parse_num(&mut iter, "--port", DEFAULT_PORT);
            }
            other if other.starts_with("--port=") => {
                args.port = other["--port=".len()..].parse().unwrap_or(DEFAULT_PORT);
            }
            "--process" => {
                args.process = iter
                    .next()
                    .map(|v| v.as_str())
                    .unwrap_or(DEFAULT_PROCESS)
                    .to_string();
            }
            other if other.starts_with("--process=") => {
                args.process = other["--process=".len()..].to_string();
            }
            other if other.starts_with('-') => {
                eprintln!("未知参数 '{}'", other);
                std::process::exit(2);
            }
            other => {
                if args.file.is_some() {
                    eprintln!("只能指定一个文件，多余参数 '{}'", other);
                    std::process::exit(2);
                }
                args.file = Some(other.to_string());
            }
        }
    }

    args
}

fn parse_num<'a, I, T>(iter: &mut I, name: &str, default: T) -> T
where
    I: Iterator<Item = &'a String>,
    T: std::str::FromStr,
{
    match iter.next() {
        Some(value) => match value.parse() {
            Ok(parsed) => parsed,
            Err(_) => {
                eprintln!("{} 需要数字，收到 '{}'", name, value);
                std::process::exit(2);
            }
        },
        None => default,
    }
}

fn parse_frame(value: &str) -> Option<(u32, u32)> {
    let (w, h) = value.split_once('x').or_else(|| value.split_once('X'))?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w >= 320 && h >= 240 && w <= 8192 && h <= 8192 {
        Some((w, h))
    } else {
        None
    }
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

#[cfg(windows)]
fn probe_window(process: &str) -> Result<()> {
    println!("进程『{}』相关的可见窗口：", process);
    let windows = screencode::win::list_windows()?;
    let mut found = false;
    for window in windows {
        if window
            .process
            .to_lowercase()
            .contains(&process.to_lowercase())
        {
            found = true;
            println!(
                "  - {} 客户区 {}",
                window.describe(),
                window.frame.describe()
            );
        }
    }
    if !found {
        println!("  （没有找到，检查游戏是否已启动、窗口是否最小化）");
    }
    Ok(())
}

#[cfg(not(windows))]
fn probe_window(_process: &str) -> Result<()> {
    Err(anyhow!("窗口探测只支持 Windows；请用 --frame WxH 手动指定"))
}

fn print_usage() {
    println!("ScreenCode 发送端（单张高密度网格码）");
    println!();
    println!("用法:");
    println!("  client <文件路径> [选项]      生成并起 HTTP 播放服务");
    println!("  client <文件路径> --export DIR  只导出 PNG 帧");
    println!("  client --probe [--process HSR] 打印游戏窗口客户区尺寸");
    println!();
    println!("选项:");
    println!("  --frame WxH          目标帧尺寸（默认 1920x1080；--frame auto 读窗口）");
    println!("  --module-px N        每个模块的像素（默认 2；1 更密但要求严格 1:1）");
    println!("  --block-bytes N      分片净荷字节（默认 4096）");
    println!("  --parity F           纠删码冗余比例（默认 0.10，0 表示关闭）");
    println!("  --dwell-ms N         每帧停留毫秒（默认 300）");
    println!("  --port N             HTTP 端口（默认 9090）");
    println!("  --process NAME       自动取窗口尺寸时匹配的进程（默认 HSR）");
    println!("  --export DIR         导出 frame_*.png 到目录后退出");
    println!("  -h, --help           显示帮助");
    println!();
    println!("示例:");
    println!("  client ./big.zip");
    println!("  client ./big.zip --frame auto --module-px 2");
    println!("  client ./big.zip --export qr_output --frame 2505x1329");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_file_and_frame() {
        let parsed = parse_args(&args(&["client", "a.bin", "--frame", "2505x1329"]));
        assert_eq!(parsed.file.as_deref(), Some("a.bin"));
        assert_eq!(parsed.frame, Some((2505, 1329)));
    }

    #[test]
    fn parses_auto_frame_and_options() {
        let parsed = parse_args(&args(&[
            "client",
            "a.bin",
            "--frame=auto",
            "--module-px",
            "1",
            "--parity",
            "0.2",
        ]));
        assert!(parsed.auto_frame);
        assert_eq!(parsed.module_px, 1);
        assert!((parsed.parity - 0.2).abs() < 1e-9);
    }
}
