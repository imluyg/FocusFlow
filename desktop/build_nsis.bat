@echo off
REM FocusFlow Tauri NSIS 安装包构建脚本。
REM 工具链路径可通过环境变量覆盖（默认回退用户级 cargo 与常见 BuildTools 位置）。
setlocal
if defined VCVARS64_PATH (
    call "%VCVARS64_PATH%" >nul
) else if exist "C:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" (
    call "C:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
)
if not defined CARGO_HOME (
    set CARGO_HOME=%USERPROFILE%\.cargo
)
set PATH=%CARGO_HOME%\bin;%PATH%
cd /d %~dp0
call npx --yes @tauri-apps/cli build
exit /b %ERRORLEVEL%
