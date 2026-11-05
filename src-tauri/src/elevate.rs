//! 提权重启。
//!
//! 策略：下载与检测阶段保持普通权限（体验最顺），
//! 仅在安装/卸载需要时请求一次提权（UAC 只打扰用户一次）。
//! 管理员检测见 sys.rs（Win32 API）。

use crate::errors::AppError;

/// 以管理员身份启动本程序的新实例（UAC 弹窗），并透传参数。
///
/// 同步等待 UAC 结果：
/// - `Ok(())`：新进程已启动，调用方应随即退出当前进程，由新进程接管；
/// - `Err(UAC_CANCELLED)`：用户点了「否」或系统拒绝，当前进程继续存活，可重试。
///
/// 每个参数按 Windows 命令行规则引用（见 `ps::win_quote_arg`），
/// 路径含空格、以 `\` 结尾（如 `D:\`）都不会被拆散。
#[cfg(windows)]
pub fn restart_elevated(extra_args: &[String]) -> Result<(), AppError> {
    use crate::ps;
    let exe = std::env::current_exe()
        .map_err(|e| AppError::new("ELEVATE_FAILED", "无法定位程序路径", e.to_string()))?;
    let params = extra_args
        .iter()
        .map(|a| ps::win_quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    // Start-Process 在 UAC 被取消时抛异常：用退出码 1223（ERROR_CANCELLED）回传
    let script = format!(
        "try {{ Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -ErrorAction Stop; exit 0 }} catch {{ exit 1223 }}",
        ps::quote_single(&exe.to_string_lossy()),
        ps::quote_single(&params)
    );
    let out = ps::run(&script)?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(1223) => Err(AppError::uac_cancelled()),
        _ => Err(AppError::new(
            "ELEVATE_FAILED",
            "请求管理员权限失败",
            ps::snippet(&out, 200),
        )),
    }
}

#[cfg(not(windows))]
pub fn restart_elevated(_extra_args: &[String]) -> Result<(), AppError> {
    Err(AppError::new(
        "UNSUPPORTED_OS",
        "提权重启仅支持 Windows",
        "",
    ))
}
