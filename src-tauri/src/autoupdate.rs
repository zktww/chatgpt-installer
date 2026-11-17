//! 可选的定时自动更新（默认关闭，由用户在欢迎页选择）。
//!
//! - 开启时：把本程序复制到固定位置（`<数据目录>\bin\`），再用「任务计划程序」
//!   为**当前用户**注册每日任务（普通权限、仅登录时运行、错过时间开机后补跑）；
//! - 到点以 `--auto-update` 启动：窗口保持隐藏，只检查官方版本；
//!   - notify：有新版才弹出本窗口，由用户决定；
//!   - auto：后台下载 → 校验 → 为当前用户安装；ChatGPT 正在运行、需要提权或出错时
//!     不强行处理，改为弹窗提醒；
//! - 关闭时：删除计划任务与复制的程序。
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::cmds::{install_flow, pkg_dir, prepare_download, push_log, run_check_update};
use crate::constants::REQUIRED_DISK_BYTES;
use crate::download::{self, DownloadState};
use crate::errors::AppError;

/// 计划任务启动本程序时携带的参数
pub const ARG: &str = "--auto-update";
/// 计划任务名（纯 ASCII：NSIS 卸载钩子里也要用到，避免编码问题）
pub const TASK_NAME: &str = "ChatGPT Installer AutoUpdate";
/// 每日检查时间（本地时间）；错过时由 StartWhenAvailable 开机后补跑
const DAILY_AT: &str = "12:00:00";

/// 自动模式下界面是否已经弹出（弹出后不再自动退出进程）
static UI_SHOWN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize)]
pub struct AutoUpdateInfo {
    /// "off" | "notify" | "auto"
    pub mode: String,
    /// 计划任务是否确实存在（模式非 off 但任务被手动删掉时为 false）
    pub task_ok: bool,
}

/* ---------------- 进程内状态 ---------------- */

/// 弹出主窗口（自动模式下需要用户介入时调用；单实例回调也会调用）。
pub fn show_main(app: &AppHandle) {
    UI_SHOWN.store(true, Ordering::SeqCst);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    // 让前端刷新欢迎页的版本状态
    let _ = app.emit("auto-update-show", serde_json::json!({}));
}

fn finish_silently(app: &AppHandle, reason: &str) {
    push_log(app, &format!("[auto] {reason}"));
    if !UI_SHOWN.load(Ordering::SeqCst) {
        app.exit(0);
    }
}

/* ---------------- 计划任务 ---------------- */

fn bin_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(crate::settings::default_data_dir(app)?.join("bin"))
}

/// 把当前程序复制到固定位置，返回计划任务要执行的路径。
/// 便携版可能被用户挪走或删除，所以不能直接用当前路径。
fn install_self_copy(app: &AppHandle) -> Result<PathBuf, AppError> {
    let dir = bin_dir(app)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::new("IO_ERROR", "无法创建程序目录", e.to_string()))?;
    let target = dir.join("chatgpt-installer.exe");
    let marker = dir.join("version.txt");
    let cur = std::env::current_exe()
        .map_err(|e| AppError::new("IO_ERROR", "无法定位当前程序", e.to_string()))?;
    let same_file = std::fs::canonicalize(&cur).ok() == std::fs::canonicalize(&target).ok();
    let up_to_date = target.exists()
        && std::fs::read_to_string(&marker).ok().as_deref() == Some(env!("CARGO_PKG_VERSION"));
    if !same_file && !up_to_date {
        if let Err(e) = std::fs::copy(&cur, &target) {
            // 正在运行的副本无法覆盖：旧副本仍可用，下次再刷新
            if !target.exists() {
                return Err(AppError::new("IO_ERROR", "无法复制程序", e.to_string()));
            }
            push_log(app, &format!("[auto] self copy skipped: {e}"));
        } else {
            let _ = std::fs::write(&marker, env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(target)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 生成计划任务 XML（Task Scheduler 1.2 架构）。
fn task_xml(exe: &Path, user: Option<&str>) -> String {
    let user_id = user
        .map(|u| format!("<UserId>{}</UserId>", xml_escape(u)))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>每天检查 OpenAI 官方 ChatGPT 桌面版更新（ChatGPT 安装助手，可在程序欢迎页关闭）</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2026-01-01T{at}</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">{user_id}<LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>true</RunOnlyIfNetworkAvailable>
    <ExecutionTimeLimit>PT4H</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{arg}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        at = DAILY_AT,
        user_id = user_id,
        exe = xml_escape(&exe.to_string_lossy()),
        arg = ARG,
    )
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<std::process::Output, AppError> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(crate::ps::NO_WINDOW)
        .output()
        .map_err(|e| AppError::new("TASK_FAILED", "无法调用任务计划程序", e.to_string()))
}

#[cfg(not(windows))]
fn schtasks(_args: &[&str]) -> Result<std::process::Output, AppError> {
    Err(AppError::new("UNSUPPORTED_OS", "仅支持 Windows", ""))
}

fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn create_task(app: &AppHandle, exe: &Path) -> Result<(), AppError> {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) if !d.is_empty() && !u.is_empty() => Some(format!("{d}\\{u}")),
        _ => None,
    };
    let xml = task_xml(exe, user.as_deref());
    // schtasks /XML 要求 UTF-16 LE（带 BOM）
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let file = bin_dir(app)?.join("autoupdate-task.xml");
    std::fs::write(&file, bytes)
        .map_err(|e| AppError::new("IO_ERROR", "无法写入任务配置", e.to_string()))?;
    let file_s = file.to_string_lossy().into_owned();
    let out = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &file_s, "/F"])?;
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        return Err(AppError::new(
            "TASK_FAILED",
            "创建计划任务失败",
            crate::ps::snippet(&out, 300),
        ));
    }
    Ok(())
}

