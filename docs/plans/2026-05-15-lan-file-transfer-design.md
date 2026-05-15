# LAN-Only File Transfer Design

## Goal

Add explicit file transfer between paired ClipBridge devices where relay may help peers discover each other, but file metadata and file bytes are transferred only over LAN.

## Confirmed Scope

- Relay may continue to provide group presence and LAN rendezvous candidates.
- File bytes must never be uploaded to relay blob storage or any relay fallback path.
- Users send files from a dedicated file transfer UI, not by monitoring file clipboard contents.
- Sending supports one or more regular files and one or more selected target devices.
- Receiving defaults to automatic save into a platform-specific ClipBridge folder.
- macOS, Windows, Android, and iOS are all in scope.
- iOS only guarantees file send/receive while the main app is open in the foreground.
- First version does not support folders, resumable transfers, iOS background receive, or relay fallback.

## Architecture

The feature is a dedicated LAN file transfer path in `core`, separate from clipboard text/image sync and separate from the relay blob endpoint. Existing LAN discovery remains the source of peer addresses: mDNS and relay-assisted `LanAdvertise`/`LanPeers` populate the same peer map, and the file transfer layer dials selected peers through that map.

The relay does not need new file APIs. It remains responsible only for WebSocket session state and rendezvous candidate distribution. When no reachable LAN peer exists, file sending is disabled or fails with a LAN-specific error instead of falling back to relay upload.

The core LAN protocol is extended with file-specific frames:

- `FileOffer`: transfer id, source device, target device, file name, size, modified time, optional MIME type, and expected sha256.
- `FileAccept`: receiver accepted the offer and is ready to write.
- `FileReject`: receiver refused before bytes were sent.
- `FileChunk`: transfer id, byte offset, and encrypted chunk data.
- `FileComplete`: sender has finished streaming all chunks.
- `FileCancel`: either side canceled or aborted.

Each target device gets an independent transfer session and status. File bytes use a dedicated short-lived LAN TCP connection, not the existing long-lived clipboard control connection. This prevents large transfers from blocking text/image sync and keeps cancellation/failure handling local to one file and one target.

## Data Flow

The sender UI passes selected file paths and target device ids to core. Core validates that every path is a regular file, every target currently has LAN candidates, and every file is within the configured size limit. Core computes sha256 with streaming I/O before sending the offer, so receivers can verify content without trusting extension or MIME metadata.

For each target, core opens a dedicated LAN connection, completes the existing Hello/group-key handshake, and sends `FileOffer`. The receiver checks auto-receive settings, destination writability, available space where available, filename safety, and same-name collision policy. If accepted, it returns `FileAccept`.

The sender streams file contents in bounded chunks. The receiver writes to a temporary `.part` file and emits progress events. After `FileComplete`, the receiver flushes, verifies sha256, and atomically renames the temp file to the final safe file name. On disconnect, cancel, write failure, size mismatch, or hash mismatch, the receiver deletes the `.part` file and emits a failed event.

The first version does not persist partial state. A failed transfer must be restarted from the beginning.

## Storage And Safety

Receivers save to default folders:

- macOS: `~/Downloads/ClipBridge`
- Windows: `%USERPROFILE%\Downloads\ClipBridge`
- Android: a user-visible ClipBridge download/media location where platform permissions allow it
- iOS: app Documents `ClipBridge` folder, visible through Files/share flows where supported

File names are sanitized before writing. Receivers keep only the basename, reject empty names, reject path separators and control characters, and guard against Windows reserved device names. Incoming files never overwrite existing files; name conflicts use `name (1).ext`, `name (2).ext`, and so on.

Resource limits are conservative and explicit:

- Per-file maximum defaults to 2 GiB.
- File chunks fit below the current encrypted LAN frame cap, with 512 KiB as the target chunk size.
- Per-device receive concurrency defaults to 1.
- Total active file transfer concurrency is capped to a small number, such as 2 or 3.
- All reads and writes are streaming; full files are never loaded into memory.

Errors shown to UI must be specific enough for action, such as no LAN device, peer unreachable, destination not writable, rejected by receiver, canceled, connection lost, or hash verification failed. Error messages must not expose full local file paths to other devices.

## Platform UI

macOS extends the existing transfer window with a file tab. The left side shows LAN devices with multi-select and select-all. The right side provides drag/drop and file picker input, plus a transfer list showing file name, size, target device, progress, speed, status, and cancel. Completed inbound transfers offer a Finder reveal action.

Windows adds the same file transfer panel in the Tauri UI. It uses native file dialogs, saves to `Downloads\ClipBridge`, and exposes an open-folder action after completion. The core API should expose stable peer records with `device_id` and display name, not just a list of names, so the UI can target devices deterministically.

Android adds a compact file transfer section in Compose. File selection uses the system picker. Target selection uses a checked list or bottom sheet. Received files appear in the transfer history and should be saved somewhere the user can find through Android file/media surfaces.

iOS adds the feature only to the main SwiftUI app. It uses `DocumentPicker` for sending, saves received files into app Documents, and provides share/open actions. The keyboard extension does not participate in file transfer.

## Testing And Verification

Core tests should cover:

- Successful file transfer over a local TCP or duplex test harness.
- Offer rejection before bytes are sent.
- Filename sanitization and same-name collision handling.
- Connection drop mid-transfer cleans up `.part`.
- Hash mismatch deletes `.part` and reports failure.
- Chunk offset and size validation rejects malformed streams.
- No LAN candidates returns a LAN-specific send error.

Workspace verification gates:

- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- Android unit tests and assemble.
- macOS app build.
- Windows-side validation commands documented when not directly runnable from macOS.

Manual device validation:

- Mac to Android small file.
- Android to Mac large file.
- Mac to Windows.
- iOS foreground send and receive.
- No LAN peer disables or rejects sending without relay fallback.
- Interrupted transfer fails and removes temporary files.

## Implementation Phases

1. Core protocol and transfer engine: data types, LAN frames, file sanitization, transfer events, temp-file writes, sha256 verification, and unit tests.
2. macOS UI: file tab, device selection, send flow, receive history, cancel/reveal actions, and real LAN validation.
3. Windows and Android UI: deterministic peer selection, file pickers, receive storage, progress history, and platform build checks.
4. iOS main app UI: foreground-only send/receive, document picker integration, documents storage, and share/open actions.

## Explicit Non-Goals For First Version

- No folder transfer.
- No zip packing/unpacking.
- No resumable transfer after failure or app restart.
- No background receive guarantee on iOS.
- No relay upload fallback.
- No changes to relay blob storage.
- No automatic file clipboard monitoring.
