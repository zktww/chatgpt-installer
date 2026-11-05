//! Tauri 命令：前端 IPC 入口。
//!
//! 线程模型（Tauri 2）：同步命令在主线程执行，会阻塞界面。
//! 因此所有耗时操作（网络、PowerShell、文件 IO）都走
//! `async` 命令 + `spawn_blocking`，把阻塞工作移到后台线程。
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::constants::*;
use crate::download::{self, cmp_versions, head_cdn_info, DownloadInfo, DownloadState};
use crate::elevate::restart_elevated;
use crate::env_check::{check_env as run_env_check, EnvItem};
use crate::errors::AppError;
use crate::install::{
    emit_stage, emit_stage_ex, ensure_license, install_for_user, installed_app_version,
    provision_all_users, uninstall_package, LICENSE_FILE,
};
use crate::sys::is_admin;
use crate::verify::{verify_package as run_verify_package, VerifyInfo};

/// 诊断日志：内存保留一份，同时追加写入 `installer.log`。
/// 落盘是为了提权重启后（新进程）仍能拿到提权前的完整日志。
#[derive(Default)]
pub struct LogState {
    pub mem: Mutex<String>,
    file_started: AtomicBool,
}

/// 应用上下文：提权重启时透传的原始用户信息与续跑动作。
/// 标准用户用另一管理员账号提权后，进程用户会变化，
/// 安装包目录必须沿用原用户的值，不能重新计算。
#[derive(Default)]
pub struct AppCtx {
    pub pkg_dir_override: Mutex<Option<PathBuf>>,
    /// 原用户的 LOCALAPPDATA（--data-dir 透传）：跨用户提权时，
    /// 设置文件与安装记录写回原用户位置，而非管理员账号的 AppData。
    pub data_dir_override: Mutex<Option<PathBuf>>,
    /// 提权后待续跑的动作（--resume）：由前端加载完成后主动领取，
    /// 避免后端定时推送时前端尚未注册监听而丢失。
    pub resume_action: Mutex<Option<String>>,
}

/// 原用户的 LOCALAPPDATA（仅跨用户提权时有值）；无值时调用方回退 $env:LOCALAPPDATA。
pub(crate) fn data_dir_override(app: &AppHandle) -> Option<PathBuf> {
    // 注意：不能用 and_then 链式写法——MutexGuard 会借用闭包内的临时 State，
    // 出闭包即悬空（E0515）；必须用 if let / let 绑定把 State 留在函数作用域内
    let ctx = app.try_state::<AppCtx>()?;
    let guard = ctx.data_dir_override.lock().ok()?;
    guard.clone()
}

/* ---------------- 日志 ---------------- */

const LOG_FILE: &str = "installer.log";
const LOG_MAX_BYTES: u64 = 1024 * 1024;
const LOG_KEEP_BYTES: usize = 256 * 1024;

fn started_at() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

fn log_path(app: &AppHandle) -> Option<PathBuf> {
    crate::settings::default_data_dir(app)
        .ok()
        .map(|d| d.join(LOG_FILE))
}

