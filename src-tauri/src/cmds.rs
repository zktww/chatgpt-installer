//! Tauri 命令：前端 IPC 入口。同步命令运行在 Tauri 工作线程池，
//! 阻塞式调用（如 PowerShell）不会卡住 UI。
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, State};

use crate::constants::*;
use crate::download::{self, DownloadInfo, DownloadState};
use crate::elevate::is_admin;
#[cfg(windows)]
use crate::elevate::restart_elevated;
use crate::env_check::{check_env, EnvItem};
use crate::errors::AppError;
use crate::install::{
    ensure_license, install_package as run_install, uninstall_package as run_uninstall,
};
use crate::verify::{verify_package, VerifyInfo};

/// 内存日志：记录关键步骤，供「复制诊断日志」使用。
#[derive(Default)]
pub struct LogState(pub Mutex<String>);

fn push_log(app: &AppHandle, line: &str) {
    if let Ok(state) = app.try_state::<LogState>() {
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

fn data_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| AppError::new("IO_ERROR", "无法确定数据目录", e.to_string()))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::new("IO_ERROR", "无法创建数据目录", e.to_string()))?;
    Ok(dir)
}

fn msix_path(app: &AppHandle) -> Result<PathBuf, AppError> {
    let arch = std::env::consts::ARCH;
    let filename = msix_filename_for_arch(arch)
        .ok_or_else(|| AppError::new("UNSUPPORTED_ARCH", "不支持的 CPU 架构", arch))?;
    Ok(data_dir(app)?.join(filename))
}

#[tauri::command]
pub fn check_env() -> Result<Vec<EnvItem>, AppError> {
    Ok(check_env())
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
    let app2 = app.clone();
    std::thread::spawn(move || {
        let result = download::run_download(&app2, &url, &dest);
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
    }
    push_log(&app, "[download] cancelled, part removed");
    Ok(())
}

#[tauri::command]
pub fn verify_package(app: AppHandle) -> Result<VerifyInfo, AppError> {
    let path = msix_path(&app)?;
    push_log(&app, &format!("[verify] {}", path.display()));
    let info = verify_package(&path)?;
    push_log(
        &app,
        &format!(
            "[verify] ok: {} {} ({} bytes)",
            info.identity, info.version, info.size
        ),
    );
    Ok(info)
}

#[tauri::command]
pub fn install_package(app: AppHandle) -> Result<(), AppError> {
    if !is_admin() {
        push_log(&app, "[install] not elevated");
        return Err(AppError::need_elevation());
    }
    let msix = msix_path(&app)?;
    if !msix.exists() {
        return Err(AppError::new(
            "NO_PACKAGE",
            "找不到安装包",
            "请先完成下载步骤。",
        ));
    }
    let dir = data_dir(&app)?;
    push_log(&app, "[install] begin");
    let license = ensure_license(&app, &dir)?;
    run_install(&app, &msix, &license)?;
    push_log(&app, "[install] success");
    Ok(())
}

/// 提权重启（当前进程退出）。调用后前端无需处理返回值。
#[tauri::command]
pub fn restart_elevated_cmd() -> Result<(), AppError> {
    #[cfg(windows)]
    {
        restart_elevated(&["--resume-install"])
    }
    #[cfg(not(windows))]
    {
        return Err(AppError::new(
            "UNSUPPORTED_OS",
            "提权重启仅支持 Windows",
            "",
        ));
    }
}

#[tauri::command]
pub fn uninstall_package_cmd(app: AppHandle, remove_data: bool) -> Result<Vec<String>, AppError> {
    if !is_admin() {
        return Err(AppError::need_elevation());
    }
    push_log(&app, "[uninstall] begin");
    let warnings = run_uninstall(&app, remove_data)?;
    push_log(&app, "[uninstall] done");
    Ok(warnings)
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
