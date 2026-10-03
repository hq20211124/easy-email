; Easy Email — NSIS hooks, pulled in by `bundle.windows.nsis.installerHooks`.
;
; Tauri's NSIS installer writes `SHCTX\Software\Classes\mailto` and nothing else, so Windows' own
; "默认应用" page cannot see us: a user who wants Easy Email to be the default mail program has no
; way to pick it. These two macros fix that, and they are the part of the installer Tauri explicitly
; does not do (design/17 §3.3).
;
; `SHCTX` is HKCU while `installMode` stays `currentUser`, which the design insists on for a second
; reason: a per-machine (elevated) install cannot raise toast notifications, and notifications are a
; feature of this client (design/17 §9).

!macro NSIS_HOOK_POSTINSTALL
  ; Make the app appear in Windows' "default apps" page, with mailto and .eml pointing at it.
  WriteRegStr SHCTX "Software\RegisteredApplications" "Easy Email" "Software\Easy Email\Capabilities"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities" "ApplicationName" "Easy Email"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities" "ApplicationDescription" "轻量、本地优先的邮件客户端"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities\URLAssociations" "mailto" "EasyEmail.mailto"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities\FileAssociations" ".eml" "EasyEmail.eml"
  WriteRegStr SHCTX "Software\Classes\EasyEmail.mailto\shell\open\command" "" '"$INSTDIR\easy-email.exe" "%1"'
  WriteRegStr SHCTX "Software\Classes\EasyEmail.eml\shell\open\command" "" '"$INSTDIR\easy-email.exe" "%1"'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; `RegisteredApplications` holds *values* (name -> capabilities path), not subkeys, so removing us
  ; from the default-apps list is `DeleteRegValue`. Deleting the key instead is a no-op, and the app
  ; would stay in that list forever (the second review of design/17 §3.3 caught exactly that).
  DeleteRegValue SHCTX "Software\RegisteredApplications" "Easy Email"
  DeleteRegKey SHCTX "Software\Easy Email"
  DeleteRegKey SHCTX "Software\Classes\EasyEmail.mailto"
  DeleteRegKey SHCTX "Software\Classes\EasyEmail.eml"
!macroend