/// 读取文件末尾最多 `keep` 字节（按 UTF-8 宽松解码）。
fn read_tail(path: &Path, keep: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(keep);
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

fn append_log_file(app: &AppHandle, line: &str) {
    let Some(path) = log_path(app) else { return };
    let first = app
        .try_state::<LogState>()
        .map(|s| !s.file_started.swap(true, Ordering::SeqCst))
        .unwrap_or(false);
    let mut text = String::new();
    if first {
        // 会话开始：日志过大则只保留末尾一段
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
            if let Some(tail) = read_tail(&path, LOG_KEEP_BYTES) {
                let _ = std::fs::write(&path, tail);
            }
        }
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        text.push_str(&format!(
            "\n===== session unix={} pid={} v{} arch={} admin={} =====\n",
            unix,
            std::process::id(),
            env!("CARGO_PKG_VERSION"),
            crate::sys::native_arch(),
            is_admin()
        ));
    }
    text.push_str(line);
    text.push('\n');
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

pub(crate) fn push_log(app: &AppHandle, line: &str) {
    let line = format!("[{:>7.1}s] {}", started_at().elapsed().as_secs_f32(), line);
    if let Some(state) = app.try_state::<LogState>() {
        if let Ok(mut log) = state.mem.lock() {
            log.push_str(&line);
            log.push('\n');
            // 防止无限制增长
            if log.len() > 200_000 {
                let mut cut = log.len() - 150_000;
                while !log.is_char_boundary(cut) {
                    cut += 1;
                }
                log.drain(..cut);
            }
        }
    }
    append_log_file(app, &line);
}

/* ---------------- 目录 ---------------- */

/// 安装包所在目录：提权透传 > 用户自定义 > 默认应用数据目录。
pub(crate) fn pkg_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    if let Some(ctx) = app.try_state::<AppCtx>() {
        if let Ok(guard) = ctx.pkg_dir_override.lock() {
            if let Some(d) = guard.as_ref() {
                return Ok(d.clone());
            }
        }
    }
    if let Some(custom) = crate::settings::load_custom_dir(app) {
        return Ok(custom);
    }
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| AppError::new("IO_ERROR", "无法确定数据目录", e.to_string()))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::new("IO_ERROR", "无法创建数据目录", e.to_string()))?;
    Ok(dir)
}

fn ensure_not_downloading(app: &AppHandle) -> Result<(), AppError> {
    let state: State<DownloadState> = app.state();
    if state.running.load(Ordering::SeqCst) {
        return Err(AppError::busy());
    }
    Ok(())
}

/// 当前生效的下载目录（供前端展示）。
#[tauri::command]
pub fn get_pkg_dir(app: AppHandle) -> Result<String, AppError> {
    Ok(pkg_dir(&app)?.to_string_lossy().into())
}

/// 设置自定义下载目录（手动输入兜底）：校验绝对路径、可创建、可写后持久化。
#[tauri::command]
pub fn set_pkg_dir(app: AppHandle, path: String) -> Result<String, AppError> {
    ensure_not_downloading(&app)?;
    let dir = crate::settings::save_custom_dir(&app, &path)?;
    push_log(&app, &format!("[settings] pkg_dir -> {}", dir.display()));
    Ok(dir.to_string_lossy().into())
}

/// 弹出系统文件夹选择器更改下载目录；用户取消返回 None。
#[tauri::command]
pub async fn pick_pkg_dir(app: AppHandle) -> Result<Option<String>, AppError> {
    ensure_not_downloading(&app)?;
    blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let mut builder = app.dialog().file().set_title("选择安装包下载目录");
        if let Ok(cur) = pkg_dir(&app) {
            builder = builder.set_directory(cur);
        }
        let Some(picked) = builder.blocking_pick_folder() else {
            return Ok(None);
        };
        let path = picked
            .into_path()
            .map_err(|e| AppError::new("BAD_DIR", "无法识别所选目录", e.to_string()))?;
        let dir = crate::settings::save_custom_dir(&app, &path.to_string_lossy())?;
        push_log(&app, &format!("[settings] pkg_dir -> {}", dir.display()));
        Ok(Some(dir.to_string_lossy().into()))
    })
    .await
}

fn msix_path(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(download::resolve_target(app)?.1)
}

async fn blocking<T, F>(f: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(inner) => inner,
        Err(e) => Err(AppError::new("INTERNAL", "后台任务异常退出", e.to_string())),
    }
}

/* ---------------- 基础信息 ---------------- */

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub version: &'static str,
    pub arch: &'static str,
    pub admin: bool,
}

#[tauri::command]
pub fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        arch: crate::sys::native_arch(),
        admin: is_admin(),
    }
}

/// 领取提权重启后的续跑动作（只能领取一次）。
#[tauri::command]
pub fn take_resume_action(app: AppHandle) -> Option<String> {
    let ctx = app.try_state::<AppCtx>()?;
    let mut guard = ctx.resume_action.lock().ok()?;
    guard.take()
}

