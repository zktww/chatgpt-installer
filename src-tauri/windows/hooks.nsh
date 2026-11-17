; NSIS installer hooks (Tauri 2)
; Remove the optional daily auto-update task when the app is uninstalled.
!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'schtasks.exe /Delete /TN "ChatGPT Installer AutoUpdate" /F'
!macroend
