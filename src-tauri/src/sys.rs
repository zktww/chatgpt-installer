//! 轻量系统信息：直接调 Win32 API，避免 PowerShell 冷启动开销。

/// 当前进程是否以管理员身份（提升后的 token）运行。
/// UAC 下未提升的管理员账号返回 false，与「是否需要提权」的语义一致。
#[cfg(windows)]
pub fn is_admin() -> bool {
    // SAFETY: 无参数的只读查询
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
pub fn is_admin() -> bool {
    false
}

/// 本机**原生** CPU 架构（"x86_64" / "aarch64"）。
///
/// 不能用 `std::env::consts::ARCH`：那是编译期常量。CI 只出 x64 版本，
/// 在 ARM64 设备上靠系统模拟运行时也会报 x86_64，导致下载错误的安装包。
pub fn native_arch() -> &'static str {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
        let mut process_machine: u16 = 0;
        let mut native_machine: u16 = 0;
        // SAFETY: 传入当前进程伪句柄与两个有效的输出指针
        let ok = unsafe {
            IsWow64Process2(
                GetCurrentProcess(),
                &mut process_machine,
                &mut native_machine,
            )
        };
        if ok != 0 {
            match native_machine {
                0xAA64 => return "aarch64", // IMAGE_FILE_MACHINE_ARM64
                0x8664 => return "x86_64",  // IMAGE_FILE_MACHINE_AMD64
                _ => {}
            }
        }
        // 兜底：注册表里的系统环境变量是原生架构（不受进程模拟影响）
        if let Ok(v) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment")
            .and_then(|k| k.get_value::<String, _>("PROCESSOR_ARCHITECTURE"))
        {
            match v.trim().to_ascii_uppercase().as_str() {
                "ARM64" => return "aarch64",
                "AMD64" => return "x86_64",
                _ => {}
            }
        }
    }
    std::env::consts::ARCH
}

/// 目录所在卷对当前用户可用的剩余字节数；目录需已存在。
#[cfg(windows)]
pub fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free: u64 = 0;
    // SAFETY: wide 以 NUL 结尾；不需要的输出参数传空指针（API 允许）
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
pub fn free_bytes(_dir: &std::path::Path) -> Option<u64> {
    None
}

/// 显式为主窗口绑定 Win32 大图标（`ICON_BIG`，用于 Windows 11 任务栏与 `Alt+Tab`）
/// 与小图标（`ICON_SMALL`，用于窗口标题栏），并同步写入窗口类 `GCLP_HICON` / `GCLP_HICONSM`。
///
/// 原因：Tauri / `tao` 在 Windows 上创建窗口时仅设置 `ICON_SMALL`，并在 `on_create` 中将
/// `ICON_BIG` 清零（`WM_SETICON(ICON_BIG, 0)`）且窗口类 `hIcon` 为 NULL，导致独立运行的
/// `.exe` 在部分 Windows 11 环境下任务栏退化为系统默认空窗口图标。
#[cfg(windows)]
pub fn apply_window_icon(window: &tauri::WebviewWindow) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, SetClassLongPtrW, GCLP_HICON, GCLP_HICONSM,
        ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED, SM_CXICON, SM_CXSMICON,
        SM_CYICON, SM_CYSMICON, WM_SETICON,
    };

    let Ok(hwnd) = window.hwnd() else { return };
    let hwnd_raw = hwnd.0;
    if hwnd_raw.is_null() {
        return;
    }

    // tauri-build 将 icons/icon.ico 以资源 ID 32512 (IDI_APPLICATION) 嵌入 PE .rsrc
    const APP_ICON_RES_ID: usize = 32512;

    // SAFETY: GetModuleHandleW(NULL) 获取当前进程模块句柄；LoadImageW 带 LR_SHARED 由系统托管生命周期；
    // SendMessageW / SetClassLongPtrW 针对有效主窗口句柄设置大小图标。
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let res_name = APP_ICON_RES_ID as *const u16;

        let big_w = GetSystemMetrics(SM_CXICON).max(32);
        let big_h = GetSystemMetrics(SM_CYICON).max(32);
        let small_w = GetSystemMetrics(SM_CXSMICON).max(16);
        let small_h = GetSystemMetrics(SM_CYSMICON).max(16);

        let hicon_big = LoadImageW(
            hinst,
            res_name,
            IMAGE_ICON,
            big_w,
            big_h,
            LR_DEFAULTCOLOR | LR_SHARED,
        );
        let hicon_small = LoadImageW(
            hinst,
            res_name,
            IMAGE_ICON,
            small_w,
            small_h,
            LR_DEFAULTCOLOR | LR_SHARED,
        );

        if !hicon_big.is_null() {
            SendMessageW(hwnd_raw, WM_SETICON, ICON_BIG as usize, hicon_big as isize);
            SetClassLongPtrW(hwnd_raw, GCLP_HICON, hicon_big as isize);
        }
        if !hicon_small.is_null() {
            SendMessageW(
                hwnd_raw,
                WM_SETICON,
                ICON_SMALL as usize,
                hicon_small as isize,
            );
            SetClassLongPtrW(hwnd_raw, GCLP_HICONSM, hicon_small as isize);
        }
    }
}

#[cfg(not(windows))]
pub fn apply_window_icon(_window: &tauri::WebviewWindow) {}
