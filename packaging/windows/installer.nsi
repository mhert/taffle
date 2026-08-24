!ifndef VERSION
  !define VERSION "0.0.0"
!endif

Name "taffle"
; The output file is named for the distribution package, `taffle-cli`, so that the two Windows
; installers on a release page tell themselves apart. Everything below keeps the name the
; program has always carried on the machine, which is what lets an older install be upgraded
; rather than doubled.
OutFile "taffle-cli-${VERSION}-x86_64-setup.exe"
InstallDir "$PROGRAMFILES64\taffle"
RequestExecutionLevel admin

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles
ShowInstDetails show
ShowUninstDetails show

; The machine PATH is edited through PowerShell rather than with NSIS registry
; writes. NSIS's default build truncates strings at 1024 characters and a machine
; PATH routinely exceeds that, so reading it into a register and writing it back
; would silently destroy whatever did not fit. PowerShell has no such limit, and
; SetEnvironmentVariable broadcasts the change itself, so a newly opened shell
; sees the entry without a reboot.
;
; The nsExec string is delimited with a backtick rather than a single quote: NSIS
; ends a quoted string at the first unescaped matching quote, and the PowerShell
; body below contains single quotes of its own, so a single-quoted delimiter would
; truncate the command at the first one and silently hand PowerShell a fragment.
; The backtick is therefore the one character the PowerShell body must never hold:
; PowerShell's own escapes (`n, `t, `") would end the NSIS string exactly the way a
; single quote used to, and the truncated installer still builds and still runs.
!macro RunPowerShell Script
  nsExec::ExecToLog `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -Command "${Script}"`
  Pop $0
  ; nsExec pushes the process exit code, or "error" when it could not be started at
  ; all. Dropping it is how a PATH edit that never ran would still show a completed
  ; install, so anything but success is said out loud rather than swallowed.
  StrCmp $0 "0" +3
  DetailPrint "taffle: the PATH update did not run (powershell said: $0)"
  SetErrors
!macroend

Section "Install"
  SetOutPath "$INSTDIR"
  File "stage\taffle.exe"
  File "stage\LICENSE"
  File "stage\README.md"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; So the install shows up in Settings > Apps and can be removed from there.
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle" \
    "DisplayName" "taffle"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle" \
    "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle" \
    "Publisher" "mhert"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle" \
    "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle" \
    "InstallLocation" "$INSTDIR"

  ; Append the install directory to the machine PATH, but only once: re-running
  ; the installer or upgrading must not leave two copies of the same entry.
  !insertmacro RunPowerShell "$$p = [Environment]::GetEnvironmentVariable('Path','Machine'); if ($$p -split ';' -notcontains '$INSTDIR') { [Environment]::SetEnvironmentVariable('Path', ($$p.TrimEnd(';') + ';$INSTDIR'), 'Machine') }"
SectionEnd

Section "Uninstall"
  ; Take the directory back out of PATH before removing it, so uninstalling does
  ; not leave an entry pointing at nothing. Guarded against an unset machine PATH:
  ; without the guard, $$p is empty and SetEnvironmentVariable would write an empty
  ; string, which deletes the machine PATH variable outright rather than leaving it
  ; alone. Only our own entry is filtered out, not every empty element, so a machine
  ; whose PATH happens to end in a trailing ';' keeps that separator untouched.
  !insertmacro RunPowerShell "$$p = [Environment]::GetEnvironmentVariable('Path','Machine'); if ($$p) { [Environment]::SetEnvironmentVariable('Path', (($$p -split ';' | Where-Object { $$_ -ne '$INSTDIR' }) -join ';'), 'Machine') }"

  Delete "$INSTDIR\taffle.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  ; The uninstaller cannot delete its own running image, and a plain RMDir would
  ; then fail on the directory it left behind. /REBOOTOK defers both removals to
  ; the next reboot instead of leaving an orphaned directory with no way to retry.
  Delete /REBOOTOK "$INSTDIR\uninstall.exe"
  RMDir /REBOOTOK "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle"
SectionEnd
