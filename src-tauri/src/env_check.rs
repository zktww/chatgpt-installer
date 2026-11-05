//! 环境检测：系统版本、架构、磁盘空间、网络连通性、管理员权限。
use serde::Serialize;
use tauri::AppHandle;

use crate::constants::*;
use crate::sys::{is_admin, native_arch};

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

/// 执行全部检查项。任一 fail 都应阻止进入下一步。
/// 网络检测最慢（最多 15 秒），放到独立线程与本地检测并行。
pub fn check_env(app: &AppHandle) -> Vec<EnvItem> {
    std::thread::scope(|s| {
        let net = s.spawn(check_network);
        let local = vec![check_os(), check_arch(), check_disk(app)];
        let net = net
            .join()
            .unwrap_or_else(|_| item("net", "网络连通", "网络检测异常退出".into(), "fail"));
        let mut all = local;
        all.push(net);
        all.push(check_admin());
        all
    })
}

fn check_os() -> EnvItem {
    #[cfg(windows)]
    {
        let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
        let build: u32 = key
            .as_ref()
            .ok()
            .and_then(|k| k.get_value::<String, _>("CurrentBuildNumber").ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if build == 0 {
            return item("os", "操作系统", "无法读取系统版本号".into(), "fail");
        }
        // Win11 的 ProductName 仍写着 "Windows 10"，按 Build 号判断更准
        let name = if build >= 22000 {
            "Windows 11"
        } else {
            "Windows 10"
        };
        let display = key
            .as_ref()
            .ok()
            .and_then(|k| k.get_value::<String, _>("DisplayVersion").ok())
            .map(|v| format!(" {}", v.trim()))
            .unwrap_or_default();
        let ok = build >= MIN_BUILD_NUMBER;
        item(
            "os",
            "操作系统",
            format!(
                "{}{}（Build {}）{}",
                name,
                display,
                build,
                if ok {
                    "，满足要求".to_string()
                } else {
                    format!("，版本过低，需要 Build {} 及以上", MIN_BUILD_NUMBER)
                }
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
    let arch = native_arch();
    let label = match arch {
        "x86_64" => "x64",
        "aarch64" => "ARM64",
        other => other,
    };
    match msix_filename_for_arch(arch) {
        Some(f) => item(
            "arch",
            "CPU 架构",
            format!("{}，已自动匹配安装包 {}", label, f),
            "ok",
        ),
        None => item(
            "arch",
            "CPU 架构",
            format!("{} 不受支持（仅支持 x64 / ARM64）", label),
            "fail",
        ),
    }
}

fn check_disk(app: &AppHandle) -> EnvItem {
    // 检查实际下载目录所在的盘，而非只查系统盘（用户可把下载目录自定义到 D 盘）
    let dir = match crate::cmds::pkg_dir(app) {
        Ok(d) => d,
        Err(e) => {
            return item(
                "disk",
                "磁盘空间",
                format!("无法确定下载目录：{}", e.message),
                "warn",
            )
        }
    };
    let need_gb = REQUIRED_DISK_BYTES as f64 / 1024f64.powi(3);
    match crate::sys::free_bytes(&dir) {
        Some(free) => {
            let gb = free as f64 / 1024f64.powi(3);
            let ok = free >= REQUIRED_DISK_BYTES;
            item(
                "disk",
                "磁盘空间",
                format!(
                    "{} 可用 {:.1} GB（需要 ≥ {:.0} GB）",
                    drive_label(&dir),
                    gb,
                    need_gb
                ),
                if ok { "ok" } else { "fail" },
            )
        }
        None => item(
            "disk",
            "磁盘空间",
            format!("无法读取磁盘空间，请确保至少有 {:.0} GB 可用", need_gb),
            "warn",
        ),
    }
}

/// 路径所在盘的展示名（如 `D: 盘`）；非盘符路径（网络共享等）显示路径本身。
fn drive_label(path: &std::path::Path) -> String {
    #[cfg(windows)]
    {
        if let Some(std::path::Component::Prefix(p)) = path.components().next() {
            if let std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) = p.kind() {
                return format!("{}: 盘", d as char);
            }
        }
    }
    path.display().to_string()
}

fn check_network() -> EnvItem {
    let url = match msix_url_for_arch(native_arch()) {
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
            "当前为普通权限：通常可直接为当前用户安装，必要时会请求一次提权".into(),
            "warn",
        )
    }
}
