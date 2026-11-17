use anyhow::{anyhow, Result};
use axum::extract::Request;
use axum::http::header;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::{routing::get, Json, Router};
use axum_embed::ServeEmbed;
use base64::Engine;
use qrcode::render::svg;
use qrcode::QrCode;
use rust_embed::RustEmbed;
use serde::Serialize;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use tower_http::services::ServeDir;



const CHUNK_SIZE: usize = 2000; // 每个 QR 码存储的 base64 字符数
const HTTP_PORT: u16 = 9090; // HTTP 服务端口
const QR_OUTPUT_DIR: &str = "qr_output"; // SVG 文件目录
const QR_FILE_PREFIX: &str = "qr_"; // 二维码文件名前缀
const QR_FILE_SUFFIX: &str = ".svg"; // 二维码文件名后缀
const MAX_GENERATE_THREADS: usize = 32; // 生成线程数上限

/// 嵌入 Web 静态资源
#[derive(RustEmbed, Clone)]
#[folder = "web/"]
struct Assets;

/// QR 码信息响应
#[derive(Serialize)]
struct QrInfo {
    total: usize,
    /// 服务实际查找的二维码目录（绝对路径），便于排查"为什么是 0 张"
    dir: String,
}

/// 单张二维码的生成结果
#[derive(Debug, Clone, PartialEq, Eq)]
struct QrResult {
    /// 片段序号（从 1 开始）
    seq: usize,
    /// 该片段承载的原始字符数
    chunk_len: usize,
    /// 生成的 SVG 字节数
    svg_bytes: usize,
}

/// 命令行参数
struct Args {
    file: Option<String>,
    threads: Option<usize>,
    help: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args: Vec<String> = env::args().collect();

    let args = match parse_args(&raw_args) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("参数错误: {}", e);
            print_usage();
            std::process::exit(2);
        }
    };

    if args.help {
        print_usage();
        return Ok(());
    }

    match args.file {
        // 有文件参数：生成 SVG 模式
        Some(file_path) => generate_svg_files(&file_path, args.threads)?,
        // 无文件参数：HTTP 服务模式
        None => start_http_server().await?,
    }

    Ok(())
}

/// 解析命令行参数
///
/// 用法: `client <文件路径> [--threads N]`
/// 无文件参数时进入 HTTP 服务模式。
fn parse_args(raw_args: &[String]) -> Result<Args> {
    let mut file = None;
    let mut threads = None;
    let mut help = false;

    let mut iter = raw_args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => help = true,
            "--threads" => {
                let value = iter.next().ok_or_else(|| anyhow!("--threads 缺少参数值"))?;
                threads = Some(parse_thread_count(value)?);
            }
            other if other.starts_with("--threads=") => {
                let value = &other["--threads=".len()..];
                threads = Some(parse_thread_count(value)?);
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(anyhow!("未知参数 '{}'", other));
            }
            other => {
                if file.is_some() {
                    return Err(anyhow!("只能指定一个文件，多余参数 '{}'", other));
                }
                file = Some(other.to_string());
            }
        }
    }

    Ok(Args {
        file,
        threads,
        help,
    })
}

fn parse_thread_count(value: &str) -> Result<usize> {
    let parsed: usize = value
        .trim()
        .parse()
        .map_err(|_| anyhow!("--threads 需要正整数，收到 '{}'", value))?;
    if parsed == 0 {
        return Err(anyhow!("--threads 至少为 1"));
    }
    Ok(parsed)
}

fn print_usage() {
    println!("QR 码文件传输 - 发送端");
    println!();
    println!("用法:");
    println!("  client <文件路径> [--threads N]   将文件切成二维码 SVG");
    println!("  client                            启动 HTTP 轮播服务");
    println!("  client --help                     显示帮助");
    println!();
    println!("选项:");
    println!(
        "  --threads N   生成二维码使用的线程数（默认: CPU 逻辑核数，上限 {}）",
        MAX_GENERATE_THREADS
    );
    println!();
    println!("示例:");
    println!("  client ./test.txt");
    println!("  client ./big.zip --threads 8");
}

