@echo off
:: Windows build: Tauri 2 (Rust + vanilla JS UI) -> standalone .exe.
::
:: Output:
::   build\windows\ClipBridge.exe   (single self-contained executable)
::
:: Bundling (MSI / NSIS) is intentionally disabled in tauri.conf.json —
:: the user-facing artifact is just the EXE, which embeds its own webview
:: assets via Tauri's frontendDist resource bundling.
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

:: --- 0/2 sanity checks --------------------------------------------------
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

:: --- 1/2 tauri build ----------------------------------------------------
:: Bundle targets are disabled in tauri.conf.json so this only compiles
:: the Rust binary; no installer step runs and no WiX / NSIS toolchain is
:: needed on the build machine.
echo ==^> 1/2 cargo tauri build ^(release^)
pushd "%WIN_DIR%"
cargo tauri build
if errorlevel 1 (
  popd
  echo [ERROR] cargo tauri build failed
  exit /b 1
)
popd

set "REL=%WIN_DIR%\target\release"

:: --- 2/2 standalone exe -------------------------------------------------
echo ==^> 2/2 collecting standalone exe
if not exist "%REL%\clipbridge-windows.exe" (
  echo [ERROR] expected %REL%\clipbridge-windows.exe, not found
  exit /b 1
)
copy /y "%REL%\clipbridge-windows.exe" "%OUT%\ClipBridge.exe" >nul

echo.
echo ^| Built artifact:
dir /b "%OUT%\ClipBridge.exe"
echo.
echo Run:    "%OUT%\ClipBridge.exe"

endlocal
