//! 断点续传下载：Range 请求 + 本地 .part 文件 + 进度事件。
//!
//! - 已有未完成的分片会自动续传（HTTP 206）；
//! - 服务器忽略 Range（HTTP 200）或分片失效（HTTP 416）时，删除分片从头开始；
//! - 用 ETag + If-Range 防止官方更新版本后把新旧文件拼接在一起；
//! - 传输中断保留分片，下次自动续传。
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::constants::*;
use crate::errors::AppError;

#[derive(Default)]
pub struct DownloadState {
    pub cancel: AtomicBool,
    pub running: AtomicBool,
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

/// 带断点续传的大文件下载。阻塞执行，调用方应在独立线程中调用。
/// 通过 `download-progress` / `download-done` / `download-error` 事件与前端通信。
pub fn run_download(app: &AppHandle, url: &str, dest: &Path) -> Result<u64, AppError> {
    let part = dest.with_extension("part");
    let etag_file = dest.with_extension("etag");
    let mut downloaded: u64 = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let stored_etag: Option<String> = fs::read_to_string(&etag_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::network(e.to_string()))?;

    // HEAD 拿总大小与当前 ETag（失败不致命）
    let head = client.head(url).send().ok();
    let mut total: u64 = head
        .as_ref()
        .and_then(|r| header_u64(r, reqwest::header::CONTENT_LENGTH.as_str()))
        .unwrap_or(0);

    // 分片已完整：直接完成（版本差异由后续签名/清单校验兜底，旧版本也可正常安装）
    if total > 0 && downloaded >= total {
        fs::rename(&part, dest)
            .map_err(|e| AppError::new("IO_ERROR", "文件整理失败", e.to_string()))?;
        let _ = fs::remove_file(&etag_file);
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
            let _ = fs::remove_file(&etag_file);
        }
        if total == 0 {
            total = header_u64(&resp, reqwest::header::CONTENT_LENGTH.as_str()).unwrap_or(0);
        }
        // 记录本次下载的 ETag，供下次续传做 If-Range
        if let Some(etag) = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
        {
            let _ = fs::write(&etag_file, etag);
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
    let mut buf = [0u8; 128 * 1024];
    loop {
        if state.cancel.load(Ordering::SeqCst) {
            return Err(AppError::cancelled());
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
        if last_emit.elapsed() >= Duration::from_millis(200) {
            emit(downloaded);
            last_emit = Instant::now();
        }
    }
    emit(downloaded);
    drop(file);
    fs::rename(&part, dest)
        .map_err(|e| AppError::new("IO_ERROR", "文件整理失败", e.to_string()))?;
    let _ = fs::remove_file(&etag_file);
    Ok(total)
}

/// 解析本机应下载的包地址与目标路径。
pub fn resolve_target(app: &AppHandle) -> Result<(String, PathBuf), AppError> {
    let arch = std::env::consts::ARCH;
    let url = msix_url_for_arch(arch).ok_or_else(|| {
        AppError::new(
            "UNSUPPORTED_ARCH",
            "不支持的 CPU 架构",
            format!("当前架构 {} 仅支持 x64 / ARM64", arch),
        )
    })?;
    let filename = msix_filename_for_arch(arch).unwrap();
    // 下载目录与安装/校验共用同一生效目录（提权透传 > 用户自定义 > 默认）
    let dir = crate::cmds::pkg_dir(app)?;
    Ok((url.to_string(), dir.join(filename)))
}