#[tauri::command]
pub async fn check_env(app: AppHandle) -> Result<Vec<EnvItem>, AppError> {
    blocking(move || {
        let items = run_env_check(&app);
        for it in &items {
            push_log(
                &app,
                &format!("[env] {} {}: {}", it.status, it.id, it.detail),
            );
        }
        Ok(items)
    })
    .await
}

/* ---------------- 下载 ---------------- */

/// 下载前的准备（后台线程执行，含网络请求）：
/// 解析目标；本地已有完整包时确认它与云端一致，过期则删掉重新下载。
fn prepare_download(app: &AppHandle) -> Result<(String, PathBuf, Option<u64>), AppError> {
    let (url, dest) = download::resolve_target(app)?;
    if dest.exists() {
        if download::is_local_stale(&url, &dest) {
            push_log(app, "[download] local package is outdated, re-downloading");
            let _ = std::fs::remove_file(&dest);
            let _ = std::fs::remove_file(dest.with_extension("etag"));
        } else {
            let total = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            return Ok((url, dest, Some(total)));
        }
    }
    Ok((url, dest, None))
}

#[tauri::command]
pub async fn start_download(app: AppHandle) -> Result<DownloadInfo, AppError> {
    {
        let state: State<DownloadState> = app.state();
        // swap：检查与置位原子完成，防止连点触发两个下载线程
        if state.running.swap(true, Ordering::SeqCst) {
            return Err(AppError::busy());
        }
        state.cancel.store(false, Ordering::SeqCst);
        state.paused.store(false, Ordering::SeqCst);
    }

    let app_prep = app.clone();
    let prepared = blocking(move || prepare_download(&app_prep)).await;
    let (url, dest, complete) = match prepared {
        Ok(t) => t,
        Err(e) => {
            // 准备失败也要复位 running，否则之后每次点下载都会报 BUSY
            app.state::<DownloadState>()
                .running
                .store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    if let Some(total) = complete {
        app.state::<DownloadState>()
            .running
            .store(false, Ordering::SeqCst);
        push_log(
            &app,
            &format!("[download] already complete: {}", dest.display()),
        );
        return Ok(DownloadInfo {
            url,
            dest: dest.to_string_lossy().into(),
            resumed_from: total,
            total,
            complete: true,
        });
    }

    let resumed_from = std::fs::metadata(dest.with_extension("part"))
        .map(|m| m.len())
        .unwrap_or(0);
    push_log(
        &app,
        &format!(
            "[download] start {} -> {} (resume from {})",
            url,
            dest.display(),
            resumed_from
        ),
    );
    let url2 = url.clone();
    let dest2 = dest.clone();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let result = download::run_download(&app2, &url2, &dest2);
        let state: State<DownloadState> = app2.state();
        state.running.store(false, Ordering::SeqCst);
        match result {
            Ok(total) => {
                push_log(&app2, &format!("[download] done, {} bytes", total));
                let _ = app2.emit("download-done", serde_json::json!({ "total": total }));
            }
            Err(e) if e.code == "PAUSED" => {
                // 用户暂停不是错误：前端显示"已暂停"状态，不进失败页
                push_log(&app2, "[download] paused by user");
                let _ = app2.emit("download-paused", serde_json::json!({}));
            }
            Err(e) => {
                push_log(&app2, &format!("[download] error: {} {}", e, e.hint));
                let _ = app2.emit("download-error", e);
            }
        }
    });

    Ok(DownloadInfo {
        url,
        dest: dest.to_string_lossy().into(),
        resumed_from,
        total: 0,
        complete: false,
    })
}

#[tauri::command]
pub fn pause_download(app: AppHandle) -> Result<(), AppError> {
    let state: State<DownloadState> = app.state();
    // 暂停与取消共用 cancel 停掉工作线程，但用 paused 区分事件：暂停不进失败页。
    // 工作线程退出后才会发 download-paused，前端据此再允许「继续」。
    state.paused.store(true, Ordering::SeqCst);
    state.cancel.store(true, Ordering::SeqCst);
    Ok(())
}

