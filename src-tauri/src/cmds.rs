//! Tauri 命令：前端 IPC 入口。
//!
//! 线程模型（Tauri 2）：同步命令在主线程执行，会阻塞界面。
//! 因此所有耗时操作（网络、PowerShell、文件 IO）都走
//! `async` 命令 + `spawn_blocking`，把阻塞工作移到后台线程。
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, State};

use crate::constants::*;
use crate::download::{self, DownloadInfo, DownloadState};
use crate::elevate::is_admin;
#[cfg(windows)]
use crate::elevate::restart_elevated;
use crate::env_check::{check_env as run_env_check, EnvItem};
use crate::errors::AppError;
use crate::install::{
    emit_stage, ensure_license, install_for_user, provision_all_users, uninstall_package,
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
pub async fn check_env() -> Result<Vec<EnvItem>, AppError> {
    blocking(|| Ok(run_env_check())).await
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

    let (url, dest) = download::resolve_target(&app)?;
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
                push_log(&app2, &format!("[download] error: {}", e));
                let _ = app2.emit("download-error", e);
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
    state.cancel.store(true, Ordering::SeqCst);
    push_log(&app, "[download] paused by user");
    Ok(())
}

#[tauri::command]
pub fn cancel_download(app: AppHandle) -> Result<(), AppError> {
    let state: State<DownloadState> = app.state();
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
        return Ok(());
    }

    // 未提权：先尝试直接为当前用户安装（通常无需管理员权限）
    emit_stage(app, 1, "正在为当前用户注册应用包…");
    match install_for_user(app, &msix) {
        Ok(()) => {
            emit_stage(app, 3, "安装完成");
            push_log(app, "[install] success (per-user, no elevation)");
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
        restart_elevated(&[
            &format!("--resume={}", action),
            &format!("--pkg-dir={}", dir.display()),
        ])
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
