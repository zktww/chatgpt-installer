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
/// 用当前 token 的管理员组成员资格判断，不依赖 LanmanServer 服务
///（`net session` 在该服务被禁用时会误判为非管理员）。
#[cfg(windows)]
pub fn is_admin() -> bool {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
        ])
        .creation_flags(NO_WINDOW)
        .output();
    matches!(out, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim().eq_ignore_ascii_case("true"))
}

#[cfg(not(windows))]
pub fn is_admin() -> bool {
    false
}

/// 以管理员身份重启本程序（UAC 弹窗），并透传参数。
/// 每个参数单独加双引号，避免路径含空格被拆散
///（Windows 文件名不允许出现 `"`，无需转义引号本身）。
/// 只负责发起提权，不再无条件退出当前进程：
/// 用户在 UAC 点"否"时本进程继续存活；提权成功后，新进程凭 --parent-pid 结束掉本进程。
#[cfg(windows)]
pub fn restart_elevated(extra_args: &[String]) {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from(""));
    let args = extra_args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "Start-Process -FilePath '{}' -ArgumentList '{}' -Verb RunAs",
        exe.display().to_string().replace('\'', "''"),
        args.replace('\'', "''")
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(NO_WINDOW)
        .spawn();
}
