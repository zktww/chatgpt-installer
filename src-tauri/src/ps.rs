//! PowerShell 调用与命令行引用工具（统一入口，替代各模块里重复的 run_ps）。
//!
//! 仅 AppX 部署 / 签名校验 / 提权这类没有轻量原生替代的操作才走 PowerShell；
//! 管理员检测、磁盘空间、CPU 架构已改用 Win32 API（见 sys.rs）。

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::errors::AppError;

/// Windows 创建进程标志：不创建控制台窗口（GUI 程序不闪黑窗口）。
#[cfg(windows)]
pub const NO_WINDOW: u32 = 0x08000000;

/// 执行一段 PowerShell 脚本，返回完整输出（含退出码）。
#[cfg(windows)]
pub fn run(script: &str) -> Result<std::process::Output, AppError> {
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(NO_WINDOW)
        .output()
        .map_err(|e| {
            AppError::new(
                "PS_FAILED",
                "无法调用 PowerShell",
                format!("请确认系统 PowerShell 可用：{}", e),
            )
        })
}

#[cfg(not(windows))]
pub fn run(_script: &str) -> Result<std::process::Output, AppError> {
    Err(AppError::new("UNSUPPORTED_OS", "仅支持 Windows", ""))
}

/// 执行脚本并返回去空白的 stdout（不检查退出码）。
pub fn text(script: &str) -> Result<String, AppError> {
    let out = run(script)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 截取 stderr（优先）或 stdout 的前 n 个字符，用于警告信息。
pub fn snippet(out: &std::process::Output, n: usize) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let src = if err.trim().is_empty() {
        String::from_utf8_lossy(&out.stdout).to_string()
    } else {
        err.to_string()
    };
    src.trim().chars().take(n).collect()
}

/// PowerShell 双引号字面路径：转义 ` " $ 及通配符 [ ] * ?，确保按字面路径处理。
/// （单引号字符串里反引号无转义作用，故用双引号；
/// Windows 文件名本身不允许出现 * ? "，实际要防的是 [ ]。）
pub fn quote_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '[' | ']' | '*' | '?') {
            out.push('`');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// PowerShell 单引号字符串：只需把 ' 写成 ''。
pub fn quote_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 按 Windows 命令行解析规则（CommandLineToArgvW）引用单个参数：
/// 引号前的反斜杠要加倍再转义引号，结尾的反斜杠也要加倍——
/// 否则 `"--pkg-dir=D:\"` 里的 `\"` 会被当成转义引号，后续参数全部粘连。
pub fn win_quote_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_quote_plain_and_spaces() {
        assert_eq!(win_quote_arg("--resume=install"), "\"--resume=install\"");
        assert_eq!(
            win_quote_arg("--pkg-dir=D:\\My Downloads"),
            "\"--pkg-dir=D:\\My Downloads\""
        );
    }

    #[test]
    fn win_quote_trailing_backslash() {
        // 盘符根目录：结尾反斜杠必须加倍
        assert_eq!(win_quote_arg("--pkg-dir=D:\\"), "\"--pkg-dir=D:\\\\\"");
    }

    #[test]
    fn win_quote_embedded_quote() {
        assert_eq!(win_quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn quote_literal_escapes_wildcards() {
        assert_eq!(quote_literal("C:\\a[1]\\b$"), "\"C:\\a`[1`]\\b`$\"");
    }

    #[test]
    fn quote_single_doubles_quote() {
        assert_eq!(quote_single("C:\\it's"), "'C:\\it''s'");
    }
}
