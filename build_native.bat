@echo off
REM FocusFlow 原生版构建脚本（P7 之后唯一的桌面端）。
REM 构建 release 并组装 dist\FocusFlow\（绿色版目录）。
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

echo [1/3] Building native release...
cargo build --release -p focusflow-native || goto :err

echo [2/3] Assembling dist folder...
set DIST=%~dp0dist\FocusFlow
if not exist "%DIST%" mkdir "%DIST%"
if not exist "%DIST%\plugins" mkdir "%DIST%\plugins"
if not exist "%DIST%\data" mkdir "%DIST%\data"
if exist "%DIST%\focusflow-native.exe" del /q "%DIST%\focusflow-native.exe"
copy /y target\release\focusflow-native.exe "%DIST%\focusflow-native.exe" >nul
if not exist "%DIST%\config.ini" if exist config.ini copy /y config.ini "%DIST%\config.ini" >nul
copy /y crates\core\plugins\*.lua "%DIST%\plugins\" >nul

echo [3/3] Done!
echo.
echo Portable folder: %DIST%
echo Run: %DIST%\focusflow-native.exe
goto :eof

:err
echo Build failed!
exit /b 1
