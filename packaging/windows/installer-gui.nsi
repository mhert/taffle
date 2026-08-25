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
  ; NSIS builds this installer against a 32-bit stub, and a 32-bit program on 64-bit Windows reads
  ; and writes the 32-bit view of the registry: every key below would land under WOW6432Node
  ; instead, where Settings > Apps and every other 64-bit reader of the uninstall list will not
  ; look. Choosing a view Windows does not have fails every registry call, and that cannot happen
  ; here: this installs an x86_64 program into $PROGRAMFILES64, so it only runs on 64-bit Windows.
  SetRegView 64

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
  ; The view the install wrote in. Left at the 32-bit default, the DeleteRegKey below would look
  ; under Software\WOW6432Node, find nothing, and leave the entry in Settings > Apps behind.
  SetRegView 64

  ; In an uninstaller $INSTDIR is not what the directory page was set to: it is the directory the
  ; uninstaller is run from, and the documented _?= switch overrides even that. A copy of
  ; uninstall.exe carried into a downloads folder or the root of a drive would therefore aim the
  ; recursive delete below at that directory, and a recursive delete cannot be taken back. So the
  ; uninstall runs only where the program it removes actually is; anywhere else it says so and
  ; stops before touching anything, leaving the install whole and removable from where it was put.
  IfFileExists "$INSTDIR\taffle-gui.exe" dir_is_ours
    DetailPrint "Taffle: $INSTDIR holds no taffle-gui.exe, so this uninstaller will not remove it."
    Abort
  dir_is_ours:

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
