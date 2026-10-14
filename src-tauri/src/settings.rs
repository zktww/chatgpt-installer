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

/// 读取整个 settings.json；不存在或解析失败返回空对象。
fn read_settings(app: &AppHandle) -> serde_json::Value {
    let Ok(file) = settings_file(app) else {
        return serde_json::json!({});
    };
    let Ok(raw) = std::fs::read_to_string(file) else {
        return serde_json::json!({});
    };
    serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
}

fn write_settings(app: &AppHandle, v: &serde_json::Value) -> Result<(), AppError> {
    let file = settings_file(app)?;
    std::fs::write(&file, serde_json::to_string_pretty(v).unwrap())
        .map_err(|e| AppError::new("IO_ERROR", "无法保存设置", e.to_string()))
}

/// 读取用户自定义的下载目录；未设置或目录已不存在时返回 None（调用方回退默认）。
pub fn load_custom_dir(app: &AppHandle) -> Option<PathBuf> {
    let v = read_settings(app);
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
    let mut v = read_settings(app);
    v["pkg_dir"] = serde_json::json!(path.to_string_lossy());
    write_settings(app, &v)?;
    Ok(path)
}

/// 上次成功安装的记录：用于“检查更新”时对比云端 ETag。
pub struct InstallRecord {
    pub etag: Option<String>,
    pub version: Option<String>,
}

pub fn load_install_record(app: &AppHandle) -> Option<InstallRecord> {
    let v = read_settings(app);
    let etag = v
        .get("installed_etag")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string());
    let version = v
        .get("installed_version")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string());
    if etag.is_none() && version.is_none() {
        return None;
    }
    Some(InstallRecord { etag, version })
}

/// 保存安装记录；失败不致命（只影响下次检查更新的准确性），调用方忽略错误。
pub fn save_install_record(app: &AppHandle, etag: Option<&str>, version: &str) {
    let mut v = read_settings(app);
    if let Some(e) = etag {
        v["installed_etag"] = serde_json::json!(e);
    }
    v["installed_version"] = serde_json::json!(version);
    let _ = write_settings(app, &v);
}