fn delete_task() {
    let _ = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]);
}

/* ---------------- 命令 ---------------- */

pub fn get(app: &AppHandle) -> AutoUpdateInfo {
    let mode = crate::settings::load_auto_update(app);
    let task_ok = mode != "off" && task_exists();
    AutoUpdateInfo { mode, task_ok }
}

pub fn set(app: &AppHandle, mode: &str) -> Result<AutoUpdateInfo, AppError> {
    if !matches!(mode, "off" | "notify" | "auto") {
        return Err(AppError::new("BAD_MODE", "未知的自动更新模式", mode));
    }
    if mode == "off" {
        delete_task();
        if let Ok(dir) = bin_dir(app) {
            // 副本正在运行时删不掉，不影响：任务已删除，不会再被启动
            let _ = std::fs::remove_dir_all(dir);
        }
    } else {
        let exe = install_self_copy(app)?;
        create_task(app, &exe)?;
    }
    crate::settings::save_auto_update(app, mode)?;
    push_log(app, &format!("[auto] mode -> {mode}"));
    Ok(get(app))
}

/// 正常（有界面）启动时：已开启自动更新则刷新程序副本、补建被删掉的任务。
pub fn refresh_on_startup(app: &AppHandle) {
    if crate::settings::load_auto_update(app) == "off" {
        return;
    }
    match install_self_copy(app) {
        Ok(exe) if !task_exists() => {
            if let Err(e) = create_task(app, &exe) {
                push_log(app, &format!("[auto] re-create task failed: {e}"));
            }
        }
        Ok(_) => {}
        Err(e) => push_log(app, &format!("[auto] refresh self copy failed: {e}")),
    }
}

/* ---------------- 计划任务触发的后台流程 ---------------- */

/// ChatGPT 是否正在运行（自动安装前检查，避免强制关掉用户正在用的窗口）。
#[cfg(windows)]
fn chatgpt_running() -> bool {
    crate::ps::text(
        "@(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*\\WindowsApps\\OpenAI.Codex_*' }).Count",
    )
    .ok()
    .and_then(|s| s.trim().parse::<u32>().ok())
    .map(|n| n > 0)
    // 判断不了时按"正在运行"处理，宁可提醒也不强关
    .unwrap_or(true)
}

#[cfg(not(windows))]
fn chatgpt_running() -> bool {
    true
}

/// 计划任务触发：在后台线程中调用，窗口保持隐藏。
pub fn run(app: &AppHandle) {
    let mode = crate::settings::load_auto_update(app);
    push_log(app, &format!("[auto] triggered, mode={mode}"));
    if mode == "off" {
        // 设置已关闭但任务残留：顺手清理
        delete_task();
        return finish_silently(app, "disabled, task removed");
    }

    let check = match run_check_update(app) {
        Ok(c) => c,
        Err(e) => return finish_silently(app, &format!("check failed: {e}")),
    };
    if !check.installed {
        return finish_silently(app, "ChatGPT not installed, nothing to do");
    }
    if !check.has_update {
        return finish_silently(app, &format!("up to date: {}", check.note));
    }
    push_log(app, &format!("[auto] {}", check.note));

    if mode == "notify" {
        return show_main(app);
    }

    // auto：后台下载并安装
    if chatgpt_running() {
        push_log(
            app,
            "[auto] ChatGPT is running, ask user instead of force closing",
        );
        return show_main(app);
    }
    if let Ok(dir) = pkg_dir(app) {
        if crate::sys::free_bytes(&dir).is_some_and(|free| free < REQUIRED_DISK_BYTES) {
            push_log(app, "[auto] not enough disk space");
            return show_main(app);
        }
    }

    let state: State<DownloadState> = app.state();
    if state.running.swap(true, Ordering::SeqCst) {
        return finish_silently(app, "a download is already running");
    }
    state.cancel.store(false, Ordering::SeqCst);
    state.paused.store(false, Ordering::SeqCst);
    let downloaded = prepare_download(app).and_then(|(url, dest, complete)| {
        if complete.is_none() {
            push_log(app, &format!("[auto] downloading {url}"));
            download::run_download(app, &url, &dest)?;
        }
        Ok(())
    });
    state.running.store(false, Ordering::SeqCst);
    if let Err(e) = downloaded {
        // 分片保留，下次（或用户手动）可续传
        return finish_silently(app, &format!("download failed: {e} {}", e.hint));
    }

    match install_flow(app) {
        Ok(()) => finish_silently(app, "updated successfully"),
        Err(e) => {
            push_log(app, &format!("[auto] install failed: {e} {}", e.hint));
            show_main(app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_escapes_path_and_has_arg() {
        let xml = task_xml(Path::new(r"C:\A&B\x.exe"), Some(r"PC\me"));
        assert!(xml.contains(r"<Command>C:\A&amp;B\x.exe</Command>"));
        assert!(xml.contains("<Arguments>--auto-update</Arguments>"));
        assert!(xml.contains(r"<UserId>PC\me</UserId>"));
        assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
    }
}