/// 生成 SVG 文件模式
fn generate_svg_files(file_path: &str, threads: Option<usize>) -> Result<()> {
    // 读取文件
    let file_content = read_file(file_path)?;

    // 转换为 base64
    let base64_data = to_base64(&file_content)?;

    // 切片数据
    let chunks = split_data(&base64_data, CHUNK_SIZE);
    if chunks.is_empty() {
        return Err(anyhow!("切片结果为空"));
    }

    println!("文件: {}", file_path);
    println!("文件大小: {}", format_file_size(file_content.len()));
    println!("Base64 长度: {}", base64_data.len());
    println!("切片数: {}", chunks.len());

    // 解析线程数
    if let Some(requested) = threads {
        if requested > MAX_GENERATE_THREADS {
            println!(
                "警告: --threads {} 超出上限 {}，已钳制",
                requested, MAX_GENERATE_THREADS
            );
        }
    }
    let thread_count = resolve_thread_count(threads, chunks.len());
    println!("线程数: {}", thread_count);
    println!();

    // 创建 SVG 输出目录
    let qr_dir = Path::new(QR_OUTPUT_DIR);
    fs::create_dir_all(qr_dir)?;

    // 清理上次运行残留的二维码文件，避免 /api/info 统计到多余文件
    clean_old_qr_files(qr_dir)?;

    println!("正在生成二维码文件...");

    let started = Instant::now();
    let results = generate_qr_svgs(&chunks, qr_dir, thread_count)?;
    let elapsed = started.elapsed();

    // generate_qr_svgs 已按序号排序，这里按序输出，保证多线程下日志顺序稳定
    for result in &results {
        println!(
            "  [{:3}/{}] 生成 qr_{:03}.svg ({} bytes)",
            result.seq,
            results.len(),
            result.seq,
            result.chunk_len
        );
    }

    let seconds = elapsed.as_secs_f64();
    let rate = if seconds > 0.0 {
        results.len() as f64 / seconds
    } else {
        0.0
    };

    println!();
    println!("✓ 生成完成!");
    println!("  - 二维码文件: {}/ ({} 个)", QR_OUTPUT_DIR, results.len());
    println!(
        "  - 耗时: {:.2}s | 线程: {} | 速率: {:.0} 张/秒",
        seconds, thread_count, rate
    );
    println!();
    println!(
        "提示: 运行 `client`（不带参数）启动 HTTP 服务，浏览器访问 http://localhost:{}",
        HTTP_PORT
    );

    Ok(())
}

/// 计算实际使用的生成线程数
///
/// 依次受限于：用户指定值、上限常量、片段总数、CPU 逻辑核数。
fn resolve_thread_count(requested: Option<usize>, total: usize) -> usize {
    let cpu = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let desired = requested.unwrap_or(cpu).clamp(1, MAX_GENERATE_THREADS);
    desired.min(total.max(1))
}

/// 清理目录下上次运行残留的 `qr_*.svg` 文件
fn clean_old_qr_files(dir: &Path) -> Result<()> {
    let entries =
        fs::read_dir(dir).map_err(|e| anyhow!("读取目录 {} 失败: {}", dir.display(), e))?;

    let mut removed = 0usize;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() && is_qr_svg_file(&path) {
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) => println!("警告: 删除 {} 失败: {}", path.display(), e),
            }
        }
    }

    if removed > 0 {
        println!("已清理上次残留的 {} 个二维码文件", removed);
    }

    Ok(())
}

/// 判断路径是否是二维码 SVG 文件
fn is_qr_svg_file(path: &Path) -> bool {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.starts_with(QR_FILE_PREFIX) && name.ends_with(QR_FILE_SUFFIX),
        None => false,
    }
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
        .nest_service(&format!("/{}", QR_OUTPUT_DIR), ServeDir::new(QR_OUTPUT_DIR))
        // 页面和二维码都不许缓存，避免重编译后浏览器仍用旧页面/旧码
        .layer(middleware::from_fn(no_cache));

    // 启动服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", HTTP_PORT)).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// 获取 QR 码信息 API
///
/// 只统计 `qr_*.svg`，避免目录中的其它文件污染总数。
async fn get_qr_info() -> Json<QrInfo> {
    let dir = std::env::current_dir()
        .map(|cwd| cwd.join(QR_OUTPUT_DIR).display().to_string())
        .unwrap_or_else(|_| QR_OUTPUT_DIR.to_string());

    let total = fs::read_dir(QR_OUTPUT_DIR)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && is_qr_svg_file(path))
                .count()
        })
        .unwrap_or(0);

    Json(QrInfo { total, dir })
}

