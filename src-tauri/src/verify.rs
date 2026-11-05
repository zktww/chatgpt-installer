//! 安装前校验：Authenticode 签名 + 发布者 + 颁发者 + 包身份。
//!
//! 信任依据是**签名是否有效**，而非哈希比对——
//! 官方下载地址是固定 latest 指针，版本会变，哈希无法 pin。
use std::io::Read;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use crate::constants::*;
use crate::errors::AppError;

#[derive(Debug, Clone, Serialize)]
pub struct VerifyInfo {
    /// "Valid" 等
    pub signature: String,
    pub publisher: String,
    /// 签名证书颁发者（Microsoft Marketplace CA）
    pub issuer: String,
    pub identity: String,
    pub version: String,
    pub size: u64,
}

/// Get-AuthenticodeSignature 的精简结果（一次 PowerShell 调用取全）。
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SigInfo {
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "Subject", default)]
    subject: Option<String>,
    #[serde(rename = "Issuer", default)]
    issuer: Option<String>,
}

/// 证书 DN（如 `CN=xxx, O=yyy`）中是否存在**完全相等**的某个 RDN。
/// 比 `contains` 更严格：防止 `CN=evil-<GUID>` 这类前后缀拼接混过去。
#[cfg_attr(not(windows), allow(dead_code))]
fn dn_has(dn: &str, rdn: &str) -> bool {
    dn.split(',')
        .map(str::trim)
        .any(|p| p.eq_ignore_ascii_case(rdn))
}

pub fn verify_package(msix_path: &Path) -> Result<VerifyInfo, AppError> {
    #[cfg(not(windows))]
    {
        let _ = msix_path;
        Err(AppError::new(
            "UNSUPPORTED_OS",
            "校验仅支持在 Windows 上执行",
            "请在 Windows 10（19041+）/ Windows 11 上运行本程序。",
        ))
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

        // 1. 先读清单（纯本地、毫秒级）：格式不对就不必再启动 PowerShell
        let (identity, version) = read_manifest_identity(msix_path)?;
        if identity != EXPECTED_IDENTITY {
            return Err(AppError::new(
                "BAD_IDENTITY",
                format!("包身份异常（{}）", identity),
                "安装包不是官方 ChatGPT 桌面版，已中止安装。",
            ));
        }

        // 2. 一次 PowerShell 调用取签名状态 / 主体 / 颁发者
        //    （-LiteralPath：路径中的 [ ] 不被当作通配符）
        let script = format!(
            "$s = Get-AuthenticodeSignature -LiteralPath {}; \
             [pscustomobject]@{{ Status = [string]$s.Status; \
             Subject = [string]$s.SignerCertificate.Subject; \
             Issuer = [string]$s.SignerCertificate.Issuer }} | ConvertTo-Json -Compress",
            crate::ps::quote_single(&msix_path.to_string_lossy())
        );
        let raw = crate::ps::text(&script)?;
        let sig: SigInfo = serde_json::from_str(&raw).map_err(|_| {
            AppError::new(
                "BAD_SIGNATURE",
                "无法读取数字签名信息",
                format!(
                    "签名查询返回了无法识别的结果：{}",
                    raw.chars().take(200).collect::<String>()
                ),
            )
        })?;
        if sig.status.trim() != "Valid" {
            return Err(AppError::new(
                "BAD_SIGNATURE",
                format!("数字签名无效（{}）", sig.status.trim()),
                "安装包可能已损坏或被篡改。请删除后重新下载；若反复出现请更换网络环境。",
            ));
        }

        // 3. 发布者必须精确等于 OpenAI 的商店发布者标识
        let subject = sig.subject.unwrap_or_default();
        if !dn_has(&subject, &format!("CN={}", EXPECTED_PUBLISHER_CN)) {
            return Err(AppError::new(
                "BAD_PUBLISHER",
                "签名发布者与官方不一致",
                format!("实际发布者：{}。为安全起见已中止安装。", subject),
            ));
        }

        // 4. 颁发者必须是微软（Microsoft Marketplace CA）：
        //    防止其他受信任 CA 给同名 CN 签发的证书混过去
        let issuer = sig.issuer.unwrap_or_default();
        if !dn_has(&issuer, EXPECTED_ISSUER_O) {
            return Err(AppError::new(
                "BAD_ISSUER",
                "签名证书颁发者不是微软",
                format!("实际颁发者：{}。为安全起见已中止安装。", issuer),
            ));
        }

        Ok(VerifyInfo {
            signature: sig.status.trim().to_string(),
            publisher: EXPECTED_PUBLISHER_CN.to_string(),
            issuer: issuer_cn(&issuer),
            identity,
            version,
            size,
        })
    }
}

/// 取颁发者 DN 里的 CN 部分用于展示（取不到就原样返回）。
#[cfg_attr(not(windows), allow(dead_code))]
fn issuer_cn(dn: &str) -> String {
    dn.split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("CN="))
        .unwrap_or(dn)
        .to_string()
}

/// 从 MSIX（zip 格式）中读取 AppxManifest.xml 的 Identity Name / Version。
pub(crate) fn read_manifest_identity(path: &Path) -> Result<(String, String), AppError> {
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
    parse_identity(&xml)
}

fn parse_identity(xml: &str) -> Result<(String, String), AppError> {
    let mut reader = Reader::from_str(xml);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dn_exact_match() {
        let dn = "CN=50BDFD77-8903-4850-9FFE-6E8522F64D5B, O=Foo";
        assert!(dn_has(dn, "CN=50BDFD77-8903-4850-9FFE-6E8522F64D5B"));
        assert!(!dn_has(
            "CN=evil-50BDFD77-8903-4850-9FFE-6E8522F64D5B",
            "CN=50BDFD77-8903-4850-9FFE-6E8522F64D5B"
        ));
        assert!(dn_has(
            "CN=Microsoft Marketplace CA G 024, O=Microsoft Corporation, L=Redmond, S=Washington, C=US",
            EXPECTED_ISSUER_O
        ));
    }

    #[test]
    fn issuer_cn_extracts() {
        assert_eq!(
            issuer_cn("CN=Microsoft Marketplace CA G 024, O=Microsoft Corporation"),
            "Microsoft Marketplace CA G 024"
        );
    }

    #[test]
    fn parses_identity() {
        let xml = r#"<?xml version="1.0"?><Package><Identity Name="OpenAI.Codex" Version="26.1.2.0" Publisher="CN=x"/></Package>"#;
        let (n, v) = parse_identity(xml).unwrap();
        assert_eq!(n, "OpenAI.Codex");
        assert_eq!(v, "26.1.2.0");
    }
}
