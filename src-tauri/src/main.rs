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
mod verify;

use cmds::*;
use download::DownloadState;
use tauri::Emitter;

fn main() {
    // 提权重启后自动回到安装步骤
    let resume_install = std::env::args().any(|a| a == "--resume-install");

    tauri::Builder::default()
        .manage(DownloadState::default())
        .manage(LogState::default())
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
        ])
        .setup(move |app| {
            if resume_install {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    // 等前端就绪后再通知跳转
                    std::thread::sleep(std::time::Duration::from_millis(900));
                    let _ = handle.emit("resume-install", ());
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("ChatGPT 安装助手启动失败");
}
