; hi-agent.nsi -- Windows installer for Hi Agent (NSIS / Modern UI 2).
;
; The Windows analog of scripts/make-dmg.sh. Produces a per-user, no-admin
; Setup.exe that drops the (cross-compiled) hi-agent.exe under
; %LOCALAPPDATA%\Programs\Hi Agent, wires up Start Menu + Desktop shortcuts and
; an Add/Remove-Programs entry, and ships an uninstaller. The managed runtime
; (Node + claude + esbuild + ffmpeg + models) is NOT bundled here -- the binary
; auto-provisions it into the OS cache on first launch (the "it runs" tier).
;
; Driven entirely by /D defines from scripts/make-installer.sh:
;   VERSION   display version, e.g. 0.1.0       (default below)
;   VERSION4  four-part numeric, e.g. 0.1.0.0   (for VIProductVersion)
;   SRCEXE    path to the built hi-agent.exe
;   ICON      path to HiAgent.ico
;   OUTFILE   output Setup.exe path

Unicode true
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef VERSION4
  !define VERSION4 "0.0.0.0"
!endif
!ifndef SRCEXE
  !define SRCEXE "..\target\x86_64-pc-windows-msvc\release\hi-agent.exe"
!endif
!ifndef ICON
  !define ICON "HiAgent.ico"
!endif
!ifndef OUTFILE
  !define OUTFILE "..\target\installer\hi-agent-setup.exe"
!endif

!define APPNAME    "Hi Agent"
!define PUBLISHER  "Human Interface"
!define EXENAME    "hi-agent.exe"
; Add/Remove-Programs key (per-user). Stable id, not the display name.
!define ARP_KEY    "Software\Microsoft\Windows\CurrentVersion\Uninstall\hi-agent"

Name "${APPNAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${APPNAME}"
; Per-user install -- no UAC elevation, like Chrome's default consumer install.
RequestExecutionLevel user
InstallDirRegKey HKCU "Software\hi-agent" "InstallDir"
BrandingText "${APPNAME} ${VERSION}"

VIProductVersion "${VERSION4}"
VIAddVersionKey "ProductName"     "${APPNAME}"
VIAddVersionKey "FileDescription" "${APPNAME} installer"
VIAddVersionKey "CompanyName"     "${PUBLISHER}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "FileVersion"     "${VERSION4}"
VIAddVersionKey "LegalCopyright"  "(C) ${PUBLISHER}"

!define MUI_ICON   "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  File "/oname=${EXENAME}" "${SRCEXE}"
  File "/oname=HiAgent.ico" "${ICON}"

  ; Shortcuts (icon from the .ico, since the exe carries none yet).
  CreateShortcut "$SMPROGRAMS\${APPNAME}.lnk" "$INSTDIR\${EXENAME}" "" "$INSTDIR\HiAgent.ico"
  CreateShortcut "$DESKTOP\${APPNAME}.lnk"    "$INSTDIR\${EXENAME}" "" "$INSTDIR\HiAgent.ico"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\hi-agent" "InstallDir" "$INSTDIR"

  ; Add/Remove Programs entry.
  WriteRegStr   HKCU "${ARP_KEY}" "DisplayName"     "${APPNAME}"
  WriteRegStr   HKCU "${ARP_KEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKCU "${ARP_KEY}" "Publisher"       "${PUBLISHER}"
  WriteRegStr   HKCU "${ARP_KEY}" "DisplayIcon"     "$INSTDIR\HiAgent.ico"
  WriteRegStr   HKCU "${ARP_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   HKCU "${ARP_KEY}" "UninstallString"      '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKCU "${ARP_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${ARP_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${ARP_KEY}" "NoRepair" 1

  ; EstimatedSize (KB) shown in Add/Remove Programs.
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${ARP_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  ; Remove the program files + shortcuts + registry. User data and the managed
  ; runtime cache (under %LOCALAPPDATA%/%APPDATA% ProjectDirs) are intentionally
  ; left untouched -- uninstalling the app must not delete the user's life DB.
  Delete "$SMPROGRAMS\${APPNAME}.lnk"
  Delete "$DESKTOP\${APPNAME}.lnk"
  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\HiAgent.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${ARP_KEY}"
  DeleteRegKey HKCU "Software\hi-agent"
SectionEnd
