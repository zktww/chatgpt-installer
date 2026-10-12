//! 用户设置：下载目录自定义。
//!
//! 设置文件（settings.json）固定放在默认应用数据目录，
//! 不跟随自定义目录走，避免“搬家后找不到设置”。

use std::path::PathBuf;

use tauri::{AppHandle, Manager};

use crate::errors::AppError;

fn default_data_dir(app: &AppHandle) -> Result<PathBuf, AppError> {
    let dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| AppError::new("IO_ERROR", "无法确定数据目录", e.to_string()))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::new("IO_ERROR", "无法创建数据目录", e.to_string()))?;
    Ok(dir)
}

fn settings_file(app: &AppHandle) -> Result<PathBuf, AppError> {
    Ok(default_data_dir(app)?.join("settings.json"))
}

/// 读取用户自定义的下载目录；未设置或目录已不存在时返回 None（调用方回退默认）。
pub fn load_custom_dir(app: &AppHandle) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(settings_file(app).ok()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let dir = v.get("pkg_dir")?.as_str()?.trim();
    if dir.is_empty() {
        return None;
    }
    let p = PathBuf::from(dir);
    if p.is_absolute() && p.exists() {
        Some(p)
    } else {
        None
    }
}

/// 校验并保存用户自定义下载目录，返回保存的路径。
pub fn save_custom_dir(app: &AppHandle, raw: &str) -> Result<PathBuf, AppError> {
    let dir = raw.trim();
    if dir.is_empty() {
        return Err(AppError::new(
            "BAD_DIR",
            "目录不能为空",
            "请输入一个有效的目录路径。",
        ));
    }
    let path = PathBuf::from(dir);
    if !path.is_absolute() {
        return Err(AppError::new(
            "BAD_DIR",
            "请填写绝对路径",
            "例如 D:\\Downloads",
        ));
    }
    std::fs::create_dir_all(&path).map_err(|e| {
        AppError::new(
            "BAD_DIR",
            "无法创建目录",
            format!("{}：{}", path.display(), e),
        )
    })?;
    // 可写性探测：写一个临时文件再删掉
    let probe = path.join(".writetest");
    std::fs::write(&probe, b"ok")
        .and_then(|_| std::fs::remove_file(&probe))
        .map_err(|e| {
            AppError::new(
                "BAD_DIR",
                "目录不可写",
                format!("{}：{}", path.display(), e),
            )
        })?;
    let file = settings_file(app)?;
    let json = serde_json::json!({ "pkg_dir": path.to_string_lossy() });
    std::fs::write(&file, serde_json::to_string_pretty(&json).unwrap())
        .map_err(|e| AppError::new("IO_ERROR", "无法保存设置", e.to_string()))?;
    Ok(path)
}
