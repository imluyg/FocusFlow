@echo off
REM FocusFlow native build script (the only desktop front-end after P7).
REM Steps: cargo release -> assemble dist\FocusFlow-<ver>\ -> portable zip -> NSIS installer.
REM NOTE: keep this file ASCII-only. cmd.exe reads .bat in the OEM code page, so UTF-8
REM Chinese comments turn into garbage commands.
setlocal EnableDelayedExpansion
set "ROOT=%~dp0"

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
for /f "tokens=3 delims= " %%a in ('findstr /b "version" "%ROOT%desktop-native\Cargo.toml"') do (
    set VERSION=%%~a
)

echo [1/5] Building native release %VERSION% ...
REM Artifacts land in the ROOT target\ (workspace member), not desktop-native\target\.
cargo build --release -p focusflow-native || goto :err

echo [2/5] Assembling dist folder...
REM Build into a fresh versioned folder: dist\FocusFlow-<ver>.
REM (The old dist\FocusFlow was the Tauri portable folder and may still hold its exe/data;
REM reusing it would ship those leftovers inside the zip and the installer.)
set DIST=%ROOT%dist\FocusFlow-%VERSION%
if exist "%DIST%" rmdir /s /q "%DIST%"
mkdir "%DIST%"
mkdir "%DIST%\plugins"
mkdir "%DIST%\data"
copy /y "%ROOT%target\release\FocusFlow.exe" "%DIST%\FocusFlow.exe" >nul
REM No assets\ in the package: build.rs embeds assets\focusflow.ico into the exe as its
REM RT_GROUP_ICON (tray/title bar/Explorer all read that resource first). Shipping the loose
REM files again only duplicated the icon (and a .png no code reads) inside the zip/installer.
REM No config.ini in the package: the repo-root one is gitignored and is the developer's
REM local file, so it drifts from default_config() (e.g. scroll_burst_window 0.8 vs 0.25).
REM The app writes the code defaults on first run, which is what a fresh install should get.
copy /y "%ROOT%crates\core\plugins\*.lua" "%DIST%\plugins\" >nul

echo [3/5] Packing portable zip...
set ZIP=%ROOT%dist\FocusFlow-%VERSION%-win64.zip
if exist "%ZIP%" del /q "%ZIP%"
powershell -NoProfile -Command "Compress-Archive -Path '%DIST%\*' -DestinationPath '%ZIP%' -CompressionLevel Optimal" || goto :err

echo [4/5] Locating NSIS (makensis)...
set "MAKENSIS="
if exist "%LOCALAPPDATA%\tauri\NSIS\Bin\makensis.exe" set "MAKENSIS=%LOCALAPPDATA%\tauri\NSIS\Bin\makensis.exe"
if not defined MAKENSIS if exist "%ProgramFiles(x86)%\NSIS\makensis.exe" set "MAKENSIS=%ProgramFiles(x86)%\NSIS\makensis.exe"
if not defined MAKENSIS if exist "%ProgramFiles%\NSIS\makensis.exe" set "MAKENSIS=%ProgramFiles%\NSIS\makensis.exe"
if not defined MAKENSIS for %%m in (makensis.exe) do if not "%%~$PATH:m"=="" set "MAKENSIS=%%~$PATH:m"

if not defined MAKENSIS (
    echo   NSIS not found - skipping the installer.
    echo   Install NSIS 3, then run: makensis -INPUTCHARSET UTF8 installer.nsi
    goto :done
)

echo [5/5] Building NSIS installer...
REM -INPUTCHARSET UTF8 is required: installer.nsi is UTF-8 and has Chinese shortcut names.
"%MAKENSIS%" -INPUTCHARSET UTF8 -DVERSION=%VERSION% "%ROOT%installer.nsi" || goto :err
set SETUP=%ROOT%dist\FocusFlow-%VERSION%-setup.exe
if not exist "%SETUP%" (
    echo   makensis reported success but %SETUP% is missing.
    goto :err
)

:done
echo.
echo Portable folder: %DIST%
echo Portable zip   : %ZIP%
if exist "%SETUP%" echo Installer     : %SETUP%
echo Run            : %DIST%\FocusFlow.exe
goto :eof

:err
echo Build failed!
exit /b 1
