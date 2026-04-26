@echo off
:: Full Windows build: Tauri 2 (Rust + vanilla JS UI) -> standalone .exe
:: plus NSIS / MSI installers, all copied into build\windows\.
::
:: Output:
::   build\windows\ClipBridge.exe         (standalone, no installer)
::   build\windows\ClipBridge-Setup.exe   (NSIS user installer, smaller)
::   build\windows\ClipBridge.msi         (MSI for Group Policy / IT mgmt)
::
:: Requires: Rust w/ MSVC toolchain (https://rustup.rs/),
::           Visual Studio Build Tools w/ "Desktop dev with C++" workload.
::           tauri-cli is auto-installed on first run if missing.
::
:: Usage:  scripts\build-windows-exe.cmd

setlocal enabledelayedexpansion

set "SCRIPT_DIR=%~dp0"
set "ROOT=%SCRIPT_DIR%.."
set "WIN_DIR=%ROOT%\clients\windows"
set "OUT=%ROOT%\build\windows"

:: --- 0/3 sanity checks --------------------------------------------------
where cargo >nul 2>&1
if errorlevel 1 (
  echo [ERROR] cargo not found on PATH. Install Rust from https://rustup.rs/
  exit /b 1
)

cargo tauri --version >nul 2>&1
if errorlevel 1 (
  echo ==^> tauri-cli not found, installing ^(one-time, ~3-5 min^)
  cargo install tauri-cli --version "^2.0" --locked
  if errorlevel 1 (
    echo [ERROR] failed to install tauri-cli
    exit /b 1
  )
)

if not exist "%OUT%" mkdir "%OUT%"

:: --- 1/3 tauri build ----------------------------------------------------
:: `cargo tauri build` from clients\windows runs:
::   - cargo build --release (compiles clipbridge-core + the tauri shell)
::   - tauri-bundler (writes MSI + NSIS installers under target\release\bundle)
echo ==^> 1/3 cargo tauri build ^(release^)
pushd "%WIN_DIR%"
cargo tauri build
if errorlevel 1 (
  popd
  echo [ERROR] cargo tauri build failed
  exit /b 1
)
popd

set "REL=%WIN_DIR%\target\release"

:: --- 2/3 standalone exe -------------------------------------------------
echo ==^> 2/3 collecting standalone exe
if not exist "%REL%\clipbridge-windows.exe" (
  echo [ERROR] expected %REL%\clipbridge-windows.exe, not found
  exit /b 1
)
copy /y "%REL%\clipbridge-windows.exe" "%OUT%\ClipBridge.exe" >nul

:: --- 3/3 installers (filenames include version, so glob via for) --------
echo ==^> 3/3 collecting installers
set "FOUND_NSIS=0"
for %%f in ("%REL%\bundle\nsis\*-setup.exe") do (
  copy /y "%%~ff" "%OUT%\ClipBridge-Setup.exe" >nul
  set "FOUND_NSIS=1"
)
if "!FOUND_NSIS!"=="0" echo [warn] no NSIS installer produced ^(check tauri.conf.json bundle.targets^)

set "FOUND_MSI=0"
for %%f in ("%REL%\bundle\msi\*.msi") do (
  copy /y "%%~ff" "%OUT%\ClipBridge.msi" >nul
  set "FOUND_MSI=1"
)
if "!FOUND_MSI!"=="0" echo [warn] no MSI produced ^(check tauri.conf.json bundle.targets^)

echo.
echo ^| Built artifacts in %OUT%:
dir /b "%OUT%"
echo.
echo Run standalone:    "%OUT%\ClipBridge.exe"
echo Install ^(NSIS^):    "%OUT%\ClipBridge-Setup.exe"
echo Install ^(MSI^):     msiexec /i "%OUT%\ClipBridge.msi"

endlocal
