; FindIt installer hooks (included by Tauri's NSIS template).

!macro NSIS_HOOK_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
!macroend

!macro NSIS_HOOK_PREUNINSTALL
!macroend

; The uninstaller's confirmation page has a "Delete the application data" checkbox.
; When it is ticked, also remove FindIt's index, settings and logs, which live in
; %LOCALAPPDATA%\FindIt (not in the identifier folder Tauri cleans by default).
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
    RMDir /r "$LOCALAPPDATA\FindIt"
  ${EndIf}
  ; "Start with Windows" entry written by the app (harmless if absent).
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "FindIt"
!macroend
