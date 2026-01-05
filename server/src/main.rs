use anyhow::{anyhow, Result};
use base64::Engine;
use image::GrayImage;
use screenshots::Screen;
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

const CAPTURE_INTERVAL_MS: u64 = 50; // 截屏间隔（毫秒）
const IDLE_TIMEOUT_SECONDS: u64 = 3; // 无新数据超时时间（秒）
fn main() {
    if let Err(e) = main_with_error() {
        println!("{}", e);
    }
}
fn main_with_error() -> Result<()> {
    println!("╔════════════════════════════════════════╗");
    println!("║       QR 码文件接收器                  ║");
    println!("╚════════════════════════════════════════╝");
    println!();
    println!("开始监听屏幕二维码...");

    // 获取主屏幕
    let screen = Screen::all()
        .map_err(|e| anyhow!("获取屏幕失败: {}", e))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("没有找到屏幕"))?;

    let mut received_chunks: Vec<String> = Vec::new();
    let mut seen_hashes: HashSet<u64> = HashSet::new();
    let mut last_data_time = Instant::now();
    let mut loop_count = 0;

    loop {
        loop_count += 1;
        if loop_count % 25 == 0 {
            // 每 5 秒输出一次状态
            println!("[运行中] 已截屏 {} 次，等待二维码...", loop_count);
        }

        // 截屏
        match capture_and_detect_qr(&screen) {
            Ok(Some(data)) => {
                let data_hash = hash_string(&data);

                // 去重：只保存新的数据块
                if !seen_hashes.contains(&data_hash) {
                    seen_hashes.insert(data_hash);
                    received_chunks.push(data.clone());
                    last_data_time = Instant::now();

                    println!(
                        "[收到] 片段 #{:>3} | 大小: {} bytes | 已收集: {} 片段",
                        received_chunks.len(),
                        data.len(),
                        received_chunks.len()
                    );
                }
            }
            Ok(None) => {
                // 未检测到二维码，继续
            }
            Err(e) => {
                println!("[错误] {}", e);
            }
        }

        // 检查是否超时（没有新数据）
        if !received_chunks.is_empty()
            && last_data_time.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECONDS)
        {
            println!();
            println!("检测到数据传输完成，开始重组文件...");
            break;
        }

        thread::sleep(Duration::from_millis(CAPTURE_INTERVAL_MS));
    }

    // 重组数据
    if received_chunks.is_empty() {
        return Err(anyhow!("未接收到任何数据"));
    }

    restore_file(&received_chunks)?;

    Ok(())
}

/// 截屏并检测二维码
fn capture_and_detect_qr(screen: &Screen) -> Result<Option<String>> {
    // 截屏
    let screenshot = screen.capture().map_err(|e| anyhow!("截屏失败: {}", e))?;

    // 获取原始 RGBA 数据
    let width = screenshot.width() as usize;
    let height = screenshot.height() as usize;
    let rgba_data = screenshot.rgba();

    // 转换为灰度图像 (rqrr 需要灰度数据)
    let mut gray_data = vec![0u8; width * height];
    for i in 0..(width * height) {
        let r = rgba_data[i * 4] as u32;
        let g = rgba_data[i * 4 + 1] as u32;
        let b = rgba_data[i * 4 + 2] as u32;
        // 灰度转换公式: 0.299*R + 0.587*G + 0.114*B
        gray_data[i] = ((299 * r + 587 * g + 114 * b) / 1000) as u8;
    }

    let gray_image = GrayImage::from_raw(width as u32, height as u32, gray_data)
        .ok_or_else(|| anyhow!("图像数据转换失败"))?;

    // 准备图像用于检测
    let mut prepared = rqrr::PreparedImage::prepare(gray_image);

    // 搜索二维码
    let grids = prepared.detect_grids();

    // 调试信息
    if !grids.is_empty() {
        println!("[调试] 检测到 {} 个二维码网格", grids.len());
    }

    if let Some(grid) = grids.first() {
        match grid.decode() {
            Ok((_metadata, decoded_string)) => {
                println!("[调试] 成功解码，数据长度: {}", decoded_string.len());
                return Ok(Some(decoded_string));
            }
            Err(e) => {
                println!("[调试] 二维码解码失败: {:?}", e);
            }
        }
    }

    Ok(None)
}

/// 简单的字符串哈希（用于去重）
fn hash_string(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// 重组并还原文件
fn restore_file(chunks: &[String]) -> Result<()> {
    println!("拼接 {} 个数据片段...", chunks.len());

    // 拼接所有数据
    let combined: String = chunks.join("");
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
    println!("║  片段数: {:30} ║", chunks.len());
    println!("╚════════════════════════════════════════╝");

    Ok(())
}

/// 生成输出文件名
fn generate_output_filename() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
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
