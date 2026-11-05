//! 断点续传下载：Range 请求 + 本地 .part 文件 + 进度事件。
//!
//! - 已有未完成的分片会自动续传（HTTP 206）；
//! - 服务器忽略 Range（HTTP 200）或分片失效（HTTP 416）时，删除分片从头开始；
//! - 用 ETag + If-Range 防止官方更新版本后把新旧文件拼接在一起；
//!   没有 ETag 记录的旧分片无法保证一致性，直接从头下载；
//! - 传输中断保留分片，下次自动续传；
//! - 下载完成后保留 `.etag`（供安装记录与「是否过期」判断），安装成功后统一清理。
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::constants::*;
use crate::errors::AppError;

#[derive(Default)]
pub struct DownloadState {
    pub cancel: AtomicBool,
    pub running: AtomicBool,
    /// 暂停标记：与 cancel 共用停掉工作线程，但事件走 download-paused 而非 download-error
    pub paused: AtomicBool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: u64,
    pub percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadInfo {
    pub url: String,
    pub dest: String,
    /// 本次启动时已存在的分片大小（续传起点）
    pub resumed_from: u64,
    pub total: u64,
    /// 安装包已完整存在（且与云端一致），无需再下载
    pub complete: bool,
}

/// 简单的一次性小文件下载（离线授权 xml 等），无进度事件。
pub fn download_small_file(url: &str, dest: &Path) -> Result<(), AppError> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AppError::network(e.to_string()))?;
    let bytes = client
        .get(url)
        .send()
        .map_err(|e| AppError::network(e.to_string()))?
        .error_for_status()
        .map_err(|e| AppError::network(e.to_string()))?
        .bytes()
        .map_err(|e| AppError::network(e.to_string()))?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AppError::new("IO_ERROR", "无法创建目录", e.to_string()))?;
    }
    fs::write(dest, &bytes)
        .map_err(|e| AppError::new("IO_ERROR", "文件写入失败", e.to_string()))?;
    Ok(())
}

fn header_u64(resp: &reqwest::blocking::Response, name: &str) -> Option<u64> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

/// 从 `Content-Range: bytes 100-999/1234` 中解析文件总大小。
fn content_range_total(resp: &reqwest::blocking::Response) -> Option<u64> {
    let v = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    v.rsplit('/').next()?.parse().ok()
}

/// 云端 MSIX 的 HEAD 信息：官方版本号 + ETag。
/// 版本号来自 Azure Blob 自定义元数据 `x-ms-meta-package_version`，
/// 由官方发布流水线写入（x64/arm64 实测一致）。
pub struct CdnInfo {
    pub version: Option<String>,
    pub etag: Option<String>,
}

pub fn head_cdn_info(url: &str) -> Option<CdnInfo> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.head(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let h = resp.headers();
    let version = h
        .get("x-ms-meta-package_version")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let etag = h
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Some(CdnInfo { version, etag })
}

/// 比较四段版本号（如 26.1007.2314.0）；解析失败返回 None。
pub fn cmp_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parse = |s: &str| -> Option<Vec<u64>> {
        s.trim().split('.').map(|p| p.parse::<u64>().ok()).collect()
    };
    let mut va = parse(a)?;
    let mut vb = parse(b)?;
    let n = va.len().max(vb.len());
    va.resize(n, 0);
    vb.resize(n, 0);
    Some(va.cmp(&vb))
}