/// 删除本地安装包及分片（校验失败后「重试」用：强制重新下载）。
#[tauri::command]
pub fn discard_package(app: AppHandle) -> Result<(), AppError> {
    ensure_not_downloading(&app)?;
    let msix = msix_path(&app)?;
    for p in [
        msix.clone(),
        msix.with_extension("part"),
        msix.with_extension("etag"),
    ] {
        let _ = std::fs::remove_file(&p);
    }
    push_log(&app, "[download] local package discarded");
    Ok(())
}

/* ---------------- 校验 ---------------- */

#[tauri::command]
pub async fn verify_package(app: AppHandle) -> Result<VerifyInfo, AppError> {
    blocking(move || {
        let path = msix_path(&app)?;
        push_log(&app, &format!("[verify] {}", path.display()));
        match run_verify_package(&path) {
            Ok(info) => {
                push_log(
                    &app,
                    &format!(
                        "[verify] ok: {} {} ({} bytes, issuer {})",
                        info.identity, info.version, info.size, info.issuer
                    ),
                );
                Ok(info)
            }
            Err(e) => {
                push_log(&app, &format!("[verify] failed: {} {}", e, e.hint));
                Err(e)
            }
        }
    })
    .await
}

/* ---------------- 安装 ---------------- */

/// 安装成功后清理安装包相关文件（800MB+），只保留设置与日志。
/// 卸载/重装都不需要这些文件，重装会重新下载。
fn cleanup_package_files(app: &AppHandle, msix: &Path) {
    let mut files = vec![
        msix.to_path_buf(),
        msix.with_extension("part"),
        msix.with_extension("etag"),
    ];
    if let Some(dir) = msix.parent() {
        files.push(dir.join(LICENSE_FILE));
    }
    for p in files {
        let _ = std::fs::remove_file(&p);
    }
    push_log(app, "[install] package files cleaned");
}

