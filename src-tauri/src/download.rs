//! 断点续传下载：Range 请求 + 本地 .part 文件 + 进度事件。
//!
//! - 已有未完成的分片会自动续传；
//! - 服务器不支持 Range（返回 200）时自动从头开始；
//! - 返回 416（分片已过期/失效）时删除分片重下。
use std::fs::{self, OpenOptions};
use std::io::Write;
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

/// 带断点续传的大文件下载。阻塞执行，调用方应在独立线程中调用。
/// 通过 `download-progress` / `download-done` / `download-error` 事件与前端通信。
pub fn run_download(app: &AppHandle, url: &str, dest: &Path) -> Result<u64, AppError> {
    let part = dest.with_extension("part");
    let mut downloaded: u64 = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::network(e.to_string()))?;

    // 尝试获取文件总大小（HEAD 失败不致命）
    let total: u64 = client
        .head(url)
        .send()
        .ok()
        .and_then(|r| {
            r.headers()
                .get(reqwest::header::CONTENT_LENGTH)?
                .to_str()
                .ok()?
                .parse()
                .ok()
        })
        .unwrap_or(0);

    // 分片已完整：直接改名完成
    if total > 0 && downloaded >= total {
        fs::rename(&part, dest)
            .map_err(|e| AppError::new("IO_ERROR", "文件整理失败", e.to_string()))?;
        return Ok(total);
    }

    let mut req = client.get(url);
    if downloaded > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={}-", downloaded));
    }
    let mut resp = req.send().map_err(|e| AppError::network(e.to_string()))?;

    if resp.status().as_u16() == 416 {
        // 分片失效，从头开始
        let _ = fs::remove_file(&part);
        downloaded = 0;
        resp = client
            .get(url)
            .send()
            .map_err(|e| AppError::network(e.to_string()))?;
    } else if resp.status().is_success() && downloaded > 0 {
        // 服务器忽略了 Range（返回 200），必须截断重写
        downloaded = 0;
    }
    if !resp.status().is_success() {
        return Err(AppError::network(format!("服务器返回 {}", resp.status())));
    }
    let total = if total > 0 {
        total
    } else {
        // 退化：用 Content-Length / Content-Range 推断
        resp.headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(|len| len + downloaded)
            .unwrap_or(0)
    };

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

    loop {
        if state.cancel.load(Ordering::SeqCst) {
            return Err(AppError::cancelled());
        }
        match resp.chunk() {
            Ok(Some(chunk)) => {
                file.write_all(&chunk)
                    .map_err(|e| AppError::new("IO_ERROR", "文件写入失败", e.to_string()))?;
                downloaded += chunk.len() as u64;
                if last_emit.elapsed() >= Duration::from_millis(200) {
                    emit(downloaded);
                    last_emit = Instant::now();
                }
            }
            Ok(None) => break,
            Err(e) => {
                // 网络中断：保留分片，下次自动续传
                return Err(AppError::network(format!("传输中断（已保留进度）：{}", e)));
            }
        }
    }
    emit(downloaded);
    drop(file);
    fs::rename(&part, dest)
        .map_err(|e| AppError::new("IO_ERROR", "文件整理失败", e.to_string()))?;
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
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| AppError::new("IO_ERROR", "无法确定数据目录", e.to_string()))?;
    fs::create_dir_all(&dir)
        .map_err(|e| AppError::new("IO_ERROR", "无法创建数据目录", e.to_string()))?;
    Ok((url.to_string(), dir.join(filename)))
}
