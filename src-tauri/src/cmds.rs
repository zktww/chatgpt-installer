//! Tauri 命令：前端 IPC 入口。
//!
//! 线程模型（Tauri 2）：同步命令在主线程执行，会阻塞界面。
//! 因此所有耗时操作（网络、PowerShell、文件 IO）都走
//! `async` 命令 + `spawn_blocking`，把阻塞工作移到后台线程。
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::constants::*;
use crate::download::{self, DownloadInfo, DownloadState};
use crate::elevate::is_admin;
#[cfg(windows)]
use crate::elevate::restart_elevated;
use crate::env_check::{check_env as run_env_check, EnvItem};
use crate::errors::AppError;
use crate::install::{
    emit_stage, ensure_license, install_for_user, installed_app_version, provision_all_users,
    uninstall_package,
};
use crate::verify::{verify_package as run_verify_package, VerifyInfo};

/// 内存日志：记录关键步骤，供「复制诊断日志」使用。
#[derive(Default)]
pub struct LogState(pub Mutex<String>);

/// 应用上下文：提权重启时透传的原始用户信息。
/// 标准用户用另一管理员账号提权后，进程用户会变化，
/// 安装包目录必须沿用原用户的值，不能重新计算。
#[derive(Default)]
pub struct AppCtx {
    pub pkg_dir_override: Mutex<Option<PathBuf>>,
    /// 原用户的 LOCALAPPDATA（--data-dir 透传）：跨用户提权时，
    /// 设置文件与安装记录写回原用户位置，而非管理员账号的 AppData。
    pub data_dir_override: Mutex<Option<PathBuf>>,
}

/// 原用户的 LOCALAPPDATA（仅跨用户提权时有值）；无值时调用方回退 $env:LOCALAPPDATA。
pub(crate) fn data_dir_override(app: &AppHandle) -> Option<PathBuf> {
    app.try_state::<AppCtx>()
        .and_then(|ctx| ctx.data_dir_override.lock().ok())
        .and_then(|guard| guard.clone())
}

fn push_log(app: &AppHandle, line: &str) {
    if let Some(state) = app.try_state::<LogState>() {
        if let Ok(mut log) = state.0.lock() {
            log.push_str(line);
            log.push('\n');
            // 防止无限制增长
            if log.len() > 200_000 {
                let cut = log.len() - 150_000;
                log.drain(..cut);
            }
        }
    }
}

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

/// 当前生效的下载目录（供前端展示）。
#[tauri::command]
pub fn get_pkg_dir(app: AppHandle) -> Result<String, AppError> {
    Ok(pkg_dir(&app)?.to_string_lossy().into())
}

/// 设置自定义下载目录：校验绝对路径、可创建、可写后持久化。
#[tauri::command]
pub fn set_pkg_dir(app: AppHandle, path: String) -> Result<String, AppError> {
    let dir = crate::settings::save_custom_dir(&app, &path)?;
    push_log(&app, &format!("[settings] pkg_dir -> {}", dir.display()));
    Ok(dir.to_string_lossy().into())
}

fn msix_path(app: &AppHandle) -> Result<PathBuf, AppError> {
    let arch = std::env::consts::ARCH;
    let filename = msix_filename_for_arch(arch)
        .ok_or_else(|| AppError::new("UNSUPPORTED_ARCH", "不支持的 CPU 架构", arch))?;
    Ok(pkg_dir(app)?.join(filename))
}

fn blocking<T, F>(f: F) -> impl std::future::Future<Output = Result<T, AppError>>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    async move {
        match tauri::async_runtime::spawn_blocking(f).await {
            Ok(inner) => inner,
            Err(e) => Err(AppError::new("INTERNAL", "后台任务异常退出", e.to_string())),
        }
    }
}

#[tauri::command]
pub async fn check_env(app: AppHandle) -> Result<Vec<EnvItem>, AppError> {
    blocking(move || Ok(run_env_check(&app))).await
}

