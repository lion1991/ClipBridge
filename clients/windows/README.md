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
   "Desktop development with C++" workload (provides MSVC + Windows SDK):
   https://visualstudio.microsoft.com/downloads/

4. Icon set is checked in under `icons/`. To regenerate from the master SVG
   (`assets/icon.svg` at the repo root) on macOS:
   ```bash
   ./scripts/regen-icons.sh
   ```
   This refreshes every platform's icon files in one shot — Windows is part
   of that single source of truth.

## Run / debug

```powershell
cd clients\windows
cargo tauri dev
```

Hot-reloads the frontend on save. The tray icon appears; left-click opens the
pairing window.

## Build the standalone EXE

```powershell
cargo tauri build
```

Output:

- `target/release/clipbridge-windows.exe` — single self-contained executable.
  Tauri's `frontendDist` bundling embeds the HTML/CSS/JS into the binary,
  so there's nothing else to ship alongside it.

Bundling MSI / NSIS installers is intentionally disabled
(`bundle.active = false` in `tauri.conf.json`); the EXE is the artifact.
Run `scripts\build-windows-exe.cmd` for the same build plus a copy into
`build\windows\ClipBridge.exe`.

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
