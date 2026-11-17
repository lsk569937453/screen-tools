//! 按进程名/窗口标题定位窗口，并后台截取窗口客户区。
//!
//! 设计要点：
//! - 只匹配**可见**的顶层窗口，且必须有非空标题（排除大量不可见的内部窗口）；
//! - 截取范围取客户区（不含标题栏与边框），坐标系是屏幕的物理像素；
//! - 用 `PrintWindow` 走 DWM 渲染，**窗口在后台被其它窗口遮挡也能截到**；
//! - 每帧重新读一次几何信息，窗口移动/缩放都能跟上；
//! - 匹配优先「非最小化」和「客户区最大」的那个，避免抓到弹窗或工具栏。
//!
//! 目前仅 Windows 实现；其它平台返回明确错误。

use anyhow::Result;

/// 客户区在屏幕坐标系中的位置与大小（物理像素）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Frame {
    /// 是否没有可截图的尺寸（最小化/隐藏时会出现）
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// 面积，用于在多个匹配窗口里挑最大的那个
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn describe(&self) -> String {
        format!("{}x{} @ ({},{})", self.width, self.height, self.x, self.y)
    }
}

/// 一个匹配到的窗口
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// 窗口句柄，转成 isize 保存，避免把平台类型暴露给主流程
    pub handle: isize,
    pub pid: u32,
    /// 进程名（不含 .exe），例如 `HSRClient`
    pub process: String,
    /// 窗口标题，例如 `liushikai-1`
    pub title: String,
    /// 客户区；最小化或隐藏时为 0x0
    pub frame: Frame,
    pub minimized: bool,
    pub visible: bool,
}

impl WindowInfo {
    /// 一行可读描述，日志里直接用
    pub fn describe(&self) -> String {
        format!(
            "进程 {}（pid {}）标题「{}」客户区 {}{}",
            self.process,
            self.pid,
            self.title,
            self.frame.describe(),
            if self.minimized { "（已最小化）" } else { "" }
        )
    }
}

/// 让本进程按「每显示器 DPI 感知」运行。
///
/// 必须在任何窗口坐标/截屏调用之前执行，否则 Windows 会对坐标做虚拟化缩放，
/// 窗口矩形和截图像素就不在同一个坐标系里，截出来的区域会错位。
/// 返回是否成功（失败通常意味着 DPI 感知已被 manifest 或其它方式设置过）。
#[cfg(windows)]
pub fn enable_dpi_awareness() -> bool {
    platform::enable_dpi_awareness()
}

#[cfg(not(windows))]
pub fn enable_dpi_awareness() -> bool {
    false
}

/// 枚举所有「可见 + 有标题」的顶层窗口，按客户区面积从大到小排序
#[cfg(windows)]
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    platform::list_windows()
}

#[cfg(not(windows))]
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    anyhow::bail!("窗口捕获目前只支持 Windows 平台；其它平台请省略 --process/--window 直接抓整屏")
}

/// 按进程名 / 窗口标题（都是大小写不敏感的部分匹配）找窗口；两个条件同时给时为「与」
#[cfg(windows)]
pub fn find(process: Option<&str>, title: Option<&str>) -> Result<WindowInfo> {
    platform::find(process, title)
}

#[cfg(not(windows))]
pub fn find(_process: Option<&str>, _title: Option<&str>) -> Result<WindowInfo> {
    anyhow::bail!("窗口捕获目前只支持 Windows 平台；其它平台请省略 --process/--window 直接抓整屏")
}

/// 重新读取窗口当前状态；窗口已关闭时返回 `None`
#[cfg(windows)]
pub fn locate(handle: isize) -> Option<WindowInfo> {
    platform::locate(handle)
}

#[cfg(not(windows))]
pub fn locate(_handle: isize) -> Option<WindowInfo> {
    None
}

/// 后台截取窗口客户区；窗口被其它窗口遮挡也能拿到画面（不改变窗口层级）
#[cfg(windows)]
pub fn capture_client(handle: isize) -> Result<screenshots::Image> {
    platform::capture_client(handle)
}

#[cfg(not(windows))]
pub fn capture_client(_handle: isize) -> Result<screenshots::Image> {
    anyhow::bail!("窗口后台截图目前只支持 Windows 平台；其它平台请省略 --process/--window 直接抓整屏")
}

