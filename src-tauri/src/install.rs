//! 安装 / 卸载：调用系统 PowerShell 的 AppX cmdlets。
//!
//! v1 采用社区验证过的可靠路线（Add-AppxPackage），进度以阶段事件呈现。
//! TODO(v2): 迁移到 PackageManager 原生 API，以获得真实百分比进度。
use std::path::Path;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::constants::LICENSE_URL;
use crate::download::download_small_file;
use crate::errors::AppError;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Debug, Clone, Serialize)]
pub struct InstallStage {
    /// 0=准备，1=为当前用户注册，2=为所有用户预置，3=完成
    pub stage: u8,
    pub text: String,
}

pub fn emit_stage(app: &AppHandle, stage: u8, text: &str) {
    let _ = app.emit(
        "install-progress",
        InstallStage {
            stage,
            text: text.to_string(),
        },
    );
}

/// 确保离线授权文件存在（整机预置需要）。
pub fn ensure_license(app: &AppHandle, dir: &Path) -> Result<std::path::PathBuf, AppError> {
    let lic = dir.join("ChatGPT-License.xml");
    if !lic.exists() {
        emit_stage(app, 0, "正在获取离线授权文件…");
        download_small_file(LICENSE_URL, &lic)?;
    }
    Ok(lic)
}

/// 为当前用户注册应用包（一般不需要管理员权限）。
/// 返回的错误已经过 from_deploy_output 映射，可直接展示。
#[cfg(windows)]
pub fn install_for_user(app: &AppHandle, msix: &Path) -> Result<(), AppError> {
    let ps = format!(
        "Add-AppxPackage -Path '{}' -ForceTargetApplicationShutdown",
        ps_escape(&msix.to_string_lossy())
    );
    let out = run_ps(&ps)?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr).to_string()
            + &String::from_utf8_lossy(&out.stdout);
        return Err(AppError::from_deploy_output(&text));
    }
    let _ = app;
    Ok(())
}

/// 整机预置（需要管理员权限）。失败只返回警告文本，不中断主流程——
/// 当前用户注册已成功的话，应用已经可用。
#[cfg(windows)]
pub fn provision_all_users(msix: &Path, license: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let ps = format!(
        "Add-AppxProvisionedPackage -Online -PackagePath '{}' -LicensePath '{}'",
        ps_escape(&msix.to_string_lossy()),
        ps_escape(&license.to_string_lossy())
    );
    match run_ps(&ps) {
        Ok(o) if o.status.success() => {}
        Ok(o) => warnings.push(format!(
            "整机预置未成功（不影响当前用户使用）：{}",
            String::from_utf8_lossy(&o.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        )),
        Err(e) => warnings.push(format!("整机预置跳过：{}", e)),
    }
    warnings
}

#[cfg(not(windows))]
pub fn install_for_user(_app: &AppHandle, _msix: &Path) -> Result<(), AppError> {
    Err(AppError::new("UNSUPPORTED_OS", "安装仅支持 Windows", ""))
}

#[cfg(not(windows))]
pub fn provision_all_users(_msix: &Path, _license: &Path) -> Vec<String> {
    vec!["预置仅支持 Windows".to_string()]
}

/// 卸载：结束进程 → 移除所有用户包 → 取消预置 → 清理服务与残留。
/// 各步骤尽力而为，失败只记录警告不中断整体流程。
pub fn uninstall_package(app: &AppHandle, remove_data: bool) -> Result<Vec<String>, AppError> {
    #[cfg(not(windows))]
    {
        let _ = (app, remove_data);
        return Err(AppError::new("UNSUPPORTED_OS", "卸载仅支持 Windows", ""));
    }
    #[cfg(windows)]
    {
        let mut warnings = Vec::new();
        let steps: Vec<(&str, &str)> = vec![
            ("正在结束残留进程…", "Get-Process -Name 'ChatGPT','codex' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue"),
            ("正在移除所有用户的应用包…", "Get-AppxPackage -Name 'OpenAI.Codex' -AllUsers | Remove-AppxPackage -AllUsers"),
            ("正在取消整机预置…", "$p = Get-AppxProvisionedPackage -Online | Where-Object { $_.PackageName -like 'OpenAI.Codex*' }; if ($p) { Remove-AppxProvisionedPackage -Online -PackageName $p.PackageName }"),
            ("正在清理残留服务…", "sc.exe delete 'CodexSandboxService.OpenAI.Codex' | Out-Null"),
        ];
        let mut stage = 1u8;
        for (text, script) in steps {
            let _ = app.emit(
                "uninstall-progress",
                InstallStage {
                    stage,
                    text: text.to_string(),
                },
            );
            match run_ps(script) {
                Ok(o) if o.status.success() => {}
                Ok(o) => warnings.push(format!(
                    "{}：{}",
                    text,
                    String::from_utf8_lossy(&o.stderr)
                        .chars()
                        .take(160)
                        .collect::<String>()
                )),
                Err(e) => warnings.push(format!("{}：{}", text, e)),
            }
            stage += 1;
        }
        if remove_data {
            let _ = app.emit(
                "uninstall-progress",
                InstallStage {
                    stage,
                    text: "正在清理用户数据…".into(),
                },
            );
            let script = "Remove-Item -Recurse -Force \"$env:LOCALAPPDATA\\Packages\\OpenAI.Codex_*\" -ErrorAction SilentlyContinue; Remove-Item -Recurse -Force \"$env:LOCALAPPDATA\\OpenAI\" -ErrorAction SilentlyContinue";
            if let Err(e) = run_ps(script) {
                warnings.push(format!("清理用户数据：{}", e));
            }
        }
        Ok(warnings)
    }
}

#[cfg(windows)]
fn run_ps(script: &str) -> Result<std::process::Output, AppError> {
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：不闪黑窗口
        .output()
        .map_err(|e| {
            AppError::new(
                "PS_FAILED",
                "无法调用 PowerShell",
                format!("请确认系统 PowerShell 可用：{}", e),
            )
        })
}

#[cfg(windows)]
fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}