/// 禁止浏览器缓存页面与二维码文件
///
/// 页面是编译期嵌进 exe 的，重新编译后若浏览器仍用旧缓存，就会看到旧版单图页面；
/// 而 `qr_001.svg` 每次生成内容都不同，缓存旧文件会直接导致收端数据错乱。
async fn no_cache(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    response.headers_mut().insert(
        header::PRAGMA,
        header::HeaderValue::from_static("no-cache"),
    );
    response
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

/// 渲染单张二维码 SVG
///
/// 载荷格式固定为 `序号/总数|数据`，是收发双方约定，不可更改。
fn render_qr_svg(chunk: &str, seq: usize, total: usize) -> Result<String> {
    let qr_data = format!("{}/{}|{}", seq, total, chunk);

    let qr_code = QrCode::new(qr_data.as_bytes())
        .map_err(|e| anyhow!("第 {} 张二维码生成失败: {}", seq, e))?;

    Ok(qr_code
        .render::<svg::Color>()
        .min_dimensions(200, 200)
        .max_dimensions(400, 400)
        .quiet_zone(true)
        .build())
}

/// 多线程生成所有二维码 SVG 文件，返回按序号排序的结果
///
/// 每个线程通过原子计数器动态领号，天然负载均衡；生成结果先落在各线程本地，
/// 最后统一按 `seq` 排序，保证输出顺序与单线程完全一致。
fn generate_qr_svgs(chunks: &[String], output_dir: &Path, threads: usize) -> Result<Vec<QrResult>> {
    let total = chunks.len();
    if total == 0 {
        return Ok(Vec::new());
    }

    let threads = threads.clamp(1, MAX_GENERATE_THREADS).min(total);
    let next_index = AtomicUsize::new(0);
    let (progress_tx, progress_rx) = mpsc::channel::<usize>();
    let mut results: Vec<QrResult> = Vec::with_capacity(total);

    thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::with_capacity(threads);

        for _ in 0..threads {
            let progress_tx = progress_tx.clone();
            let next_index = &next_index;

            handles.push(scope.spawn(move || -> Result<Vec<QrResult>> {
                let mut local: Vec<QrResult> = Vec::new();

                loop {
                    let idx = next_index.fetch_add(1, Ordering::Relaxed);
                    if idx >= total {
                        break;
                    }

                    let seq = idx + 1;
                    let chunk = &chunks[idx];
                    let svg_string = render_qr_svg(chunk, seq, total)?;

                    let filename = output_dir.join(format!("qr_{:03}.svg", seq));
                    fs::write(&filename, svg_string.as_bytes())
                        .map_err(|e| anyhow!("写入文件 {} 失败: {}", filename.display(), e))?;

                    local.push(QrResult {
                        seq,
                        chunk_len: chunk.len(),
                        svg_bytes: svg_string.len(),
                    });

                    // 发送进度失败说明主线程已退出，直接结束即可
                    if progress_tx.send(seq).is_err() {
                        break;
                    }
                }

                Ok(local)
            }));
        }

        // 主线程持有的一份 sender 必须丢弃，否则进度循环永远等不到通道关闭
        drop(progress_tx);

        let mut done = 0usize;
        for _ in progress_rx {
            done += 1;
            print!("\r生成进度: {}/{} ({}%)", done, total, done * 100 / total);
            let _ = std::io::stdout().flush();
        }
        println!();

        // 所有线程都会 join，错误取第一个上报，避免静默失败
        let mut first_error: Option<anyhow::Error> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(partial)) => results.extend(partial),
                Ok(Err(e)) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(anyhow!("生成线程异常退出"));
                    }
                }
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })?;

    // 最后排序：多线程完成顺序不确定，这里恢复严格序号顺序
    results.sort_by_key(|r| r.seq);

    Ok(results)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    /// 创建唯一的临时输出目录
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qr_gen_test_{}_{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir
    }

    /// 构造确定性的测试片段
    fn sample_chunks(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| format!("{}{}", "QUJD".repeat(20), i))
            .collect()
    }

    fn file_name(seq: usize) -> String {
        format!("qr_{:03}.svg", seq)
    }

    #[test]
    fn split_data_keeps_all_bytes() {
        let data = "ABCDEFGHIJ".repeat(3); // 30 个字符
        let chunks = split_data(&data, 7);

        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0].len(), 7);
        assert_eq!(chunks[4].len(), 2);
        assert_eq!(chunks.concat(), data);
    }

    #[test]
    fn render_qr_svg_is_deterministic() {
        let first = render_qr_svg("abc123", 1, 2).expect("渲染失败");
        let second = render_qr_svg("abc123", 1, 2).expect("渲染失败");
        assert_eq!(first, second);
        assert!(first.contains("<svg"));
    }

    #[test]
    fn thread_count_is_clamped() {
        // 上限
        assert!(resolve_thread_count(Some(9999), 100) <= MAX_GENERATE_THREADS);
        // 至少 1
        assert_eq!(resolve_thread_count(Some(1), 100), 1);
        // 不超过片段数
        assert_eq!(resolve_thread_count(Some(8), 3), 3);
        // 空集合也有 1 个线程，避免除零
        assert_eq!(resolve_thread_count(Some(8), 0), 1);
    }

    #[test]
    fn multithreaded_generation_is_ordered_and_complete() {
        let dir = temp_dir();
        let chunks = sample_chunks(37);
        let total = chunks.len();

        let results = generate_qr_svgs(&chunks, &dir, 8).expect("生成失败");

        // 数量与顺序
        assert_eq!(results.len(), total);
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.seq, i + 1, "结果必须按序号排序");
        }

        // 文件齐全、命名连续
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("读取目录失败")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        let expected: Vec<String> = (1..=total).map(file_name).collect();
        assert_eq!(names, expected);

        // 每个文件的内容必须对应它自己的序号，证明多线程没有错位
        for (idx, chunk) in chunks.iter().enumerate() {
            let seq = idx + 1;
            let content = fs::read_to_string(dir.join(file_name(seq))).expect("读取失败");
            let expected_svg = render_qr_svg(chunk, seq, total).expect("渲染失败");
            assert_eq!(content, expected_svg, "片段 #{} 内容错位", seq);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_and_multi_thread_outputs_are_identical() {
        let dir_single = temp_dir();
        let dir_multi = temp_dir();
        let chunks = sample_chunks(25);
        let total = chunks.len();

        let single = generate_qr_svgs(&chunks, &dir_single, 1).expect("单线程生成失败");
        let multi = generate_qr_svgs(&chunks, &dir_multi, 8).expect("多线程生成失败");

        assert_eq!(single, multi, "排序后的结果结构必须一致");

        for seq in 1..=total {
            let a = fs::read(dir_single.join(file_name(seq))).expect("读取失败");
            let b = fs::read(dir_multi.join(file_name(seq))).expect("读取失败");
            assert_eq!(a, b, "片段 #{} 两种线程数的输出不一致", seq);
        }

        let _ = fs::remove_dir_all(&dir_single);
        let _ = fs::remove_dir_all(&dir_multi);
    }

    #[test]
    fn clean_old_qr_files_only_removes_qr_svgs() {
        let dir = temp_dir();
        fs::write(dir.join("qr_001.svg"), b"old").expect("写入失败");
        fs::write(dir.join("qr_002.svg"), b"old").expect("写入失败");
        fs::write(dir.join("keep.txt"), b"keep").expect("写入失败");

        clean_old_qr_files(&dir).expect("清理失败");

        assert!(!dir.join("qr_001.svg").exists());
        assert!(!dir.join("qr_002.svg").exists());
        assert!(dir.join("keep.txt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_args_handles_threads_and_file() {
        let args: Vec<String> = ["client", "a.bin", "--threads", "4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_args(&args).expect("解析失败");
        assert_eq!(parsed.file.as_deref(), Some("a.bin"));
        assert_eq!(parsed.threads, Some(4));

        let args: Vec<String> = ["client", "--threads=6", "b.bin"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_args(&args).expect("解析失败");
        assert_eq!(parsed.file.as_deref(), Some("b.bin"));
        assert_eq!(parsed.threads, Some(6));

        // 无参数 = HTTP 模式
        let args: Vec<String> = vec!["client".to_string()];
        let parsed = parse_args(&args).expect("解析失败");
        assert!(parsed.file.is_none());
        assert!(!parsed.help);
    }

    #[test]
    fn parse_args_rejects_invalid_threads() {
        let args: Vec<String> = ["client", "a.bin", "--threads", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&args).is_err());

        let args: Vec<String> = ["client", "a.bin", "--threads", "abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&args).is_err());
    }
}
