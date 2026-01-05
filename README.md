# Screen Tools - QR Code File Transfer

基于二维码的文件传输工具，适用于需要在不同设备间通过屏幕扫码传输文件的场景。

## 工作原理

1. **发送端 (Client)**：将文件切片并转换为 Base64，生成多个二维码 SVG 文件
2. **展示**：通过 HTTP 服务轮播展示二维码
3. **接收端 (Server)**：持续截屏识别二维码，收集并重组还原文件

## 编译项目

```bash
# 编译 client
cd client
cargo build --release

# 编译 server
cd server
cargo build --release
```

## 使用方法

### 发送端 (Client)

#### 1. 生成二维码

```bash
cd client
cargo run --release -- <文件路径>
```

示例：
```bash
cargo run --release -- ./test.txt
```

该命令会：
- 读取指定文件
- 将文件内容转换为 Base64
- 按 2000 字符切片
- 生成二维码 SVG 文件到 `qr_output/` 目录
- 生成网页文件到 `web/` 目录

#### 2. 启动 HTTP 服务

```bash
cd client
cargo run --release
```

服务启动后访问：`http://localhost:9090`

在浏览器中打开页面，点击播放按钮即可开始轮播二维码。

### 接收端 (Server)

```bash
cd server
cargo run --release
```

程序启动后会：
- 每 50ms 截屏一次
- 自动识别屏幕上的二维码
- 收集所有数据片段
- 当 3 秒内无新数据时，自动重组并保存文件

接收的文件保存在当前目录下，命名为 `received_file_<时间戳>`

## 典型使用场景

1. 在发送设备上运行 `client` 生成二维码
2. 启动 `client` 的 HTTP 服务，浏览器打开轮播页面
3. 在接收设备上运行 `server`
4. 将轮播页面展示给接收设备的摄像头
5. 等待传输完成，接收文件自动保存

## 技术特点

- **Client**：
  - 文件切片处理，支持大文件传输
  - Base64 编码确保数据完整性
  - HTTP 服务实现二维码轮播
  - SVG 格式二维码，清晰可缩放

- **Server**：
  - 高频截屏 (50ms 间隔)
  - 去重机制，避免重复接收
  - 超时检测，自动判断传输结束