#[tauri::command]
pub fn start_download(app: AppHandle) -> Result<DownloadInfo, AppError> {
    let state: State<DownloadState> = app.state();
    if state.running.load(Ordering::SeqCst) {
        return Err(AppError::new(
            "BUSY",
            "下载已在进行中",
            "请等待当前下载完成。",
        ));
    }
    state.running.store(true, Ordering::SeqCst);
    state.cancel.store(false, Ordering::SeqCst);
    state.paused.store(false, Ordering::SeqCst);

    let (url, dest) = match download::resolve_target(&app) {
        Ok(t) => t,
        Err(e) => {
            // resolve 失败也要复位 running，否则之后每次点下载都会报 BUSY
            state.running.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    let resumed_from = std::fs::metadata(dest.with_extension("part"))
        .map(|m| m.len())
        .unwrap_or(0);
    // 已下载完整则直接返回
    if dest.exists() {
        state.running.store(false, Ordering::SeqCst);
        let total = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        return Ok(DownloadInfo {
            url,
            dest: dest.to_string_lossy().into(),
            resumed_from: total,
            total,
        });
    }

    push_log(
        &app,
        &format!("[download] start {} -> {}", url, dest.display()),
    );
    // move 捕获前先 clone：返回值仍需使用 url / dest
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
            Err(e) => {
                if e.code == "PAUSED" {
                    // 用户暂停不是错误：前端显示"已暂停"状态，不进失败页
                    push_log(&app2, "[download] paused by user");
                    let _ = app2.emit("download-paused", serde_json::json!({}));
                } else {
                    push_log(&app2, &format!("[download] error: {}", e));
                    let _ = app2.emit("download-error", e);
                }
            }
        }
    });

    Ok(DownloadInfo {
        url,
        dest: dest.to_string_lossy().into(),
        resumed_from,
        total: 0,
    })
}

#[tauri::command]
pub fn pause_download(app: AppHandle) -> Result<(), AppError> {
    let state: State<DownloadState> = app.state();
    // 暂停与取消共用 cancel 停掉工作线程，但用 paused 区分事件：暂停不进失败页
    state.paused.store(true, Ordering::SeqCst);
    state.cancel.store(true, Ordering::SeqCst);
    push_log(&app, "[download] paused by user");
    Ok(())
}

#[tauri::command]
pub fn cancel_download(app: AppHandle) -> Result<(), AppError> {
    let state: State<DownloadState> = app.state();
    state.paused.store(false, Ordering::SeqCst);
    state.cancel.store(true, Ordering::SeqCst);
    // 给工作线程一点时间退出，避免删文件中
    std::thread::sleep(std::time::Duration::from_millis(300));
    if let Ok(p) = msix_path(&app) {
        let _ = std::fs::remove_file(p.with_extension("part"));
        let _ = std::fs::remove_file(p.with_extension("etag"));
    }
    push_log(&app, "[download] cancelled, part removed");
    Ok(())
}

#[tauri::command]
pub async fn verify_package(app: AppHandle) -> Result<VerifyInfo, AppError> {
    blocking(move || {
        let path = msix_path(&app)?;
        push_log(&app, &format!("[verify] {}", path.display()));
        let info = run_verify_package(&path)?;
        push_log(
            &app,
            &format!(
                "[verify] ok: {} {} ({} bytes)",
                info.identity, info.version, info.size
            ),
        );
        Ok(info)
    })
    .await
}

/// 安装成功后清理安装包文件（800MB+），只保留设置文件。
/// 卸载/重装都不需要这些文件，重装会重新下载。
fn cleanup_package_files(app: &AppHandle, msix: &std::path::Path) {
    for p in [
        msix.to_path_buf(),
        msix.with_extension("part"),
        msix.with_extension("etag"),
    ] {
        let _ = std::fs::remove_file(&p);
    }
    push_log(app, "[install] package files cleaned");
}

