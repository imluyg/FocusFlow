; FocusFlow 原生版安装脚本（NSIS 3）
;
; 编译：makensis -INPUTCHARSET UTF8 -DVERSION=0.5.0 installer.nsi
;       （build_native.bat 的第 [5/5] 步会自动带这两个参数；版本号从 desktop-native/Cargo.toml 读。）
;
; 为什么必须带 -INPUTCHARSET UTF8：本文件按 UTF-8 存，中文快捷方式名（"卸载"）只有按 UTF-8
; 读才不变成乱码。NSIS 默认按系统 ANSI 代码页读脚本，同一份文件会编译出乱码快捷方式。
;
; 安装位置：当前用户的 LocalAppData，不需要管理员权限（RequestExecutionLevel user）。
; 卸载口径：统计资料（$INSTDIR\data 里的库、截图、日志）默认保留 —— 静默卸载（/S）不弹问、
; 直接保留，图形卸载问一次且默认按钮是「否」；只有一条明确答「是」的路径会带 /r 整目录删掉。
; 理由：卸载程序不等于删掉几年的记录，而那次删除没有第二次机会（详见 Uninstall 段的注释）。

Unicode true
!define APPNAME "FocusFlow"

; 版本号：命令行 -DVERSION=… 优先（build_native.bat 传的是 Cargo.toml 里的真值），
; 这里的默认值只是让人能手工单独编译一次。
!ifndef VERSION
  !define VERSION "0.5.2"
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

!if /FILEEXISTS "${DISTDIR}\FocusFlow.exe"
!else
  !error "Missing ${DISTDIR}\FocusFlow.exe - run build_native.bat first."
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

  CreateShortCut "$DESKTOP\FocusFlow.lnk" "$INSTDIR\FocusFlow.exe"
  CreateDirectory "$SMPROGRAMS\FocusFlow"
  CreateShortCut "$SMPROGRAMS\FocusFlow\FocusFlow.lnk" "$INSTDIR\FocusFlow.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortCut "$SMPROGRAMS\FocusFlow\卸载.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "Software\FocusFlow" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "DisplayIcon" "$INSTDIR\FocusFlow.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$DESKTOP\FocusFlow.lnk"
  RMDir /r "$SMPROGRAMS\FocusFlow"

  ; 统计资料默认保留。库与截图在 $INSTDIR\data、可回滚的快照在 $INSTDIR\backup，而"卸载程序"
  ; 这个动作从来不代表"删掉我这两年记下的东西"—— 旧版一条 RMDir /r 把它们一起抹了，
  ; 且没有第二次机会。静默卸载（/S，含脚本与「程序和功能」里的自动卸载）不弹问、直接保留：
  ; 没人能回答的对话框只会让卸载挂在那里等。图形卸载问一次，默认按钮是「否」。
  IfSilent keepdata
  MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 \
    "要把统计资料一起删掉吗？$\r$\n$\r$\n数据库、截图和日志都在：$\r$\n$INSTDIR\data$\r$\n$\r$\n选「否」：只删程序，资料留着，重新安装后接着能用。$\r$\n选「是」：整个文件夹一起删掉，删完找不回来。" \
    IDYES deldata
  Goto keepdata

  deldata:
    ; 用户明确答了「是」才走到这里（带 /r 的整目录删除只有这一条入口）。
    RMDir /r "$INSTDIR"
    Goto regkeys

  keepdata:
    ; 只删 Install 段自己放下去的东西。data\ 与用户往这个目录里塞的任何文件都留着，
    ; 所以最后那条 RMDir 不带 /r —— 目录非空就删不动，这正是"留着资料"该有的样子。
    ; （装完没跑过、目录里确实没攒下东西时，这一条会把空壳一起收掉。）
    Delete "$INSTDIR\FocusFlow.exe"
    Delete "$INSTDIR\config.ini"
    Delete "$INSTDIR\window_state.ini"
    Delete "$INSTDIR\focusflow.log"
    Delete "$INSTDIR\focusflow.log.1"
    Delete "$INSTDIR\uninstall.exe"
    RMDir /r "$INSTDIR\plugins"
    RMDir /r "$INSTDIR\logs"
    RMDir "$INSTDIR"

  regkeys:
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow"
    DeleteRegKey HKCU "Software\FocusFlow"
SectionEnd
