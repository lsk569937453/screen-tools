use anyhow::{anyhow, Result};
use axum::{routing::get, Json, Router};
use axum_embed::ServeEmbed;
use base64::Engine;
use qrcode::render::svg;
use qrcode::QrCode;
use rust_embed::RustEmbed;
use serde::Serialize;
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::io::Write;
use std::path::Path;
use tower_http::services::ServeDir;

const CHUNK_SIZE: usize = 2000; // 每个 QR 码存储的 base64 字符数
const SLIDE_INTERVAL_MS: u64 = 1000; // 轮播间隔（毫秒）
const HTTP_PORT: u16 = 9090; // HTTP 服务端口
const QR_OUTPUT_DIR: &str = "qr_output"; // SVG 文件目录
const HTML_OUTPUT_DIR: &str = "web"; // HTML 文件目录

/// 嵌入 Web 静态资源
#[derive(RustEmbed, Clone)]
#[folder = "web/"]
struct Assets;

/// QR 码信息响应
#[derive(Serialize)]
struct QrInfo {
    total: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() >= 2 {
        // 有参数：生成 SVG 模式
        let file_path = &args[1];
        generate_svg_files(file_path)?;
    } else {
        // 无参数：HTTP 服务模式
        start_http_server().await?;
    }

    Ok(())
}

/// 生成 SVG 文件模式
fn generate_svg_files(file_path: &str) -> Result<()> {
    // 读取文件
    let file_content = read_file(file_path)?;

    // 转换为 base64
    let base64_data = to_base64(&file_content)?;

    // 切片数据
    let chunks = split_data(&base64_data, CHUNK_SIZE);

    println!("文件: {}", file_path);
    println!("Base64 长度: {}", base64_data.len());
    println!("切片数: {}", chunks.len());
    println!();
    println!("正在生成二维码文件...");

    // 创建 SVG 输出目录
    let qr_dir = Path::new(QR_OUTPUT_DIR);
    fs::create_dir_all(qr_dir)?;

    // 创建 HTML 输出目录
    let html_dir = Path::new(HTML_OUTPUT_DIR);
    fs::create_dir_all(html_dir)?;

    // 生成所有 SVG 文件
    generate_qr_svgs(&chunks, qr_dir)?;

    println!();
    println!("✓ 生成完成!");
    println!("  - 二维码文件: {}/", QR_OUTPUT_DIR);
    println!("  - 网页文件: {}/index.html", HTML_OUTPUT_DIR);
    println!();
    println!(
        "提示: 用浏览器打开 {}/index.html 即可开始轮播",
        HTML_OUTPUT_DIR
    );

    Ok(())
}

/// HTTP 服务模式
async fn start_http_server() -> Result<()> {
    println!("╔════════════════════════════════════════╗");
    println!("║       QR 码文件传输 - HTTP 服务         ║");
    println!("╚════════════════════════════════════════╝");
    println!();
    println!("服务地址: http://localhost:{}", HTTP_PORT);
    println!("提示: 请先将文件转换为二维码（使用带参数运行）");
    println!();

    // 构建路由
    let serve_assets = ServeEmbed::<Assets>::new();
    let app = Router::new()
        // API 路由
        .route("/api/info", get(get_qr_info))
        // 嵌入 web 目录的静态文件
        .nest_service("/", serve_assets)
        // SVG 文件路由 - 从文件系统读取
        .nest_service(&format!("/{}", QR_OUTPUT_DIR), ServeDir::new(QR_OUTPUT_DIR));

    // 启动服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", HTTP_PORT)).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// 获取 QR 码信息 API
async fn get_qr_info() -> Json<QrInfo> {
    let total = fs::read_dir(QR_OUTPUT_DIR)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);

    Json(QrInfo { total })
}

/// 读取文件内容
fn read_file(path: &str) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|e| anyhow!("无法打开文件 '{}': {}", path, e))?;

    let metadata = file
        .metadata()
        .map_err(|e| anyhow!("无法读取文件元数据: {}", e))?;

    let file_size = metadata.len();
    if file_size == 0 {
        return Err(anyhow!("文件为空"));
    }

    let mut buffer = Vec::with_capacity(file_size as usize);
    file.read_to_end(&mut buffer)
        .map_err(|e| anyhow!("读取文件失败: {}", e))?;

    Ok(buffer)
}

/// 将字节数据转换为 base64 字符串
fn to_base64(data: &[u8]) -> Result<String> {
    Ok(base64::engine::general_purpose::STANDARD.encode(data))
}

/// 将数据切片
fn split_data(data: &str, chunk_size: usize) -> Vec<String> {
    data.as_bytes()
        .chunks(chunk_size)
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect()
}

/// 生成所有二维码 SVG 文件
fn generate_qr_svgs(chunks: &[String], output_dir: &Path) -> Result<()> {
    for (idx, chunk) in chunks.iter().enumerate() {
        // 生成二维码
        let qr_code =
            QrCode::new(chunk.as_bytes()).map_err(|e| anyhow!("生成二维码失败: {}", e))?;

        // 渲染为 SVG
        let svg_string = qr_code
            .render::<svg::Color>()
            .min_dimensions(200, 200)
            .max_dimensions(400, 400)
            .quiet_zone(true)
            .build();

        // 保存 SVG 文件
        let filename = output_dir.join(format!("qr_{:03}.svg", idx + 1));
        let mut file = File::create(&filename).map_err(|e| anyhow!("创建文件失败: {}", e))?;
        file.write_all(svg_string.as_bytes())
            .map_err(|e| anyhow!("写入文件失败: {}", e))?;

        println!(
            "  [{:3}/{}] 生成 qr_{:03}.svg ({} bytes)",
            idx + 1,
            chunks.len(),
            idx + 1,
            chunk.len()
        );
    }

    Ok(())
}
