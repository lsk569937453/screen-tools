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
use clap::Parser;
use common::{digest, format, logging};
use rust_embed::RustEmbed;
use screencode::fec;
use screencode::header::{Header, HeaderSpec};
use screencode::layout::Geometry;
use screencode::render::render_png;
use screencode::shard::encode_slot;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

mod cli;

use cli::{Cli, FrameSpec};

const LOG_DIR: &str = "logs";
const MAX_CACHED_FRAMES: usize = 4;

#[derive(RustEmbed, Clone)]
#[folder = "web/"]
struct Assets;

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
    let cli = Cli::parse();
    if cli.wants_help() {
        if let Err(error) = cli::print_help() {
            eprintln!("打印帮助失败: {error}");
            std::process::exit(1);
        }
        return;
    }

    let log = match logging::init("client", cli.verbose, Some(FsPath::new(LOG_DIR))) {
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

    if let Err(error) = run(cli).await {
        error!("{error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    if cli.probe {
        return probe_window(&cli.process);
    }

    let file_path = cli
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

    let (frame_w, frame_h) = resolve_frame(&cli);
    let geometry = Geometry::plan(frame_w, frame_h, cli.module_px, cli.block_bytes)?;
    let plan = fec::plan(file.len(), cli.block_bytes, cli.parity)?;

    let digest_bytes = digest::sha256(&file);
    let mut hash_prefix = [0u8; 8];
    hash_prefix.copy_from_slice(&digest_bytes[..8]);
    let hash_hex = hex::encode(digest_bytes);

    let started = std::time::Instant::now();
    let shards = fec::encode_shards(&file, &plan, cli.block_bytes)?;
    let header = Header::new(HeaderSpec {
        module_px: geometry.module_px,
        grid_w: geometry.grid_w,
        grid_h: geometry.grid_h,
        block_bytes: geometry.block_bytes,
        k: plan.k,
        m: plan.m,
        total_shards: plan.total_shards,
        file_len: file.len() as u64,
        hash_prefix,
    });

    let player = Arc::new(Player::new(
        header.clone(),
        geometry,
        shards,
        file_path.to_string(),
        file.len() as u64,
        cli.dwell_ms,
        hash_hex.clone(),
    ));

    info!("文件: {}", file_path);
    info!(
        "大小: {}（{} 字节）| SHA-256: {}",
        format::format_bytes(file.len() as u64),
        file.len(),
        hash_hex
    );
    info!(
        "版式: {}x{} px | 模块 {}px | 网格 {}x{} | 每帧 {} 个分片 / {}",
        geometry.frame_w,
        geometry.frame_h,
        geometry.module_px,
        geometry.grid_w,
        geometry.grid_h,
        player.slots_per_frame,
        format::format_bytes(player.bytes_per_frame() as u64)
    );
    info!(
        "码制: {} KB/帧({}) | k={} m={} | 分片 {} | 总帧数 {} | 停留 {} ms | 编码 {:.1}s",
        player.bytes_per_frame() / 1024,
        format::format_bytes(player.bytes_per_frame() as u64),
        plan.k,
        plan.m,
        plan.total_shards,
        player.frame_count,
        cli.dwell_ms,
        started.elapsed().as_secs_f64()
    );

    if let Some(dir) = cli.export {
        return export_all(&player, &dir);
    }

    serve(player, cli.port).await
}

fn resolve_frame(cli: &Cli) -> (u32, u32) {
    match cli.resolved_frame() {
        FrameSpec::Fixed { width, height } => (width, height),
        FrameSpec::Auto => match detect_frame(&cli.process) {
            Some(frame) => frame,
            None => {
                let (width, height) = (cli::DEFAULT_FRAME_WIDTH, cli::DEFAULT_FRAME_HEIGHT);
                warn!(
                    "没有找到进程 {} 的窗口，回退到 {}x{}",
                    cli.process, width, height
                );
                warn!(
                    "--frame 必须 <= 播放页面的可视区（页面面板里会打印“页面可视区 WxH”），否则码图会被浏览器/任务栏裁掉，接收端找不到完整码框"
                );
                (width, height)
            }
        },
    }
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
    info!(
        "已写出 {} 帧到 {}/（每帧 {}）",
        player.frame_count,
        dir,
        format::format_bytes(player.bytes_per_frame() as u64)
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
            debug!(
                "渲染第 {} 帧（{}）",
                index + 1,
                format::format_bytes(png.len() as u64)
            );
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
            error!("渲染第 {} 帧失败: {}", index + 1, error);
            (StatusCode::INTERNAL_SERVER_ERROR, "渲染失败").into_response()
        }
        Err(error) => {
            error!("渲染任务失败: {}", error);
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
