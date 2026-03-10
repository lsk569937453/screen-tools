use anyhow::{anyhow, Result};
use base64::Engine;
use image::GrayImage;
use screenshots::Screen;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const CAPTURE_INTERVAL_MS: u64 = 50; // 截屏间隔（毫秒）
const IDLE_TIMEOUT_SECONDS: u64 = 3; // 无新数据超时时间（秒）
const SAVE_FILE: &str = ".qr_session.json"; // 会话保存文件

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
fn main() {
    let args: Vec<String> = std::env::args().collect();

    if let Err(e) = main_with_error(&args) {
        println!("\n错误: {}", e);
        std::process::exit(1);
    }
}

fn main_with_error(args: &[String]) -> Result<()> {
    // 解析命令行参数
    let is_resume = args.iter().any(|a| a == "--resume");
    let is_clean = args.iter().any(|a| a == "--clean");

    // 清除保存的会话
    if is_clean {
        if PathBuf::from(SAVE_FILE).exists() {
            fs::remove_file(SAVE_FILE)?;
            println!("已清除保存的会话");
        }
        return Ok(());
    }

    println!("╔════════════════════════════════════════╗");
    println!("║       QR 码文件接收器                  ║");
    println!("╚════════════════════════════════════════╝");
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
    let mut loop_count = 0;

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
        match capture_and_detect_qr(&screen) {
            Ok(Some(raw_data)) => {
                // 解析序号信息: "序号/总数|数据"
                match parse_chunk_data(&raw_data) {
                    Ok(chunk) => {
                        let seq = chunk.seq;
                        let total = chunk.total;
                        let data_len = chunk.data.len();

                        // 更新总数信息
                        if expected_total.is_none() {
                            expected_total = Some(total);
                            println!("[信息] 总片段数: {}", total);
                        }

                        // 检查序号是否一致
                        if expected_total != Some(total) {
                            println!(
                                "[警告] 片段 #{} 的总数 ({}) 与预期 ({}) 不符",
                                seq,
                                total,
                                expected_total.unwrap_or(0)
                            );
                        }

                        // 去重：只保存新的数据块
                        if !received_chunks.contains_key(&seq) {
                            received_chunks.insert(seq, chunk);
                            last_data_time = Instant::now();

                            println!(
                                "[收到] 片段 #{:>3} | 数据大小: {} bytes | 已收集: {}/{}",
                                seq,
                                data_len,
                                received_chunks.len(),
                                expected_total.unwrap_or(0)
                            );
                        }
                    }
                    Err(e) => {
                        println!("[警告] 解析片段数据失败: {} (原始数据: {}...)", e, &raw_data[..raw_data.len().min(30)]);
                    }
                }
            }
            Ok(None) => {
                // 未检测到二维码，继续
            }
            Err(e) => {
                println!("[错误] {}", e);
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
                println!("数据传输超时，已收集 {}/{} 片段", received_chunks.len(), total);

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

        thread::sleep(Duration::from_millis(CAPTURE_INTERVAL_MS));
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

/// 保存会话到文件
fn save_session(chunks: &HashMap<usize, Chunk>, expected_total: Option<usize>) -> Result<()> {
    let session = Session {
        chunks: chunks.clone(),
        expected_total,
    };

    let json = serde_json::to_string_pretty(&session)?;
    let mut file = File::create(SAVE_FILE)?;
    file.write_all(json.as_bytes())?;

    Ok(())
}

/// 从文件加载会话
fn load_session() -> Result<Session> {
    let mut file = File::open(SAVE_FILE)?;
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
