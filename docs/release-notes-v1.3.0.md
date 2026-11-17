# v1.3.0 发布说明

## ✨ 新功能

- **可选的定时自动更新**（默认关闭）：欢迎页「自动更新」可选择
  - **有新版时提醒我**：每天检查一次 OpenAI 官方版本，发现新版时弹出安装助手；
  - **自动下载并安装**：后台下载、四重校验后为当前用户安装；ChatGPT 正在运行、需要管理员权限或磁盘空间不足时不会强行处理，改为弹窗提醒。
  - 通过当前用户的普通权限计划任务「ChatGPT Installer AutoUpdate」实现（每天 12:00，错过则开机后补查）；关闭时删除任务，卸载安装版时自动删除。
- **winget 分发**：`winget install zktww.ChatGPTInstaller`（以 winget 仓库审核结果为准）。

## 🐛 修复与调整

- 磁盘空间要求由 3 GB 调整为 4 GB（官方安装包已增至 x64 约 1.06 GB / ARM64 约 1.17 GB）。
- 修正 README 与界面中过时的描述（旧下载地址、包名、安装包体积、程序体积、版本检测方式）。
- 安装版文件名统一为 `chatgpt-installer_<版本>_x64-setup.exe`。

## 🔧 依赖与工程

- Tauri 2.12.3 → 2.12.6；reqwest 0.12 → 0.13（TLS 改用系统 SChannel，自动使用系统代理）；zip 2 → 9（仅保留 deflate）；quick-xml 0.37 → 0.42；windows-sys 0.59 → 0.61；winreg 0.55 → 0.56；MSRV 1.90。
- GitHub Actions 升级到 Node 24 版本（checkout v7 / upload-artifact v7 / action-gh-release v3）。
