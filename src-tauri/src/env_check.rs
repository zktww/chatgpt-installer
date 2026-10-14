//! 环境检测：系统版本、架构、磁盘空间、网络连通性、管理员权限。
use serde::Serialize;
use tauri::AppHandle;

use crate::constants::*;
use crate::elevate::is_admin;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct EnvItem {
    pub id: &'static str,
    pub title: &'static str,
    pub detail: String,
    /// ok | warn | fail
    pub status: &'static str,
}

fn item(id: &'static str, title: &'static str, detail: String, status: &'static str) -> EnvItem {
    EnvItem {
        id,
        title,
        detail,
        status,
    }
}

/// 依次执行全部检查项。任一 fail 都应阻止进入下一步。
pub fn check_env(app: &AppHandle) -> Vec<EnvItem> {
    vec![
        check_os(),
        check_arch(),
        check_disk(app),
        check_network(),
        check_admin(),
    ]
}

fn check_os() -> EnvItem {
    #[cfg(windows)]
    {
        let build: u32 = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
            .and_then(|k| k.get_value::<String, _>("CurrentBuildNumber"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if build == 0 {
            return item("os", "操作系统", "无法读取系统版本号".into(), "fail");
        }
        let ok = build >= MIN_BUILD_NUMBER;
        item(
            "os",
            "操作系统",
            format!(
                "Windows Build {}（需要 ≥ {}）{}",
                build,
                MIN_BUILD_NUMBER,
                if ok { "" } else { "，版本过低" }
            ),
            if ok { "ok" } else { "fail" },
        )
    }
    #[cfg(not(windows))]
    {
        item("os", "操作系统", "此安装器仅支持 Windows".into(), "fail")
    }
}

fn check_arch() -> EnvItem {
    let arch = std::env::consts::ARCH;
    match msix_filename_for_arch(arch) {
        Some(f) => item(
            "arch",
            "CPU 架构",
            format!("{}，已自动匹配安装包 {}", arch, f),
            "ok",
        ),
        None => item(
            "arch",
            "CPU 架构",
            format!("{} 不受支持（仅支持 x64 / ARM64）", arch),
            "fail",
        ),
    }
}

fn check_disk(app: &AppHandle) -> EnvItem {
    #[cfg(windows)]
    {
        // 检查实际下载目录所在的盘，而非只查系统盘（用户可把下载目录自定义到 D 盘）
        let dir = crate::cmds::pkg_dir(app)
            .unwrap_or_else(|_| std::path::PathBuf::from("C:\\"));
        let drive = drive_of(&dir).unwrap_or_else(|| "C:".to_string());
        // 用 PowerShell 读磁盘可用空间，避免引入额外依赖
        let script = format!(
            "[math]::Round((Get-CimInstance Win32_LogicalDisk -Filter \"DeviceID='{}'\").FreeSpace / 1GB, 1)",
            drive
        );
        match run_ps_quick(&script) {
            Ok(out) => {
                let gb: f64 = out.trim().parse().unwrap_or(0.0);
                let ok = (gb * 1024.0 * 1024.0 * 1024.0) as u64 >= REQUIRED_DISK_BYTES;
                item(
                    "disk",
                    "磁盘空间",
                    format!("{} 盘可用 {:.1} GB（需要 ≥ 3 GB）", drive, gb),
                    if ok { "ok" } else { "fail" },
                )
            }
            Err(_) => item(
                "disk",
                "磁盘空间",
                "无法读取磁盘空间，请确保至少有 3 GB 可用".into(),
                "warn",
            ),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        item(
            "disk",
            "磁盘空间",
            "非 Windows 环境，跳过检测".into(),
            "warn",
        )
    }
}

/// 取路径所在的盘符（如 `D:`）；非盘符路径返回 None。
#[cfg(windows)]
fn drive_of(path: &Path) -> Option<String> {
    match path.components().next() {
        Some(std::path::Component::Prefix(p)) => match p.kind() {
            std::path::Prefix::Disk(d) => Some(format!("{}:", d as char)),
            _ => None,
        },
        _ => None,
    }
}

fn check_network() -> EnvItem {
    let arch = std::env::consts::ARCH;
    let url = match msix_url_for_arch(arch) {
        Some(u) => u,
        None => {
            return item(
                "net",
                "网络连通",
                "架构不受支持，无法确定下载地址".into(),
                "fail",
            )
        }
    };
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return item(
                "net",
                "网络连通",
                format!("网络组件初始化失败：{}", e),
                "fail",
            )
        }
    };
    let start = std::time::Instant::now();
    match client.head(url).send() {
        Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 405 => {
            // 部分 CDN 对 HEAD 返回 405，视为可达
            item(
                "net",
                "网络连通",
                format!("官方 CDN 可达，延迟约 {}ms", start.elapsed().as_millis()),
                "ok",
            )
        }
        Ok(resp) => item(
            "net",
            "网络连通",
            format!("官方 CDN 返回异常状态码 {}", resp.status()),
            "fail",
        ),
        Err(e) => item(
            "net",
            "网络连通",
            format!("无法连接官方 CDN：{}", e),
            "fail",
        ),
    }
}

fn check_admin() -> EnvItem {
    if is_admin() {
        item("admin", "管理员权限", "当前已具备管理员权限".into(), "ok")
    } else {
        item(
            "admin",
            "管理员权限",
            "当前为普通权限，安装步骤开始时将请求一次提权".into(),
            "warn",
        )
    }
}

#[cfg(windows)]
fn run_ps_quick(script: &str) -> Result<String, ()> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：不闪黑窗口
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
