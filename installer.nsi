; FocusFlow 原生版安装脚本（需要 NSIS 3：装好后 `makensis installer.nsi`）
; 说明：安装到当前用户的 LocalAppData，不需要管理员权限；
;       卸载会连同 data/ 一起删（纯绿色应用，数据跟着程序目录走）——
;       如果希望卸载保留数据，把下面 Uninstall 段的 RMDir /r "$INSTDIR" 换成逐目录删除。
Unicode true
!define APPNAME "FocusFlow"
!define VERSION "0.5.0"
!define PUBLISHER "FocusFlow"

Name "${APPNAME} ${VERSION}"
OutFile "dist\FocusFlow-${VERSION}-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\FocusFlow"
RequestExecutionLevel user
SetCompressor /SOLID lzma

Page directory
Page instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File /r "dist\FocusFlow\*.*"
  CreateShortCut "$DESKTOP\FocusFlow.lnk" "$INSTDIR\focusflow-native.exe"
  CreateDirectory "$SMPROGRAMS\FocusFlow"
  CreateShortCut "$SMPROGRAMS\FocusFlow\FocusFlow.lnk" "$INSTDIR\focusflow-native.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortCut "$SMPROGRAMS\FocusFlow\卸载.lnk" "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "UninstallString" "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayIcon" "$INSTDIR\focusflow-native.exe"
SectionEnd

Section "Uninstall"
  Delete "$DESKTOP\FocusFlow.lnk"
  RMDir /r "$SMPROGRAMS\FocusFlow"
  RMDir /r "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow"
SectionEnd
