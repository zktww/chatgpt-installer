//! 固定常量：官方下载地址与包身份标识。
//!
//! 安装包全部来自 OpenAI 官方 CDN，本程序不做任何二次打包或修改，
//! 也不托管任何二进制文件。

/// x64 官方安装包（直连，内容随官方发布更新）
pub const X64_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/ChatGPT-x64.msix";
/// ARM64 官方安装包
pub const ARM64_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/ChatGPT-arm64.msix";
/// 企业离线授权文件（整机预置时使用）
pub const LICENSE_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/ChatGPT-License.xml";

/// MSIX 包身份（必须与 AppxManifest.xml 内一致）
pub const EXPECTED_IDENTITY: &str = "OpenAI.Codex";
/// 商店签名发布者 CN（Microsoft Marketplace 证书链）
pub const EXPECTED_PUBLISHER_CN: &str = "50BDFD77-8903-4850-9FFE-6E8522F64D5B";
/// 包系列名
pub const PACKAGE_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";
/// 应用用户模型 ID，用于启动已安装的应用
pub const APP_AUMID: &str = "OpenAI.Codex_2p2nqsd0c76g0!App";

/// 最低系统版本（Windows 10 2004）
pub const MIN_BUILD_NUMBER: u32 = 19041;
/// 磁盘空间要求：安装包 ~800MB + 展开后 ~1.9GB，预留 3GB
pub const REQUIRED_DISK_BYTES: u64 = 3 * 1024 * 1024 * 1024;

pub fn msix_url_for_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "x64" => Some(X64_URL),
        "aarch64" | "arm64" => Some(ARM64_URL),
        _ => None,
    }
}

pub fn msix_filename_for_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "x64" => Some("ChatGPT-x64.msix"),
        "aarch64" | "arm64" => Some("ChatGPT-arm64.msix"),
        _ => None,
    }
}
