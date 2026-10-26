; FocusFlow 原生版安装脚本（NSIS 3）
;
; 编译：makensis -INPUTCHARSET UTF8 -DVERSION=0.5.0 installer.nsi
;       （build_native.bat 的第 [5/5] 步会自动带这两个参数；版本号从 desktop-native/Cargo.toml 读。）
;
; 为什么必须带 -INPUTCHARSET UTF8：本文件按 UTF-8 存，中文快捷方式名（"卸载"）只有按 UTF-8
; 读才不变成乱码。NSIS 默认按系统 ANSI 代码页读脚本，同一份文件会编译出乱码快捷方式。
;
; 安装位置：当前用户的 LocalAppData，不需要管理员权限（RequestExecutionLevel user）。
; 数据目录跟着程序目录走：卸载会连同 data/ 一起删。想保留数据，把 Uninstall 段里的
;   RMDir /r "$INSTDIR"
; 换成逐目录删除（只删 exe/assets/plugins/config.ini，留下 data/）。

Unicode true
!define APPNAME "FocusFlow"

; 版本号：命令行 -DVERSION=… 优先（build_native.bat 传的是 Cargo.toml 里的真值），
; 这里的默认值只是让人能手工单独编译一次。
!ifndef VERSION
  !define VERSION "0.5.0"
!endif
!ifndef PUBLISHER
  !define PUBLISHER "FocusFlow"
!endif

; 源目录 = build_native.bat 组装的 dist\FocusFlow-<版本>\。
; 注意不要指回旧的 dist\FocusFlow\：那是 Tauri 绿色包，里面还留着旧 FocusFlow.exe
; 和一份用户的 123\ 数据目录，按老路径打包会把这些一起塞进安装程序。
; ${__FILEDIR__} = 本脚本所在目录，因此 makensis 从哪个目录调用都不影响。
!ifndef DISTDIR
  !define DISTDIR "${__FILEDIR__}\dist\FocusFlow-${VERSION}"
!endif

!if /FILEEXISTS "${DISTDIR}\focusflow-native.exe"
!else
  !error "Missing ${DISTDIR}\focusflow-native.exe - run build_native.bat first."
!endif

Name "${APPNAME} ${VERSION}"
OutFile "${__FILEDIR__}\dist\FocusFlow-${VERSION}-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\FocusFlow"
InstallDirRegKey HKCU "Software\FocusFlow" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
BrandingText "${APPNAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APPNAME} installer"
VIAddVersionKey "CompanyName" "${PUBLISHER}"

Page directory
Page instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  ; File /r 只按匹配到的文件重建目录树，空目录会被丢掉 —— data\ 在 dist 里是空的，
  ; 这里补回来，用户装完就能看到"数据放哪"。
  File /r "${DISTDIR}\*.*"
  CreateDirectory "$INSTDIR\data"

  CreateShortCut "$DESKTOP\FocusFlow.lnk" "$INSTDIR\focusflow-native.exe"
  CreateDirectory "$SMPROGRAMS\FocusFlow"
  CreateShortCut "$SMPROGRAMS\FocusFlow\FocusFlow.lnk" "$INSTDIR\focusflow-native.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortCut "$SMPROGRAMS\FocusFlow\卸载.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "Software\FocusFlow" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayIcon" "$INSTDIR\focusflow-native.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$DESKTOP\FocusFlow.lnk"
  RMDir /r "$SMPROGRAMS\FocusFlow"
  ; 连同 data/ 一起删（纯绿色应用：数据跟着程序目录走）。
  RMDir /r "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow"
  DeleteRegKey HKCU "Software\FocusFlow"
SectionEnd
