//! ChatGPT 安装助手 — Tauri 入口。
//!
//! 纯客户端：所有下载直连 OpenAI 官方 CDN，校验与安装均在本地完成，
//! 本程序不设服务端、不收集任何数据。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod cmds;
mod constants;
mod download;
mod elevate;
mod env_check;
mod errors;
mod install;
mod settings;
mod verify;

use std::path::PathBuf;
use std::time::Duration;

use cmds::*;
use download::DownloadState;
use tauri::{Emitter, Manager};

fn main() {
    // 提权重启透参：
    //   --resume=install | uninstall | uninstall-data
    //   --pkg-dir=<原用户安装包目录>（标准用户用另一管理员账号提权时，进程用户会变化）
    //   --parent-pid=<未提权的父进程 PID>（提权成功后由新进程结束掉它；UAC 点"否"则无新进程，父进程继续存活）
    //   --data-dir=<原用户 LOCALAPPDATA>（同上，设置与安装记录写回原用户位置）
    let mut resume_action: Option<String> = None;
    let mut pkg_dir: Option<PathBuf> = None;
    let mut parent_pid: Option<u32> = None;
    let mut data_dir: Option<PathBuf> = None;
    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--resume=") {
            resume_action = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--pkg-dir=") {
            pkg_dir = Some(PathBuf::from(v));
        } else if let Some(v) = arg.strip_prefix("--parent-pid=") {
            parent_pid = v.parse().ok();
        } else if let Some(v) = arg.strip_prefix("--data-dir=") {
            if !v.is_empty() {
                data_dir = Some(PathBuf::from(v));
            }
        }
    }

    tauri::Builder::default()
        .manage(DownloadState::default())
        .manage(LogState::default())
        .manage(AppCtx::default())
        .invoke_handler(tauri::generate_handler![
            check_env,
            start_download,
            pause_download,
            cancel_download,
            verify_package,
            install_package,
            restart_elevated_cmd,
            uninstall_package_cmd,
            launch_app,
            get_last_log,
            get_pkg_dir,
            set_pkg_dir,
            exit_app,
            check_update,
        ])
        .setup(move |app| {
            // 把透传的包目录存入上下文，后续命令优先使用它
            if let Some(dir) = pkg_dir {
                if let Some(ctx) = app.try_state::<AppCtx>() {
                    if let Ok(mut guard) = ctx.pkg_dir_override.lock() {
                        *guard = Some(dir);
                    }
                }
            }
            // 同理透传原用户数据目录（跨用户提权场景）
            if let Some(dir) = data_dir {
                if let Some(ctx) = app.try_state::<AppCtx>() {
                    if let Ok(mut guard) = ctx.data_dir_override.lock() {
                        *guard = Some(dir);
                    }
                }
            }
            // 提权成功后来到这里：结束掉未提权的父进程，避免双实例。
            // （UAC 点"否"时不会有新进程，父进程继续存活，可重试）
            if let Some(pid) = parent_pid {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        let _ = std::process::Command::new("taskkill")
                            .args(["/PID", &pid.to_string(), "/F"])
                            .creation_flags(0x08000000)
                            .output();
                    }
                    #[cfg(not(windows))]
                    let _ = pid;
                });
            }
            // 等前端就绪后派发提权后的续跑动作
            if let Some(action) = resume_action {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(900));
                    let _ = handle.emit("resume-action", serde_json::json!({ "action": action }));
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("ChatGPT 安装助手启动失败");
}