/// 安装成功后记录包 ETag 与版本，供“检查更新”对比云端。
/// 失败不致命，只影响下次检查更新的准确性。
fn record_install(app: &AppHandle, msix: &std::path::Path, version: &str) {
    let etag = std::fs::read_to_string(msix.with_extension("etag"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    crate::settings::save_install_record(app, etag.as_deref(), version);
    push_log(app, &format!("[install] recorded version {}", version));
}

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

/// 云端 MSIX 的 HEAD 信息：官方版本号 + ETag。
/// 版本号来自 Azure Blob 自定义元数据 `x-ms-meta-package_version`，
/// 由官方发布流水线写入（x64/arm64 实测一致）。
struct CdnInfo {
    version: Option<String>,
    etag: Option<String>,
}

fn head_cdn_info(url: &str) -> Option<CdnInfo> {
    let client = reqwest::blocking::Client::builder()
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
fn cmp_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parse = |s: &str| -> Option<Vec<u64>> {
        s.split('.').map(|p| p.parse::<u64>().ok()).collect()
    };
    let mut va = parse(a)?;
    let mut vb = parse(b)?;
    let n = va.len().max(vb.len());
    va.resize(n, 0);
    vb.resize(n, 0);
    Some(va.cmp(&vb))
}

fn run_check_update(app: &AppHandle) -> Result<UpdateCheck, AppError> {
    let installed_version = installed_app_version();
    let Some(ver) = installed_version else {
        return Ok(UpdateCheck {
            installed: false,
            installed_version: None,
            latest_version: None,
            has_update: false,
            note: "本机未安装 ChatGPT，直接点击「开始安装」即可装上最新版。".into(),
        });
    };
    let record = crate::settings::load_install_record(app);
    // 版本漂移检测：本机当前版本 vs 本工具上次安装记录的版本。
    // 微软商店自动更新等其他渠道会改变本机版本，我们控制不了，但能识别出来并如实告知。
    let drift_note: Option<String> = record
        .as_ref()
        .and_then(|r| r.version.clone())
        .filter(|rv| rv != &ver)
        .map(|rv| {
            format!(
                "检测到本机 ChatGPT 已变为 v{ver}（本工具上次安装的是 v{rv}），可能是通过微软商店等其他渠道更新。"
            )
        });
    // 每次检查都先请求 CDN 拿官方版本元数据，不依赖历史 ETag。
    // v1.0.0 老用户没有 ETag 记录，也能用 x-ms-meta-package_version 做精确版本对比。
    let url = msix_url_for_arch(std::env::consts::ARCH).ok_or_else(|| {
        AppError::new(
            "NO_PACKAGE",
            "不支持的 CPU 架构",
            "当前架构没有对应的官方安装包。",
        )
    })?;
    // 有版本漂移时，把提示拼在检查结果前面
    let prefix = drift_note.map(|d| format!("{d} ")).unwrap_or_default();
    let info = match head_cdn_info(url) {
        Some(i) => i,
        None => {
            return Ok(UpdateCheck {
                installed: true,
                installed_version: Some(ver.clone()),
                latest_version: None,
                has_update: false,
                note: format!(
                    "{prefix}本机版本 v{ver}；网络异常，无法获取云端版本信息，请稍后重试。"
                ),
            })
        }
    };
    // 优先用官方版本号做精确对比
    if let Some(cdn_ver) = info.version {
        let (has_update, note) = match cmp_versions(&cdn_ver, &ver) {
            Some(std::cmp::Ordering::Greater) => (
                true,
                format!(
                    "{prefix}发现新版本：云端 v{cdn_ver}，本机 v{ver}。点击「开始更新」重新下载安装（约 800MB，支持断点续传）。"
                ),
            ),
            Some(std::cmp::Ordering::Less) => (
                false,
                format!("{prefix}本机 v{ver} 比云端 v{cdn_ver} 更新，无需操作。"),
            ),
            _ => (
                false,
                format!("{prefix}已是最新版本（v{ver}），无需更新。"),
            ),
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
    match (info.etag, stored_etag) {
        (Some(cur), Some(stored)) if cur == stored => Ok(UpdateCheck {
            installed: true,
            installed_version: Some(ver.clone()),
            latest_version: None,
            has_update: false,
            note: format!("{prefix}云端安装包无变化，本机 v{ver} 已是最新。"),
        }),
        (Some(_), Some(_)) => Ok(UpdateCheck {
            installed: true,
            installed_version: Some(ver),
            latest_version: None,
            has_update: true,
            note: format!(
                "{prefix}云端安装包有变化，点击「开始更新」重新下载安装（约 800MB，支持断点续传）。"
            ),
        }),
        _ => Ok(UpdateCheck {
            installed: true,
            installed_version: Some(ver.clone()),
            latest_version: None,
            has_update: false,
            note: format!(
                "{prefix}本机已安装 v{ver}，但缺少上次安装的云端标识、云端也未返回版本信息，无法判断是否有新版。如需确保最新，可直接重新运行本工具覆盖安装。"
            ),
        }),
    }
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
        install_for_user(app, &msix)?;
        emit_stage(app, 2, "正在为所有用户预置…");
        let dir = pkg_dir(app)?;
        let license = ensure_license(app, &dir)?;
        for w in provision_all_users(&msix, &license) {
            let _ = app.emit("install-warning", w);
        }
        emit_stage(app, 3, "安装完成");
        push_log(app, "[install] success (elevated)");
        record_install(app, &msix, &info.version);
        cleanup_package_files(app, &msix);
        return Ok(());
    }

    // 未提权：先尝试直接为当前用户安装（通常无需管理员权限）
    emit_stage(app, 1, "正在为当前用户注册应用包…");
    match install_for_user(app, &msix) {
        Ok(()) => {
            emit_stage(app, 3, "安装完成");
            push_log(app, "[install] success (per-user, no elevation)");
            record_install(app, &msix, &info.version);
            cleanup_package_files(app, &msix);
            Ok(())
        }
        Err(e) if e.is_elevation_error() => {
            push_log(app, "[install] per-user install needs elevation");
            Err(AppError::need_elevation())
        }
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub async fn install_package(app: AppHandle) -> Result<(), AppError> {
    blocking(move || install_flow(&app)).await
}

/// 提权重启。action: "install" | "uninstall" | "uninstall-data"。
/// 会把当前用户的安装包目录透传给提权后的进程，避免用户上下文切换导致找不到包。
#[tauri::command]
pub fn restart_elevated_cmd(app: AppHandle, action: String) -> Result<(), AppError> {
    #[cfg(windows)]
    {
        let dir = pkg_dir(&app)?;
        let mut args = vec![
            format!("--resume={}", action),
            format!("--pkg-dir={}", dir.display()),
            // 提权成功后，新进程凭此结束掉当前（未提权的）进程；
            // 用户在 UAC 点"否"时不会有新进程，当前进程继续存活
            format!("--parent-pid={}", std::process::id()),
        ];
        // 原用户的 LOCALAPPDATA：用另一管理员账号提权时进程用户会变化，
        // 设置/安装记录/数据清理必须作用回原用户，不能跟着管理员账号走
        if let Ok(dd) = std::env::var("LOCALAPPDATA") {
            if !dd.is_empty() {
                args.push(format!("--data-dir={dd}"));
            }
        }
        restart_elevated(&args);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (app, action);
        Err(AppError::new(
            "UNSUPPORTED_OS",
            "提权重启仅支持 Windows",
            "",
        ))
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
        push_log(&app, "[uninstall] begin");
        let warnings = uninstall_package(&app, remove_data)?;
        push_log(&app, "[uninstall] done");
        Ok(warnings)
    })
    .await
}

#[tauri::command]
pub fn launch_app() -> Result<(), AppError> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(format!("shell:AppsFolder\\{}", APP_AUMID))
            .spawn()
            .map_err(|e| AppError::new("LAUNCH_FAILED", "无法启动 ChatGPT", e.to_string()))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(AppError::new("UNSUPPORTED_OS", "启动仅支持 Windows", ""))
    }
}

#[tauri::command]
pub fn get_last_log(app: AppHandle) -> Result<String, AppError> {
    let state: State<LogState> = app.state();
    Ok(state.0.lock().map(|l| l.clone()).unwrap_or_default())
}

/// 退出程序：安装成功页的「完成」按钮 / 自动关闭倒计时。
#[tauri::command]
pub fn exit_app(app: AppHandle) {
    push_log(&app, "[app] exit");
    app.exit(0);
}
