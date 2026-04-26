# ClipBridge — Windows tray client

A Tauri 2 app that runs in the Windows tray and bridges the local clipboard to
the shared `clipbridge-core` Rust library. Same pairing format / wire protocol
as the macOS and Android clients.

## First-time build setup (Windows)

1. Install **Rust** with the MSVC toolchain (default on Windows):
   https://rustup.rs/

2. Install the Tauri CLI:
   ```powershell
   cargo install tauri-cli --version "^2.0" --locked
   ```

3. Install Microsoft **Visual Studio Build Tools** with the
   "Desktop development with C++" workload (provides MSVC, Windows SDK, WiX
   support that Tauri needs to bundle MSI installers):
   https://visualstudio.microsoft.com/downloads/

4. Generate the icon set from the source SVG (one-time):
   ```powershell
   cd clients\windows
   cargo tauri icon icons\source.svg
   ```
   This produces `icons/32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.ico`,
   and a few platform-specific variants.

## Run / debug

```powershell
cd clients\windows
cargo tauri dev
```

Hot-reloads the frontend on save. The tray icon appears; left-click opens the
pairing window.

## Build a distributable installer

```powershell
cargo tauri build
```

Outputs:

- `target/release/clipbridge-windows.exe` — standalone executable
- `target/release/bundle/msi/ClipBridge_<version>_x64_en-US.msi` — Windows
  Installer
- `target/release/bundle/nsis/ClipBridge_<version>_x64-setup.exe` — NSIS
  installer (smaller, more user-friendly)

Both installers are configured to support `zh-CN` and `en-US` UI.

## What it does

- **Tray icon** — left-click opens the pairing window, right-click shows
  "打开配对窗口…" / "退出 ClipBridge".
- **Closing the X button** hides the window; the app stays in the tray.
- **Pairing window** — generate a QR locally for another device to scan, or
  paste a JSON config received from another device.
- **Clipboard polling** — 500 ms loop using `arboard`. When the local clipboard
  changes and the new value isn't something we just received from the relay,
  it's encrypted and sent.
- **Receiving** — the `clipbridge_core::Client` listener writes inbound clips
  to the system clipboard and tracks the value to suppress the next polling
  echo.

## Autostart at login

The bundled `tauri-plugin-autostart` plugin can register the app under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Hooking a UI toggle for
this is on the to-do list — for now you can add it manually via Task Scheduler
or the Startup folder (`shell:startup` in the Run dialog).

## Project layout

```
clients/windows/
├── Cargo.toml           # Tauri 2 + clipbridge-core path dependency
├── tauri.conf.json      # bundle / window configuration
├── build.rs             # tauri-build invocation
├── src/
│   ├── main.rs          # Tauri setup, tray, commands, lifecycle
│   ├── bridge.rs        # owns Client + clipboard poller
│   └── pairing.rs       # PairingConfig + on-disk persistence
├── ui/                  # vanilla HTML / CSS / JS (no npm)
│   ├── index.html
│   ├── style.css
│   └── app.js
├── icons/
│   └── source.svg       # source for `cargo tauri icon …`
└── README.md
```

## Status

- Tray + pairing UI: implemented
- Clipboard polling: implemented (500 ms)
- QR generation: server-side via `qrcode` crate, rendered as inline SVG
- Connection state pill: live via Tauri events
- **Not yet:** native event-driven clipboard listener (`AddClipboardFormatListener`),
  tray icon recolouring per state, autostart UI toggle, dark/light Mica.