#[cfg(windows)]
mod platform {
    use super::{Frame, WindowInfo};
    use anyhow::Result;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, BOOL, HANDLE, HWND, LPARAM, POINT, RECT, TRUE,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsIconic, IsWindow, IsWindowVisible,
    };

    /// 无标题的窗口不参与匹配，太多了（输入法、托盘、隐藏宿主等）
    const MAX_TITLE_LEN: usize = 512;

    /// `PrintWindow` 的客户端区域标志
    const PW_CLIENTONLY: u32 = 0x0000_0001;
    /// 让 DWM 把真实内容渲染出来；少了它，Chrome 等 GPU 窗口只会得到黑屏
    const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

    pub fn enable_dpi_awareness() -> bool {
        // PER_MONITOR_AWARE_V2：Win10 1703+。失败说明已经被设置过，同样算可用
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 }
    }

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        let mut windows: Vec<WindowInfo> = Vec::new();

        let ok = unsafe {
            EnumWindows(
                Some(enum_windows_cb),
                &mut windows as *mut Vec<WindowInfo> as LPARAM,
            )
        };
        if ok == 0 {
            anyhow::bail!("EnumWindows 调用失败");
        }

        windows.sort_by_key(|window| std::cmp::Reverse(window.frame.area()));
        Ok(windows)
    }

    pub fn find(process: Option<&str>, title: Option<&str>) -> Result<WindowInfo> {
        let want_process = process
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty());
        let want_title = title
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty());

        if want_process.is_none() && want_title.is_none() {
            anyhow::bail!("--process / --window 至少要给一个");
        }

        let mut matches: Vec<WindowInfo> = list_windows()?
            .into_iter()
            .filter(|window| {
                if let Some(needle) = &want_process {
                    if !window.process.to_lowercase().contains(needle) {
                        return false;
                    }
                }
                if let Some(needle) = &want_title {
                    if !window.title.to_lowercase().contains(needle) {
                        return false;
                    }
                }
                true
            })
            .collect();

        if matches.is_empty() {
            anyhow::bail!("没有找到符合条件的窗口");
        }

        // 非最小化的优先；同样状态时客户区大的优先（主窗口通常最大）
        matches.sort_by_key(|window| (window.minimized, std::cmp::Reverse(window.frame.area())));
        Ok(matches.remove(0))
    }

    pub fn locate(handle: isize) -> Option<WindowInfo> {
        let hwnd = handle as HWND;
        unsafe {
            if IsWindow(hwnd) == 0 {
                return None;
            }

            let minimized = IsIconic(hwnd) != 0;
            let visible = IsWindowVisible(hwnd) != 0;
            // 最小化时 GetClientRect 仍返回原尺寸，但 ClientToScreen 会给出 -32000 之类的
            // 最小化坐标，用来截图只会拿到桌面图标，所以这里直接给空范围
            let frame = if minimized || !visible {
                Frame::default()
            } else {
                client_frame(hwnd).unwrap_or_default()
            };

            let pid = window_pid(hwnd);
            Some(WindowInfo {
                handle,
                pid,
                process: process_name(pid),
                title: window_title(hwnd),
                frame,
                minimized,
                visible,
            })
        }
    }

    /// 后台截取窗口客户区
    ///
    /// 走 `PrintWindow(PW_CLIENTONLY | PW_RENDERFULLCONTENT)`：DWM 会把窗口真实内容
    /// 渲染到内存 DC，因此窗口被其它窗口完全遮挡时依然能拿到画面，且不会改变窗口层级。
    pub fn capture_client(handle: isize) -> Result<screenshots::Image> {
        let hwnd = handle as HWND;
        unsafe {
            if IsWindow(hwnd) == 0 {
                anyhow::bail!("目标窗口已关闭");
            }
            if IsIconic(hwnd) != 0 {
                anyhow::bail!("目标窗口已最小化；后台截图要求窗口处于非最小化状态");
            }

            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetClientRect(hwnd, &mut rect) == 0 {
                anyhow::bail!("读取窗口客户区尺寸失败");
            }
            let width = (rect.right - rect.left).max(0) as u32;
            let height = (rect.bottom - rect.top).max(0) as u32;
            if width == 0 || height == 0 {
                anyhow::bail!("窗口客户区尺寸为 0（窗口可能被隐藏）");
            }

            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                anyhow::bail!("获取屏幕 DC 失败");
            }
            let mem_dc = CreateCompatibleDC(screen_dc);
            if mem_dc.is_null() {
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                anyhow::bail!("创建内存 DC 失败");
            }
            let bitmap = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
            if bitmap.is_null() {
                DeleteDC(mem_dc);
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                anyhow::bail!("创建位图失败");
            }
            let previous = SelectObject(mem_dc, bitmap);

            let printed = PrintWindow(hwnd, mem_dc, PW_CLIENTONLY | PW_RENDERFULLCONTENT);

            // 32 位 BGRA、自上而下（biHeight 取负数），与 DIB 内存布局一致
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width as i32;
            info.bmiHeader.biHeight = -(height as i32);
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;

            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let lines = GetDIBits(
                mem_dc,
                bitmap,
                0,
                height,
                pixels.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            );

            SelectObject(mem_dc, previous);
            DeleteObject(bitmap);
            DeleteDC(mem_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);

            if printed == 0 {
                anyhow::bail!("PrintWindow 失败（窗口可能使用了不支持的渲染方式）");
            }
            if lines == 0 {
                anyhow::bail!("GetDIBits 读取像素失败");
            }

            // GPU 渲染的窗口偶尔只会返回全黑，这里直接给出明确提示
            let (sampled_pixels, _) = pixels.as_chunks::<4>();
            let non_black = sampled_pixels
                .iter()
                .step_by(97)
                .filter(|pixel| pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32 > 30)
                .count();
            if non_black == 0 {
                crate::detail(
                    "警告",
                    format!(
                        "窗口后台截图为全黑（{}x{}）：该窗口可能使用 GPU 独占渲染，后台抓不到画面",
                        width, height
                    ),
                );
            }

            Ok(screenshots::Image::from_bgra(
                pixels,
                width,
                height,
                width as usize * 4,
            ))
        }
    }

    /// EnumWindows 的回调：把符合条件的窗口收集进 `lparam` 指向的 Vec
    unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam as *mut Vec<WindowInfo>);

        if IsWindowVisible(hwnd) == 0 {
            return TRUE;
        }

        let title = window_title(hwnd);
        if title.is_empty() {
            return TRUE;
        }

        let pid = window_pid(hwnd);
        let minimized = IsIconic(hwnd) != 0;
        let frame = if minimized {
            Frame::default()
        } else {
            client_frame(hwnd).unwrap_or_default()
        };

        out.push(WindowInfo {
            handle: hwnd as isize,
            pid,
            process: process_name(pid),
            title,
            frame,
            minimized,
            visible: true,
        });

        TRUE
    }

    unsafe fn window_pid(hwnd: HWND) -> u32 {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid
    }

    unsafe fn window_title(hwnd: HWND) -> String {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }

        let len = (len as usize).min(MAX_TITLE_LEN);
        let mut buffer = vec![0u16; len + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        if copied <= 0 {
            return String::new();
        }

        OsString::from_wide(&buffer[..copied as usize])
            .to_string_lossy()
            .into_owned()
    }

    /// 进程可执行文件名（去掉目录和 `.exe`），例如 `HSRClient`
    unsafe fn process_name(pid: u32) -> String {
        if pid == 0 {
            return String::new();
        }

        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return String::new();
        }

        let mut buffer = vec![0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);

        if ok == 0 || size == 0 {
            return String::new();
        }

        let path = OsString::from_wide(&buffer[..size as usize])
            .to_string_lossy()
            .into_owned();
        let file_name = path.rsplit(['\\', '/']).next().unwrap_or(&path);

        // 去掉扩展名；顺便把 ".exe" 之外的形式（大写等）也覆盖掉
        match file_name.rsplit_once('.') {
            Some((stem, _extension)) if !stem.is_empty() => stem.to_string(),
            _ => file_name.to_string(),
        }
    }

    /// 客户区在屏幕坐标系中的矩形（物理像素）
    unsafe fn client_frame(hwnd: HWND) -> Option<Frame> {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetClientRect(hwnd, &mut rect) == 0 {
            return None;
        }

        let width = (rect.right - rect.left).max(0) as u32;
        let height = (rect.bottom - rect.top).max(0) as u32;
        if width == 0 || height == 0 {
            return None;
        }

        let mut origin = POINT { x: 0, y: 0 };
        if ClientToScreen(hwnd, &mut origin) == 0 {
            return None;
        }

        Some(Frame {
            x: origin.x,
            y: origin.y,
            width,
            height,
        })
    }
}
