//! 权限：检测管理员身份 / 提权重启。
//!
//! 策略：下载与检测阶段保持普通权限（体验最顺），
//! 仅在安装/卸载瞬间请求一次提权（UAC 只打扰用户一次）。
//!
//! 所有子进程调用都加 CREATE_NO_WINDOW，避免 GUI 程序闪黑窗口。

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Windows 创建进程标志：不创建控制台窗口。
#[cfg(windows)]
const NO_WINDOW: u32 = 0x08000000;

/// 是否已具备管理员权限。
/// `net session` 是经典技巧：成功返回 0 仅当提权后。
#[cfg(windows)]
pub fn is_admin() -> bool {
    std::process::Command::new("net")
        .arg("session")
        .creation_flags(NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn is_admin() -> bool {
    false
}

/// 以管理员身份重启本程序（UAC 弹窗），并透传参数。
/// 成功时当前进程直接退出，不返回。
#[cfg(windows)]
pub fn restart_elevated(extra_args: &[&str]) -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from(""));
    let args = extra_args.join(" ");
    let script = format!(
        "Start-Process -FilePath '{}' -ArgumentList '{}' -Verb RunAs",
        exe.display().to_string().replace('\'', "''"),
        args.replace('\'', "''")
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(NO_WINDOW)
        .spawn();
    // 无论提权是否成功，当前实例都退出，避免出现两个实例
    std::process::exit(0);
}