/// 安装成功后记录包 ETag 与版本，供“检查更新”对比云端。
/// 失败不致命，只影响下次检查更新的准确性。
fn record_install(app: &AppHandle, msix: &Path, version: &str) {
    let etag = std::fs::read_to_string(msix.with_extension("etag"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    crate::settings::save_install_record(app, etag.as_deref(), version);
    push_log(
        app,
        &format!("[install] recorded version {} etag {:?}", version, etag),
    );
}

/// 安装主流程（后台线程执行）：
/// 1. 强制安全校验（签名/发布者/包身份），任何入口都必须通过；
/// 2. 已提权：当前用户注册 + 整机预置；
/// 3. 未提权：先尝试免提权为当前用户安装，失败且疑似权限问题时才要求提权。
fn install_flow(app: &AppHandle) -> Result<(), AppError> {
    let msix = msix_path(app)?;
    if !msix.exists() {
        return Err(AppError::new(
            "NO_PACKAGE",
            "找不到安装包",
            "请先完成下载步骤。",
        ));
    }
    let info = run_verify_package(&msix)?;
    push_log(
        app,
        &format!(
            "[install] verified {} {} ({} bytes)",
            info.identity, info.version, info.size
        ),
    );

    if is_admin() {
        emit_stage(app, 1, "正在为当前用户注册应用包…");
        install_for_user(&msix)?;
        emit_stage(app, 2, "正在为所有用户预置…");
        let dir = pkg_dir(app)?;
        match ensure_license(app, &dir) {
            Ok(license) => {
                for w in provision_all_users(&msix, &license) {
                    push_log(app, &format!("[install] warning: {}", w));
                    let _ = app.emit("install-warning", w);
                }
            }
            Err(e) => {
                // 授权文件下载失败只影响整机预置，当前用户已可用
                let w = format!("整机预置跳过（授权文件获取失败）：{}", e);
                push_log(app, &format!("[install] warning: {}", w));
                let _ = app.emit("install-warning", w);
            }
        }
        emit_stage(app, 3, "安装完成");
        push_log(app, "[install] success (elevated)");
        record_install(app, &msix, &info.version);
        cleanup_package_files(app, &msix);
        return Ok(());
    }

    // 未提权：先尝试直接为当前用户安装（通常无需管理员权限）
    emit_stage(app, 1, "正在为当前用户注册应用包…");
    match install_for_user(&msix) {
        Ok(()) => {
            emit_stage_ex(app, 2, "普通权限下跳过整机预置（不影响使用）", true);
            emit_stage(app, 3, "安装完成");
            push_log(app, "[install] success (per-user, no elevation)");
            record_install(app, &msix, &info.version);
            cleanup_package_files(app, &msix);
            Ok(())
        }
        Err(e) if e.is_elevation_error() => {
            push_log(
                app,
                &format!("[install] per-user install needs elevation: {}", e),
            );
            Err(AppError::need_elevation())
        }
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub async fn install_package(app: AppHandle) -> Result<(), AppError> {
    blocking(move || {
        let r = install_flow(&app);
        if let Err(e) = &r {
            push_log(&app, &format!("[install] failed: {} {}", e, e.hint));
        }
        r
    })
    .await
}

/// 提权重启。action: "install" | "uninstall" | "uninstall-data"。
/// 会把当前用户的安装包目录与数据目录透传给提权后的进程，避免用户上下文切换导致找不到包。
/// UAC 通过后当前进程立即退出，由新进程接管；UAC 被取消则返回 `UAC_CANCELLED`。
#[tauri::command]
pub async fn restart_elevated_cmd(app: AppHandle, action: String) -> Result<(), AppError> {
    if !matches!(action.as_str(), "install" | "uninstall" | "uninstall-data") {
        return Err(AppError::new("BAD_ACTION", "未知的续跑动作", action));
    }
    let dir = pkg_dir(&app)?;
    let mut args = vec![
        format!("--resume={}", action),
        format!("--pkg-dir={}", dir.display()),
    ];
    // 原用户的 LOCALAPPDATA：用另一管理员账号提权时进程用户会变化，
    // 设置/安装记录/数据清理必须作用回原用户，不能跟着管理员账号走
    if let Some(dd) = data_dir_override(&app)
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| std::env::var("LOCALAPPDATA").ok())
        .filter(|s| !s.is_empty())
    {
        args.push(format!("--data-dir={dd}"));
    }
    push_log(&app, &format!("[elevate] request: {}", action));
    let result = blocking(move || restart_elevated(&args)).await;
    match result {
        Ok(()) => {
            push_log(
                &app,
                "[elevate] accepted, handing over to elevated instance",
            );
            app.exit(0);
            Ok(())
        }
        Err(e) => {
            push_log(&app, &format!("[elevate] {}", e));
            Err(e)
        }
    }
}

#[tauri::command]
pub async fn uninstall_package_cmd(
    app: AppHandle,
    remove_data: bool,
) -> Result<Vec<String>, AppError> {
    if !is_admin() {
        return Err(AppError::need_elevation());
    }
    blocking(move || {
        push_log(
            &app,
            &format!("[uninstall] begin (remove_data={})", remove_data),
        );
        let warnings = uninstall_package(&app, remove_data)?;
        for w in &warnings {
            push_log(&app, &format!("[uninstall] warning: {}", w));
        }
        push_log(&app, "[uninstall] done");
        Ok(warnings)
    })
    .await
}

/* ---------------- 检查更新 ---------------- */

#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheck {
    pub installed: bool,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub has_update: bool,
    pub note: String,
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<UpdateCheck, AppError> {
    blocking(move || run_check_update(&app)).await
}

fn run_check_update(app: &AppHandle) -> Result<UpdateCheck, AppError> {
    let installed_version = installed_app_version();
    let Some(ver) = installed_version else {
        return Ok(UpdateCheck {
            installed: false,
            installed_version: None,
            latest_version: None,
            has_update: false,
            note: "本机尚未安装 ChatGPT".into(),
        });
    };
    let record = crate::settings::load_install_record(app);
    // 版本漂移检测：本机当前版本 vs 本工具上次安装记录的版本。
    // 微软商店自动更新等其他渠道会改变本机版本，我们控制不了，但能识别出来并如实告知。
    let drift_note: Option<String> = record
        .as_ref()
        .and_then(|r| r.version.clone())
        .filter(|rv| rv != &ver)
        .map(|rv| format!("（本工具上次安装的是 v{rv}，可能已通过其他渠道更新）"));
    let suffix = drift_note.unwrap_or_default();
    // 每次检查都先请求 CDN 拿官方版本元数据，不依赖历史 ETag。
    let url = msix_url_for_arch(crate::sys::native_arch()).ok_or_else(|| {
        AppError::new(
            "NO_PACKAGE",
            "不支持的 CPU 架构",
            "当前架构没有对应的官方安装包。",
        )
    })?;
    let Some(info) = head_cdn_info(url) else {
        return Ok(UpdateCheck {
            installed: true,
            installed_version: Some(ver.clone()),
            latest_version: None,
            has_update: false,
            note: format!("本机已安装 v{ver}，暂时无法获取云端版本{suffix}"),
        });
    };
    // 优先用官方版本号做精确对比
    if let Some(cdn_ver) = info.version {
        let (has_update, note) = match cmp_versions(&cdn_ver, &ver) {
            Some(std::cmp::Ordering::Greater) => (
                true,
                format!("发现新版本 v{cdn_ver}（本机 v{ver}）{suffix}"),
            ),
            _ => (false, format!("本机 v{ver} 已是最新{suffix}")),
        };
        return Ok(UpdateCheck {
            installed: true,
            installed_version: Some(ver),
            latest_version: Some(cdn_ver),
            has_update,
            note,
        });
    }
    // 回退：ETag 启发式（官方若去掉版本元数据时仍可用）。
    // 只有"云端 ETag 与历史 ETag 都在"时才能对比；缺任何一边都如实告知无法判断。
    let stored_etag = record.as_ref().and_then(|r| r.etag.clone());
    let (has_update, note) = match (info.etag, stored_etag) {
        (Some(cur), Some(stored)) if cur == stored => {
            (false, format!("本机 v{ver} 已是最新{suffix}"))
        }
        (Some(_), Some(_)) => (true, format!("云端安装包有更新（本机 v{ver}）{suffix}")),
        _ => (
            false,
            format!("本机已安装 v{ver}，无法判断是否有新版，可重新安装以确保最新{suffix}"),
        ),
    };
    Ok(UpdateCheck {
        installed: true,
        installed_version: Some(ver),
        latest_version: None,
        has_update,
        note,
    })
}

/* ---------------- 其他 ---------------- */

#[tauri::command]
pub fn launch_app(app: AppHandle) -> Result<(), AppError> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(format!("shell:AppsFolder\\{}", APP_AUMID))
            .spawn()
            .map_err(|e| AppError::new("LAUNCH_FAILED", "无法启动 ChatGPT", e.to_string()))?;
        push_log(&app, "[app] launched ChatGPT");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Err(AppError::new("UNSUPPORTED_OS", "启动仅支持 Windows", ""))
    }
}

/// 诊断日志：优先读落盘文件（含提权前后多个会话），读不到时用内存日志。
#[tauri::command]
pub fn get_last_log(app: AppHandle) -> Result<String, AppError> {
    if let Some(text) = log_path(&app).and_then(|p| read_tail(&p, 200_000)) {
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }
    let state: State<LogState> = app.state();
    let mem = state.mem.lock().map(|l| l.clone()).unwrap_or_default();
    Ok(mem)
}

/// 退出程序：安装成功页的「关闭」按钮 / 自动关闭倒计时。
#[tauri::command]
pub fn exit_app(app: AppHandle) {
    push_log(&app, "[app] exit");
    app.exit(0);
}
