@echo off
REM FocusFlow Tauri build script.
REM Builds release and assembles dist\FocusFlow\ (portable folder).
REM NOTE: no longer deploys to the install dir (production folder must not be overwritten).
REM Toolchain paths (CARGO_HOME etc.) can be overridden via environment variables.
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

echo [1/3] Building Tauri release...
cargo build --release -p focusflow-desktop || goto :err

echo [2/3] Assembling dist folder...
REM 输出目录固定在本仓库下，与仓库位置无关
set DIST=%~dp0dist\FocusFlow
if not exist "%DIST%" mkdir "%DIST%"
if not exist "%DIST%\plugins" mkdir "%DIST%\plugins"
if not exist "%DIST%\data" mkdir "%DIST%\data"
if exist "%DIST%\FocusFlow.exe" del /q "%DIST%\FocusFlow.exe"
copy /y target\release\focusflow-desktop.exe "%DIST%\FocusFlow.exe" >nul
if not exist "%DIST%\config.ini" if exist config.ini copy /y config.ini "%DIST%\config.ini" >nul
copy /y crates\core\plugins\*.lua "%DIST%\plugins\" >nul

echo [3/3] Done!
echo.
echo Portable folder: %DIST%
echo Run: %DIST%\FocusFlow.exe
goto :eof

:err
echo Build failed!
exit /b 1
