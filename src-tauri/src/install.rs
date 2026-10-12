//! 安装 / 卸载：调用系统 PowerShell 的 AppX cmdlets。
//!
//! v1 采用社区验证过的可靠路线（Add-AppxPackage），进度以阶段事件呈现。
//! TODO(v2): 迁移到 PackageManager 原生 API，以获得真实百分比进度。
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::constants::*;
use crate::download::download_small_file;
use crate::errors::AppError;

#[derive(Debug, Clone, Serialize)]
pub struct InstallStage {
    /// 1=为当前用户注册，2=为所有用户预置，3=完成
    pub stage: u8,
    pub text: String,
}

fn emit_stage(app: &AppHandle, stage: u8, text: &str) {
    let _ = app.emit(
        "install-progress",
        InstallStage {
            stage,
            text: text.to_string(),
        },
    );
}

/// 确保离线授权文件存在（整机预置需要）。
pub fn ensure_license(app: &AppHandle, dir: &Path) -> Result<PathBuf, AppError> {
    let lic = dir.join("ChatGPT-License.xml");
    if !lic.exists() {
        let _ = app.emit(
            "install-progress",
            InstallStage {
                stage: 0,
                text: "正在获取离线授权文件…".into(),
            },
        );
        download_small_file(LICENSE_URL, &lic)?;
    }
    Ok(lic)
}

/// 安装流程：当前用户注册（立即生效）→ 整机预置（新用户也可用）。
/// 预置失败不视为整体失败（主流程已成功），仅通过警告事件告知。
pub fn install_package(app: &AppHandle, msix: &Path, license: &Path) -> Result<(), AppError> {
    #[cfg(not(windows))]
    {
        let _ = (app, msix, license);
        return Err(AppError::new("UNSUPPORTED_OS", "安装仅支持 Windows", ""));
    }
    #[cfg(windows)]
    {
        emit_stage(app, 1, "正在为当前用户注册应用包…");
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

        emit_stage(app, 2, "正在为所有用户预置（新登录的用户也能直接用）…");
        let ps2 = format!(
            "Add-AppxProvisionedPackage -Online -PackagePath '{}' -LicensePath '{}'",
            ps_escape(&msix.to_string_lossy()),
            ps_escape(&license.to_string_lossy())
        );
        match run_ps(&ps2) {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stderr).to_string();
                let _ = app.emit(
                    "install-warning",
                    format!(
                        "整机预置未成功（不影响当前用户使用）：{}",
                        text.chars().take(200).collect::<String>()
                    ),
                );
            }
            Err(e) => {
                let _ = app.emit("install-warning", format!("整机预置跳过：{}", e));
            }
        }

        emit_stage(app, 3, "安装完成");
        Ok(())
    }
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
