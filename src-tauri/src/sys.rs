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
