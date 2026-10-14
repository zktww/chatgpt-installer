//! 安装前校验：Authenticode 签名 + 发布者 + 包身份。
//!
//! 信任依据是**签名是否有效**，而非哈希比对——
//! 官方下载地址是固定 latest 指针，版本会变，哈希无法 pin。
use std::io::Read;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;

use crate::constants::*;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use crate::errors::AppError;

#[derive(Debug, Clone, Serialize)]
pub struct VerifyInfo {
    /// "Valid" 等
    pub signature: String,
    pub publisher: String,
    pub identity: String,
    pub version: String,
    pub size: u64,
}

pub fn verify_package(msix_path: &Path) -> Result<VerifyInfo, AppError> {
    #[cfg(not(windows))]
    {
        let _ = msix_path;
        return Err(AppError::new(
            "UNSUPPORTED_OS",
            "校验仅支持在 Windows 上执行",
            "请在 Windows 10（19041+）/ Windows 11 上运行本程序。",
        ));
    }
    #[cfg(windows)]
    {
        let size = std::fs::metadata(msix_path)
            .map(|m| m.len())
            .map_err(|e| AppError::new("IO_ERROR", "无法读取安装包", e.to_string()))?;
        if size < 10 * 1024 * 1024 {
            return Err(AppError::new(
                "BAD_PACKAGE",
                "安装包体积异常（可能下载不完整）",
                "请删除安装包后重新下载，下载支持断点续传。",
            ));
        }

        // 1. Authenticode 签名状态（-LiteralPath：路径中的 [ ] 不被当作通配符）
        let status = ps_text(&format!(
            "(Get-AuthenticodeSignature -LiteralPath '{}').Status",
            ps_escape(&msix_path.to_string_lossy())
        ))?;
        if status.trim() != "Valid" {
            return Err(AppError::new(
                "BAD_SIGNATURE",
                format!("数字签名无效（{}）", status.trim()),
                "安装包可能已损坏或被篡改。请删除后重新下载；若反复出现请更换网络环境。",
            ));
        }

        // 2. 发布者必须为 OpenAI 的商店发布者标识
        let subject = ps_text(&format!(
            "(Get-AuthenticodeSignature -LiteralPath '{}').SignerCertificate.Subject",
            ps_escape(&msix_path.to_string_lossy())
        ))?;
        if !subject.contains(EXPECTED_PUBLISHER_CN) {
            return Err(AppError::new(
                "BAD_PUBLISHER",
                "签名发布者与官方不一致",
                "为安全起见已中止安装。请确认安装包来自本程序下载的官方地址。",
            ));
        }

        // 3. 包身份必须为 OpenAI.Codex（读 MSIX 内清单）
        let (identity, version) = read_manifest_identity(msix_path)?;
        if identity != EXPECTED_IDENTITY {
            return Err(AppError::new(
                "BAD_IDENTITY",
                format!("包身份异常（{}）", identity),
                "安装包不是官方 ChatGPT 桌面版，已中止安装。",
            ));
        }

        Ok(VerifyInfo {
            signature: status.trim().to_string(),
            publisher: EXPECTED_PUBLISHER_CN.to_string(),
            identity,
            version,
            size,
        })
    }
}

/// 从 MSIX（zip 格式）中读取 AppxManifest.xml 的 Identity Name / Version。
fn read_manifest_identity(path: &Path) -> Result<(String, String), AppError> {
    let file = std::fs::File::open(path)
        .map_err(|e| AppError::new("IO_ERROR", "无法打开安装包", e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| {
        AppError::new(
            "BAD_PACKAGE",
            "安装包格式错误",
            "文件不是有效的 MSIX（zip）格式，请重新下载。",
        )
    })?;
    let mut entry = zip.by_name("AppxManifest.xml").map_err(|_| {
        AppError::new(
            "BAD_PACKAGE",
            "安装包内缺少清单文件",
            "文件不完整，请重新下载。",
        )
    })?;
    let mut xml = String::new();
    entry
        .read_to_string(&mut xml)
        .map_err(|_| AppError::new("BAD_PACKAGE", "清单文件读取失败", "请重新下载安装包。"))?;

    let mut reader = Reader::from_str(&xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"Identity" => {
                let mut name = String::new();
                let mut version = String::new();
                for attr in e.attributes() {
                    let attr = attr.map_err(|_| {
                        AppError::new("BAD_MANIFEST", "清单解析失败", "请重新下载安装包。")
                    })?;
                    match attr.key.as_ref() {
                        b"Name" => {
                            name = attr
                                .unescape_value()
                                .map(|v| v.into_owned())
                                .unwrap_or_default()
                        }
                        b"Version" => {
                            version = attr
                                .unescape_value()
                                .map(|v| v.into_owned())
                                .unwrap_or_default()
                        }
                        _ => {}
                    }
                }
                return Ok((name, version));
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                return Err(AppError::new(
                    "BAD_MANIFEST",
                    "清单解析失败",
                    "请重新下载安装包。",
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Err(AppError::new(
        "BAD_MANIFEST",
        "清单中未找到包身份",
        "请重新下载安装包。",
    ))
}

#[cfg(windows)]
fn ps_text(script: &str) -> Result<String, AppError> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：不闪黑窗口
        .output()
        .map_err(|e| {
            AppError::new(
                "PS_FAILED",
                "无法调用 PowerShell",
                format!("请确认系统 PowerShell 可用：{}", e),
            )
        })?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}