fn read_etag(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 本地已完整的安装包是否已落后于云端（上次失败残留、或云端已发新版）。
/// 优先用官方版本号对比清单版本；其次对比 ETag；网络异常时保守返回 false（继续用本地包，
/// 之后的签名校验仍会兜底安全性）。
pub fn is_local_stale(url: &str, dest: &Path) -> bool {
    let Some(cdn) = head_cdn_info(url) else {
        return false;
    };
    if let Some(cdn_ver) = cdn.version.as_deref() {
        return match crate::verify::read_manifest_identity(dest) {
            // 本地包清单读不出来：文件已坏，按过期处理重新下载
            Err(_) => true,
            Ok((_, local_ver)) => {
                matches!(
                    cmp_versions(cdn_ver, &local_ver),
                    Some(std::cmp::Ordering::Greater)
                )
            }
        };
    }
    match (cdn.etag, read_etag(&dest.with_extension("etag"))) {
        (Some(cur), Some(stored)) => cur != stored,
        _ => false,
    }
}

/// 带断点续传的大文件下载。阻塞执行，调用方应在独立线程中调用。
/// 通过 `download-progress` 事件汇报进度，结果由调用方转成 done / paused / error 事件。
pub fn run_download(app: &AppHandle, url: &str, dest: &Path) -> Result<u64, AppError> {
    let part = dest.with_extension("part");
    let etag_file = dest.with_extension("etag");
    let mut downloaded: u64 = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let stored_etag = read_etag(&etag_file);

    // 有分片但没有 ETag 记录：无法用 If-Range 保证新旧版本不被拼接，直接从头下载
    if downloaded > 0 && stored_etag.is_none() {
        let _ = fs::remove_file(&part);
        downloaded = 0;
    }

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // 大文件下载必须禁用 reqwest 默认的 30 秒总超时，否则慢速连接下 800MB 必被掐断；
        // 中断后靠断点续传恢复。HEAD 等小请求单独设超时。
        .timeout(None)
        .build()
        .map_err(|e| AppError::network(e.to_string()))?;

    // HEAD 拿总大小（失败不致命）
    let head = client
        .head(url)
        .timeout(Duration::from_secs(15))
        .send()
        .ok();
    let mut total: u64 = head
        .as_ref()
        .and_then(|r| header_u64(r, reqwest::header::CONTENT_LENGTH.as_str()))
        .unwrap_or(0);
    let head_etag: Option<String> = head
        .as_ref()
        .and_then(|r| r.headers().get(reqwest::header::ETAG))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 分片已完整且云端未变：直接完成
    if total > 0 && downloaded >= total && head_etag.is_some() && head_etag == stored_etag {
        fs::rename(&part, dest)
            .map_err(|e| AppError::new("IO_ERROR", "文件整理失败", e.to_string()))?;
        return Ok(total);
    }

    let mut req = client.get(url);
    let mut resume = false;
    if downloaded > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={}-", downloaded));
        // 用最初下载时的 ETag 做 If-Range：官方更新版本后服务器会回 200，
        // 我们据此丢弃旧分片，避免新旧拼接
        if let Some(ref etag) = stored_etag {
            req = req.header("If-Range", etag);
        }
        resume = true;
    }
    let mut resp = req.send().map_err(|e| AppError::network(e.to_string()))?;
    let mut status = resp.status().as_u16();

    if status == 416 {
        // 分片在服务端已失效，清理后从头开始
        let _ = fs::remove_file(&part);
        let _ = fs::remove_file(&etag_file);
        downloaded = 0;
        resume = false;
        resp = client
            .get(url)
            .send()
            .map_err(|e| AppError::network(e.to_string()))?;
        status = resp.status().as_u16();
    }

    if status == 206 {
        // 正常续传：追加写入，分片保留
        if total == 0 {
            total = content_range_total(&resp).unwrap_or(0);
        }
    } else if status == 200 {
        if resume {
            // 服务器忽略了 Range（不支持断点续传，或 If-Range 未命中即版本已更新）：
            // 丢弃旧分片，用本次返回的完整 body 从头写入
            downloaded = 0;
            let _ = fs::remove_file(&part);
        }
        // 续传失败时的 Content-Length 才是整包大小
        total = header_u64(&resp, reqwest::header::CONTENT_LENGTH.as_str()).unwrap_or(total);
        // 记录本次下载的 ETag，供下次续传做 If-Range；没有 ETag 则清掉旧记录
        match resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
        {
            Some(etag) => {
                let _ = fs::write(&etag_file, etag);
            }
            None => {
                let _ = fs::remove_file(&etag_file);
            }
        }
    } else {
        return Err(AppError::network(format!("服务器返回 HTTP {}", status)));
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(downloaded == 0)
        .append(downloaded > 0)
        .open(&part)
        .map_err(|e| AppError::new("IO_ERROR", "无法写入下载文件", e.to_string()))?;

    let state: State<DownloadState> = app.state();
    let mut last_emit = Instant::now();
    let emit = |downloaded: u64| {
        let percent = if total > 0 {
            (downloaded as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        let _ = app.emit(
            "download-progress",
            DownloadProgress {
                downloaded,
                total,
                percent,
            },
        );
    };
    emit(downloaded);

    // reqwest blocking::Response 没有 chunk()，用 Read 分块读取
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if state.cancel.load(Ordering::SeqCst) {
            // 暂停与取消都停掉工作线程，但暂停走 PAUSED 事件（前端显示"已暂停"而非失败页）
            let _ = file.flush();
            return Err(if state.paused.load(Ordering::SeqCst) {
                AppError::paused()
            } else {
                AppError::cancelled()
            });
        }
        let n = resp
            .read(&mut buf)
            .map_err(|e| AppError::network(format!("传输中断（已保留进度）：{}", e)))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| AppError::new("IO_ERROR", "文件写入失败", e.to_string()))?;
        downloaded += n as u64;
        if last_emit.elapsed() >= Duration::from_millis(250) {
            emit(downloaded);
            last_emit = Instant::now();
        }
    }
    emit(downloaded);
    file.flush()
        .map_err(|e| AppError::new("IO_ERROR", "文件写入失败", e.to_string()))?;
    drop(file);
    if total > 0 && downloaded < total {
        // 连接被提前关闭：保留分片，下次续传
        return Err(AppError::network(format!(
            "传输提前结束（{} / {} 字节，已保留进度）",
            downloaded, total
        )));
    }
    let _ = fs::remove_file(dest);
    fs::rename(&part, dest)
        .map_err(|e| AppError::new("IO_ERROR", "文件整理失败", e.to_string()))?;
    Ok(downloaded)
}

/// 解析本机应下载的包地址与目标路径（按**原生**架构选包）。
pub fn resolve_target(app: &AppHandle) -> Result<(String, PathBuf), AppError> {
    let arch = crate::sys::native_arch();
    let (Some(url), Some(filename)) = (msix_url_for_arch(arch), msix_filename_for_arch(arch))
    else {
        return Err(AppError::new(
            "UNSUPPORTED_ARCH",
            "不支持的 CPU 架构",
            format!("当前架构 {} 仅支持 x64 / ARM64", arch),
        ));
    };
    // 下载目录与安装/校验共用同一生效目录（提权透传 > 用户自定义 > 默认）
    let dir = crate::cmds::pkg_dir(app)?;
    Ok((url.to_string(), dir.join(filename)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn version_compare() {
        assert_eq!(
            cmp_versions("26.1007.2314.0", "1.2025.301.0"),
            Some(Greater)
        );
        assert_eq!(cmp_versions("1.2.0", "1.2.0.0"), Some(Equal));
        assert_eq!(cmp_versions("1.2.3.4", "1.2.10.0"), Some(Less));
        assert_eq!(cmp_versions("1.x", "1.0"), None);
    }
}
