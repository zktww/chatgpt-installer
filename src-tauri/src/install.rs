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
    // 路径用通配符安全引用：Add-AppxPackage -Path 会解释 [ ] 为通配符
    let ps = format!(
        "Add-AppxPackage -Path {} -ForceTargetApplicationShutdown",
        ps_quote_literal(&msix.to_string_lossy())
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
        "Add-AppxProvisionedPackage -Online -PackagePath {} -LicensePath {}",
        ps_quote_literal(&msix.to_string_lossy()),
        ps_quote_literal(&license.to_string_lossy())
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
            // 跨用户提权时清理原用户的数据，而非提权用的管理员账号的 $env:LOCALAPPDATA
            let base_ps: String = match crate::cmds::data_dir_override(app) {
                Some(d) => {
                    // 字面引用目录（防 [ ] 通配），但保留末尾有意的 *（匹配版本号后缀）
                    let mut s = String::from("\"");
                    for c in d.to_string_lossy().chars() {
                        if matches!(c, '"' | '`' | '$' | '[' | ']') {
                            s.push('`');
                        }
                        s.push(c);
                    }
                    s.push('"');
                    s
                }
                None => "\"$env:LOCALAPPDATA\"".to_string(),
            };
            let script = format!(
                "Remove-Item -Recurse -Force {}\\Packages\\OpenAI.Codex_* -ErrorAction SilentlyContinue; Remove-Item -Recurse -Force {}\\OpenAI -ErrorAction SilentlyContinue",
                base_ps, base_ps
            );
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

/// PowerShell 双引号字面路径：转义 ` " $ 及通配符 [ ] * ?，确保按字面路径处理。
/// （单引号字符串里反引号无转义作用，故用双引号；
/// Windows 文件名本身不允许出现 * ? "，实际要防的是 [ ]。）
#[cfg(windows)]
pub(crate) fn ps_quote_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '[' | ']' | '*' | '?') {
            out.push('`');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// 本机已安装的 ChatGPT 版本（Get-AppxPackage）；未安装返回 None。
#[cfg(windows)]
pub fn installed_app_version() -> Option<String> {
    let out =
        run_ps("(Get-AppxPackage -Name 'OpenAI.Codex' | Select-Object -First 1).Version").ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

#[cfg(not(windows))]
pub fn installed_app_version() -> Option<String> {
    None
}
