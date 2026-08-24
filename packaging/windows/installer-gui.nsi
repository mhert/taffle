!ifndef VERSION
  !define VERSION "0.0.0"
!endif

Name "Taffle"
OutFile "taffle-gui-${VERSION}-x86_64-setup.exe"
InstallDir "$PROGRAMFILES64\taffle-gui"
RequestExecutionLevel admin

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles
ShowInstDetails show
ShowUninstDetails show

; Nothing here touches the machine PATH, where the command-line installer beside this one
; has to: a window is started from its Start-menu shortcut and never typed at a prompt, so
; an entry on PATH would give the user nothing and hand every shell one more directory to
; search. That also leaves this installer with nothing to run PowerShell for.

Section "Install"
  SetOutPath "$INSTDIR"
  ; The staged tree goes in whole rather than file by file. windeployqt fills it with the Qt
  ; libraries, the platform and image plugins and the QML modules the chrome imports:
  ; hundreds of files across a tree of directories whose shape follows the Qt release it was
  ; deployed from. Naming them here would mean editing this file on every Qt upgrade, and a
  ; name that fell behind would ship an installer that silently leaves a library out.
  File /r "stage-gui\*"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; So the install shows up in Settings > Apps and can be removed from there.
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle-gui" \
    "DisplayName" "Taffle"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle-gui" \
    "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle-gui" \
    "Publisher" "mhert"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle-gui" \
    "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle-gui" \
    "InstallLocation" "$INSTDIR"

  ; The shortcut is the way in, so it is what the install ends with: one entry in the Start
  ; menu rather than a folder of its own, because there is a single program to start and
  ; nothing to stand beside it.
  CreateShortcut "$SMPROGRAMS\Taffle.lnk" "$INSTDIR\taffle-gui.exe"
SectionEnd

Section "Uninstall"
  Delete "$SMPROGRAMS\Taffle.lnk"

  ; Taken back as a tree, where the command-line installer deletes the three files it wrote
  ; by name: what File /r put here is a deployed Qt tree, too wide and too tied to the Qt it
  ; came from to list, and an uninstaller written file by file would leave behind everything
  ; a later Qt added. This directory holds nothing but the install, so removing all of it
  ; removes exactly what was put there.
  ;
  ; /REBOOTOK covers what Windows will not release while the uninstaller runs: a Qt library a
  ; still-running copy of the program holds open cannot be deleted, and without the flag the
  ; uninstall would stop at it and leave a half-removed tree with nothing left to retry it.
  RMDir /r /REBOOTOK "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\taffle-gui"
SectionEnd
