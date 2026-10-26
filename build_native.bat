@echo off
REM FocusFlow native build script (the only desktop front-end after P7).
REM Builds release, assembles dist\FocusFlow\ (portable folder), then packs a versioned zip.
REM NOTE: keep this file ASCII-only. cmd.exe reads .bat in the OEM code page, so UTF-8
REM Chinese comments turn into garbage commands.
setlocal EnableDelayedExpansion

if defined VCVARS64_PATH (
    call "%VCVARS64_PATH%" >nul
) else if exist "C:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" (
    call "C:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
)
if not defined CARGO_HOME (
    set CARGO_HOME=%USERPROFILE%\.cargo
)
set PATH=%CARGO_HOME%\bin;%PATH%

REM Version comes from desktop-native/Cargo.toml so there is only one place to bump.
REM line looks like: version = "0.5.0" -> token 3 is the quoted version
for /f "tokens=3 delims= " %%a in ('findstr /b "version" "%~dp0desktop-native\Cargo.toml"') do (
    set VERSION=%%~a
)

echo [1/4] Building native release %VERSION% ...
cargo build --release -p focusflow-native || goto :err

echo [2/4] Assembling dist folder...
REM Build into a fresh versioned folder: dist\FocusFlow-<ver>.
REM (The old dist\FocusFlow was the Tauri portable folder and may still hold its exe/data;
REM reusing it would ship those leftovers inside the zip.)
set DIST=%~dp0dist\FocusFlow-%VERSION%
if exist "%DIST%" rmdir /s /q "%DIST%"
mkdir "%DIST%"
mkdir "%DIST%\plugins"
mkdir "%DIST%\data"
mkdir "%DIST%\assets"
copy /y target\release\focusflow-native.exe "%DIST%\focusflow-native.exe" >nul
if exist "%~dp0desktop-native\assets\*" copy /y "%~dp0desktop-native\assets\*" "%DIST%\assets\" >nul
if exist config.ini copy /y config.ini "%DIST%\config.ini" >nul
copy /y crates\core\plugins\*.lua "%DIST%\plugins\" >nul

echo [3/4] Packing portable zip...
set ZIP=%~dp0dist\FocusFlow-%VERSION%-win64.zip
if exist "%ZIP%" del /q "%ZIP%"
powershell -NoProfile -Command "Compress-Archive -Path '%DIST%\*' -DestinationPath '%ZIP%' -CompressionLevel Optimal" || goto :err

echo [4/4] Done!
echo.
echo Portable folder: %DIST%
echo Portable zip   : %ZIP%
echo Run            : %DIST%\focusflow-native.exe
echo.
echo Installer needs NSIS (makensis); once installed run: makensis installer.nsi
goto :eof

:err
echo Build failed!
exit /b 1
