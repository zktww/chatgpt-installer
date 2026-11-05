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
mod ps;
mod settings;
mod sys;
mod verify;

use std::path::PathBuf;

use cmds::*;
use download::DownloadState;
use tauri::Manager;

fn main() {
    // 提权重启透参：
    //   --resume=install | uninstall | uninstall-data
    //   --pkg-dir=<原用户安装包目录>（标准用户用另一管理员账号提权时，进程用户会变化）
    //   --data-dir=<原用户 LOCALAPPDATA>（同上，设置与安装记录写回原用户位置）
    let mut resume_action: Option<String> = None;
    let mut pkg_dir: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--resume=") {
            resume_action = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--pkg-dir=") {
            if !v.is_empty() {
                pkg_dir = Some(PathBuf::from(v));
            }
        } else if let Some(v) = arg.strip_prefix("--data-dir=") {
            if !v.is_empty() {
                data_dir = Some(PathBuf::from(v));
            }
        }
    }

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default();

    // 单实例：再次双击时聚焦已有窗口，避免两个进程同时写同一个下载分片。
    // 提权重启出来的实例（带 --resume）不注册：它启动时旧进程可能还没退出，
    // 注册的话会被当成“第二个实例”直接退出，提权就白做了。
    #[cfg(windows)]
    {
        if resume_action.is_none() {
            builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }));
        }
    }

    builder
        .plugin(tauri_plugin_dialog::init())
        .manage(DownloadState::default())
        .manage(LogState::default())
        .manage(AppCtx::default())
        .invoke_handler(tauri::generate_handler![
            app_info,
            take_resume_action,
            check_env,
            start_download,
            pause_download,
            discard_package,
            verify_package,
            install_package,
            restart_elevated_cmd,
            uninstall_package_cmd,
            launch_app,
            get_last_log,
            get_pkg_dir,
            set_pkg_dir,
            pick_pkg_dir,
            exit_app,
            check_update,
        ])
        .setup(move |app| {
            if let Some(ctx) = app.try_state::<AppCtx>() {
                // 把透传的包目录 / 原用户数据目录存入上下文，后续命令优先使用
                if let Some(dir) = pkg_dir {
                    if let Ok(mut guard) = ctx.pkg_dir_override.lock() {
                        *guard = Some(dir);
                    }
                }
                if let Some(dir) = data_dir {
                    if let Ok(mut guard) = ctx.data_dir_override.lock() {
                        *guard = Some(dir);
                    }
                }
                // 续跑动作由前端加载完成后通过 take_resume_action 主动领取
                if let Some(action) = resume_action.clone() {
                    if let Ok(mut guard) = ctx.resume_action.lock() {
                        *guard = Some(action);
                    }
                }
            }
            let handle = app.handle().clone();
            push_log(&handle, &format!("[app] start, resume={:?}", resume_action));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("ChatGPT 安装助手启动失败");
}
