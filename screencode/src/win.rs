//! 按进程名/窗口标题定位窗口，并后台截取窗口客户区（Windows）。
//!
//! 设计要点：
//! - 只匹配**可见**的顶层窗口，且必须有非空标题（排除大量不可见的内部窗口）；
//! - 截取范围取客户区（不含标题栏与边框），坐标系是屏幕的物理像素；
//! - 用 `PrintWindow` 走 DWM 渲染，**窗口在后台被其它窗口遮挡也能截到**；
//! - 每帧重新读一次几何信息，窗口移动/缩放都能跟上；
//! - 匹配优先「非最小化」和「客户区最大」的那个，避免抓到弹窗或工具栏。

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
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn describe(&self) -> String {
        format!("{}x{} @ ({},{})", self.width, self.height, self.x, self.y)
    }
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub handle: isize,
    pub pid: u32,
    pub process: String,
    pub title: String,
    pub frame: Frame,
    pub minimized: bool,
    pub visible: bool,
}

impl WindowInfo {
    pub fn describe(&self) -> String {
        format!(
            "进程 {}（pid {}）标题「{}」客户区 {}{}",
            self.process,
            self.pid,
            self.title,
            self.frame.describe(),
            if self.minimized {
                "（已最小化）"
            } else {
                ""
            }
        )
    }
}

pub fn enable_dpi_awareness() -> bool {
    platform::enable_dpi_awareness()
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    platform::list_windows()
}

pub fn find(process: Option<&str>, title: Option<&str>) -> Result<WindowInfo> {
    platform::find(process, title)
}

pub fn locate(handle: isize) -> Option<WindowInfo> {
    platform::locate(handle)
}

pub fn capture_client(handle: isize) -> Result<screenshots::Image> {
    platform::capture_client(handle)
}

/// 采样画面里非纯黑像素的比例（后台抓 GPU 窗口偶尔只能拿到全黑）
pub fn non_black_ratio(image: &screenshots::Image) -> f64 {
    let rgba = image.rgba();
    let (pixels, _) = rgba.as_chunks::<4>();
    let mut non_black = 0usize;
    let mut total = 0usize;
    for p in pixels.iter().step_by(97) {
        if p[0] as u32 + p[1] as u32 + p[2] as u32 > 30 {
            non_black += 1;
        }
        total += 1;
    }
    if total == 0 {
        0.0
    } else {
        non_black as f64 / total as f64
    }
}

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

    const MAX_TITLE_LEN: usize = 512;
    const PW_CLIENTONLY: u32 = 0x0000_0001;
    const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

    pub fn enable_dpi_awareness() -> bool {
        // SAFETY: 只设置当前进程的 DPI 感知级别，不传入指针，也不保存任何返回值。
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 }
    }

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        let mut windows: Vec<WindowInfo> = Vec::new();
        // SAFETY: `windows` 在整个调用期间存活；回调把 `LPARAM` 还原成同一个 Vec，
        // 只在该 Vec 上 push，不把借用带出回调。
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

        matches.sort_by_key(|window| (window.minimized, std::cmp::Reverse(window.frame.area())));
        Ok(matches.remove(0))
    }

    pub fn locate(handle: isize) -> Option<WindowInfo> {
        let hwnd = handle as HWND;
        // SAFETY: `hwnd` 来自先前枚举到的窗口句柄；下面全是只读的窗口状态查询，
        // 输出缓冲区都在栈上，且每个 API 的失败都有显式分支。
        unsafe {
            if IsWindow(hwnd) == 0 {
                return None;
            }
            let minimized = IsIconic(hwnd) != 0;
            let visible = IsWindowVisible(hwnd) != 0;
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

    pub fn capture_client(handle: isize) -> Result<screenshots::Image> {
        let hwnd = handle as HWND;
        // SAFETY: `hwnd` 是先前枚举到的窗口句柄；`RECT`/像素缓冲区在栈或本函数内分配，
        // PrintWindow 只读取窗口内容并写入该缓冲区，失败路径立即返回错误。
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

            Ok(screenshots::Image::from_bgra(
                pixels,
                width,
                height,
                width as usize * 4,
            ))
        }
    }

    /// # Safety
    ///
    /// 只由 `EnumWindows` 调用；`lparam` 必须是 `list_windows` 传入的
    /// `*mut Vec<WindowInfo>`，并在回调期间保持有效。
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

    /// # Safety
    ///
    /// `hwnd` 必须是有效窗口句柄；无效句柄只会让 `GetWindowThreadProcessId` 写入 0。
    unsafe fn window_pid(hwnd: HWND) -> u32 {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid
    }

    /// # Safety
    ///
    /// `hwnd` 必须是有效窗口句柄；长度来自 `GetWindowTextLengthW`，
    /// 缓冲区按该长度分配，并用返回值截断。
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

    /// # Safety
    ///
    /// `pid` 可以是任意进程号：权限不足或进程不存在时 `OpenProcess` 返回空句柄，
    /// 本函数会提前返回空字符串，不会解引用空句柄。
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
        match file_name.rsplit_once('.') {
            Some((stem, _extension)) if !stem.is_empty() => stem.to_string(),
            _ => file_name.to_string(),
        }
    }

    /// # Safety
    ///
    /// `hwnd` 必须是有效窗口句柄；客户区尺寸通过 `GetClientRect` 获取，
    /// 分配失败或尺寸为 0 时返回 `None`。
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
