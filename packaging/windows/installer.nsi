; The Windows installer: per user (no administrator rights), into
; %LOCALAPPDATA%\Programs\Ferry, with a Start menu shortcut and an entry
; in Settings → Apps.
;
;   makensis /DVERSION=1.2.3 /DFILE_VERSION=1.2.3.0 /DAPP=path\to\ferry-gui.exe
;            /DCLI=path\to\ferry-cli.exe /DLICENSES=path\to\THIRD_PARTY_LICENSES.html
;            /DLANGUAGES=path\to\languages.nsh /DOUT=path\to\setup.exe installer.nsi
;
; The app is installed as Ferry.exe and the CLI next to it as ferry-cli.exe
; (docs/adr/0001, "Packaging"). LICENSES is the notices cargo-about wrote
; (about.toml), installed next to Ferry.exe, where About opens it, with
; Ferry's own LICENSE as LICENSE.txt.
; LANGUAGES is what `packaging/i18n.sh nsis` prints: the app's languages
; and the installer's own strings in each. The installer speaks the
; system's language if it is one of them, else English.

Unicode true
!include "MUI2.nsh"

!define APP_ID "dev.fanchao.Ferry"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

Name "Ferry"
OutFile "${OUT}"
InstallDir "$LOCALAPPDATA\Programs\Ferry"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma

VIProductVersion "${FILE_VERSION}"
VIAddVersionKey "ProductName" "Ferry"
VIAddVersionKey "FileDescription" "Ferry installer"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Copyright © 2026 Fanchao"

!define MUI_ICON "..\..\assets\windows\app_icon.ico"
!define MUI_UNICON "..\..\assets\windows\app_icon.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\Ferry.exe"
!define MUI_FINISHPAGE_RUN_TEXT "$(package_start_app)"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!include /CHARSET=UTF8 "${LANGUAGES}"

Section "Ferry"
  SetOutPath "$INSTDIR"
  File "/oname=Ferry.exe" "${APP}"
  File "/oname=THIRD_PARTY_LICENSES.html" "${LICENSES}"
  ; makensis runs from this file's directory.
  File "/oname=LICENSE.txt" "..\..\LICENSE"
  File "/oname=ferry-cli.exe" "${CLI}"
  ; Where versions before the rename put the CLI.
  Delete "$INSTDIR\cli\ferry.exe"
  RMDir "$INSTDIR\cli"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\Ferry.lnk" "$INSTDIR\Ferry.exe"

  ; The notification id (AUMID) the app sends toasts as, registered the way
  ; Windows documents for apps that aren't packaged.
  WriteRegStr HKCU "Software\Classes\AppUserModelId\${APP_ID}" "DisplayName" "Ferry"
  WriteRegStr HKCU "Software\Classes\AppUserModelId\${APP_ID}" "IconUri" "$INSTDIR\Ferry.exe"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "Ferry"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\Ferry.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Fanchao"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/cloudwalkerlabs/ferryapp"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

; Leaves the data (identity, trust, settings) and received files alone.
Section "Uninstall"
  Delete "$SMPROGRAMS\Ferry.lnk"
  Delete "$INSTDIR\Ferry.exe"
  Delete "$INSTDIR\THIRD_PARTY_LICENSES.html"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\ferry-cli.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "Software\Classes\AppUserModelId\${APP_ID}"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
