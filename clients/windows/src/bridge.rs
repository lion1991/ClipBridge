//! Owns the `clipbridge_core::Client` and bridges Windows clipboard events
//! to it. Counterpart of the Mac `BridgeCoordinator` and the Android
//! `ClipBridgeAccessibilityService`.

use std::{
    collections::{HashMap, VecDeque},
    io::Cursor,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use clipbridge_core::{
    sha256_hex, Client, ClipKind, ClipListener, ClipPayload, ConnectionState,
};
use image::{GenericImageView, ImageBuffer, ImageFormat, Rgba};
use serde::Serialize;
use tokio::sync::mpsc;

#[cfg(windows)]
use crate::clipboard_listener::ClipboardListener;
use crate::pairing::{PairingConfig, Store};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum UiState {
    Idle,
    Connecting,
    Connected,
    Disconnected,
    Error { message: String },
}

/// One image's metadata + a small thumbnail for the UI. Full PNG bytes
/// live in `Bridge::image_bytes` keyed by `id` so the IPC payload stays
/// small (a few KB per entry vs MBs).
#[derive(Debug, Clone, Serialize)]
pub struct ImageHistoryEntry {
    /// SHA-256 of the PNG bytes — also the dedup key inside the bytes map.
    pub id: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
    pub device_name: String,
    pub ts: u64,
    /// "received" or "sent" — UI tab routing key.
    pub direction: &'static str,
    /// `data:image/png;base64,...` of a downscaled thumbnail (≤96 px side).
    pub thumbnail: String,
}

/// Window during which a clipboard change matching the most recent remote
/// write is treated as our own echo and skipped. Long enough that the OS
/// `WM_CLIPBOARDUPDATE` (or 500 ms poll tick) fires while we still know it's
/// an echo; short enough that the user can re-copy the same content on
/// purpose.
const ECHO_WINDOW: Duration = Duration::from_secs(10);
/// Hard cap on outbound image bytes — match the relay's default
/// `CLIPBRIDGE_BLOB_MAX_BYTES`. Larger images fail fast with a status
/// message rather than 413 from the relay.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// Bound on the in-process image history surfaced to the UI. Each entry
/// keeps full PNG bytes so save-to-file works without re-fetching the relay.
const HISTORY_LIMIT: usize = 24;

pub struct Bridge {
    client: Option<Arc<Client>>,
    listener: Option<Arc<BridgeListener>>,
    poller: Option<JoinHandle<()>>,
    poll_stop: Arc<Mutex<bool>>,
    expected_echo: Arc<Mutex<Option<(String, Instant)>>>,
    state_tx: mpsc::UnboundedSender<UiState>,
    image_tx: mpsc::UnboundedSender<ImageHistoryEntry>,
    /// Pixel-content hashes (SHA-256 of decoded RGBA) of recent images,
    /// bounded LRU. Inserted on both publish and receive-write so neither
    /// side re-publishes its own write echo.
    recent_image_hashes: Arc<Mutex<VecDeque<String>>>,
    /// Full PNG bytes of every entry surfaced to the UI, keyed by
    /// ImageHistoryEntry.id. Frontend asks us to save by id.
    image_bytes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// All entries we've published over IPC. Replayed when the UI asks
    /// for recent images on startup.
    image_history: Arc<Mutex<VecDeque<ImageHistoryEntry>>>,

    // Native clipboard listener (Windows only). Held here so its Drop
    // signals the worker thread to stop on `stop()`.
    #[cfg(windows)]
    native_listener: Option<ClipboardListener>,
}

impl Bridge {
    pub fn new(
        state_tx: mpsc::UnboundedSender<UiState>,
        image_tx: mpsc::UnboundedSender<ImageHistoryEntry>,
    ) -> Self {
        Self {
            client: None,
            listener: None,
            poller: None,
            poll_stop: Arc::new(Mutex::new(false)),
            expected_echo: Arc::new(Mutex::new(None)),
            state_tx,
            image_tx,
            recent_image_hashes: Arc::new(Mutex::new(VecDeque::new())),
            image_bytes: Arc::new(Mutex::new(HashMap::new())),
            image_history: Arc::new(Mutex::new(VecDeque::new())),
            #[cfg(windows)]
            native_listener: None,
        }
    }

    pub fn start(&mut self, cfg: &PairingConfig) -> Result<(), String> {
        self.stop();
        let key = cfg.key_bytes().ok_or_else(|| "密钥无效".to_string())?;
        let device_id = Store::device_id();

        let listener = Arc::new(BridgeListener {
            state_tx: self.state_tx.clone(),
            image_tx: self.image_tx.clone(),
            expected_echo: self.expected_echo.clone(),
            recent_image_hashes: self.recent_image_hashes.clone(),
            image_bytes: self.image_bytes.clone(),
            image_history: self.image_history.clone(),
            client: Mutex::new(None),
        });

        let client = Client::new(
            cfg.relay_url.clone(),
            cfg.group_id.clone(),
            key,
            device_id,
            listener.clone() as Arc<dyn ClipListener>,
        )
        .map_err(|e| format!("客户端启动失败:{e}"))?;

        // Hand the listener a back-reference to the client so its receive
        // path can call fetch_image. Set after Client::new because the
        // listener is what Client uses to call back, and we need a
        // chicken-and-egg lifecycle here.
        if let Ok(mut g) = listener.client.lock() {
            *g = Some(client.clone());
        }

        self.listener = Some(listener);
        self.client = Some(client);
        self.spawn_clipboard_handler();
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(client) = self.client.take() {
            client.stop();
        }
        self.listener = None;

        // Tear down whichever clipboard input we're using.
        #[cfg(windows)]
        {
            self.native_listener = None;
        }
        if let Ok(mut s) = self.poll_stop.lock() {
            *s = true;
        }
        if let Some(handle) = self.poller.take() {
            let _ = handle.join();
        }
        self.poll_stop = Arc::new(Mutex::new(false));

        let _ = self.state_tx.send(UiState::Idle);
    }

    /// Public entry point for picker-driven send (file-pick or paste-from
    /// disk in the UI). Bypasses the clipboard entirely so the user's
    /// current clipboard contents stay untouched.
    pub fn send_image_bytes(&self, bytes: Vec<u8>) -> Result<ImageHistoryEntry, String> {
        let device_name = device_name();
        let png = normalize_to_png(&bytes).ok_or_else(|| "图片解码失败".to_string())?;
        if png.bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!("图片 {}MB 超过 32MB 上限", png.bytes.len() / 1024 / 1024));
        }
        let h = pixel_hash_hex(&png.bytes).ok_or_else(|| "无法计算像素哈希".to_string())?;
        if remember_hash(&self.recent_image_hashes, &h) {
            return Err("最近已发送过相同内容".to_string());
        }
        let entry = build_history_entry(&png, &device_name, "sent");
        publish_image(
            self.client.as_ref(),
            &png,
            &device_name,
            entry.ts,
        )?;
        store_history(
            &self.image_history,
            &self.image_bytes,
            &self.image_tx,
            entry.clone(),
            png.bytes,
        );
        Ok(entry)
    }

    /// Snapshot of the in-process image history for the UI to render
    /// after a hot reload / window close-and-reopen.
    pub fn recent_images(&self) -> Vec<ImageHistoryEntry> {
        self.image_history
            .lock()
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Look up the full PNG bytes of an entry by id (for save-to-file).
    /// None if the id was evicted under HISTORY_LIMIT pressure.
    pub fn image_bytes_for(&self, id: &str) -> Option<Vec<u8>> {
        self.image_bytes
            .lock()
            .ok()
            .and_then(|m| m.get(id).cloned())
    }

    /// Prefer the OS-native event-driven listener
    /// (`AddClipboardFormatListener`). If creating the message-only window
    /// or registering the listener fails for any reason, fall back to the
    /// 500 ms polling loop so the app still works.
    fn spawn_clipboard_handler(&mut self) {
        #[cfg(windows)]
        {
            let expected_echo = self.expected_echo.clone();
            let client = self.client.clone();
            let device_name = device_name();
            let recent_image_hashes = self.recent_image_hashes.clone();
            let image_history = self.image_history.clone();
            let image_bytes = self.image_bytes.clone();
            let image_tx = self.image_tx.clone();

            match ClipboardListener::start(move || {
                handle_clipboard_change(
                    &client,
                    &expected_echo,
                    &device_name,
                    &recent_image_hashes,
                    &image_history,
                    &image_bytes,
                    &image_tx,
                );
            }) {
                Ok(l) => {
                    self.native_listener = Some(l);
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "[clipbridge] native clipboard listener unavailable, falling back to polling: {e}"
                    );
                }
            }
        }
        self.spawn_poller();
    }

    fn spawn_poller(&mut self) {
        let stop_flag = self.poll_stop.clone();
        let expected_echo = self.expected_echo.clone();
        let client = self.client.clone();
        let device_name = device_name();
        let recent_image_hashes = self.recent_image_hashes.clone();
        let image_history = self.image_history.clone();
        let image_bytes = self.image_bytes.clone();
        let image_tx = self.image_tx.clone();

        let handle = std::thread::Builder::new()
            .name("clipbridge-clipboard-poller".into())
            .spawn(move || {
                let mut last_text: Option<String> = None;
                let mut last_image_hash: Option<String> = None;
                loop {
                    if stop_flag.lock().map(|s| *s).unwrap_or(true) {
                        break;
                    }
                    // Image first — Windows doesn't let us see "what type
                    // is on the clipboard" cheaply; arboard's get_image is
                    // the probe.
                    if let Some(png) = read_clipboard_image() {
                        if let Some(h) = pixel_hash_hex(&png.bytes) {
                            if Some(&h) != last_image_hash.as_ref() {
                                last_image_hash = Some(h.clone());
                                last_text = None;
                                try_publish_image(
                                    &client,
                                    &device_name,
                                    &recent_image_hashes,
                                    &image_history,
                                    &image_bytes,
                                    &image_tx,
                                    png,
                                );
                                std::thread::sleep(Duration::from_millis(500));
                                continue;
                            }
                        }
                    }
                    if let Some(text) = read_clipboard_text() {
                        if !text.is_empty() && Some(&text) != last_text.as_ref() {
                            last_text = Some(text.clone());
                            last_image_hash = None;
                            try_publish_text(&client, &expected_echo, &device_name, text);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .expect("spawn poller");
        self.poller = Some(handle);
    }
}

/// Called from `WM_CLIPBOARDUPDATE` (or, in fallback, from the poller).
/// Tries image first, falls back to text — Windows has no cheap probe for
/// "what's on the clipboard", so we attempt both reads and let arboard's
/// errors filter.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(windows), allow(dead_code))]
fn handle_clipboard_change(
    client: &Option<Arc<Client>>,
    expected_echo: &Arc<Mutex<Option<(String, Instant)>>>,
    device_name: &str,
    recent_image_hashes: &Arc<Mutex<VecDeque<String>>>,
    image_history: &Arc<Mutex<VecDeque<ImageHistoryEntry>>>,
    image_bytes: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
    image_tx: &mpsc::UnboundedSender<ImageHistoryEntry>,
) {
    if let Some(png) = read_clipboard_image() {
        try_publish_image(
            client,
            device_name,
            recent_image_hashes,
            image_history,
            image_bytes,
            image_tx,
            png,
        );
        return;
    }
    if let Some(text) = read_clipboard_text() {
        if !text.is_empty() {
            try_publish_text(client, expected_echo, device_name, text);
        }
    }
}

fn try_publish_text(
    client: &Option<Arc<Client>>,
    expected_echo: &Arc<Mutex<Option<(String, Instant)>>>,
    device_name: &str,
    text: String,
) {
    // If this change matches the most recent remote write (within the echo
    // window), it's our own `arboard` set firing the listener — skip without
    // republishing. Outside the window, treat it as a real user copy so they
    // can re-share the same text on purpose.
    if let Ok(e) = expected_echo.lock() {
        if let Some((s, t)) = e.as_ref() {
            if s == &text && t.elapsed() < ECHO_WINDOW {
                return;
            }
        }
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let payload = ClipPayload {
        kind: ClipKind::Text,
        content: text,
        device_name: device_name.to_string(),
        ts: now,
        image: None,
    };
    if let Some(c) = client.as_ref() {
        let _ = c.send_clip(payload);
    }
}

#[allow(clippy::too_many_arguments)]
fn try_publish_image(
    client: &Option<Arc<Client>>,
    device_name: &str,
    recent_image_hashes: &Arc<Mutex<VecDeque<String>>>,
    image_history: &Arc<Mutex<VecDeque<ImageHistoryEntry>>>,
    image_bytes_map: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
    image_tx: &mpsc::UnboundedSender<ImageHistoryEntry>,
    png: NormalizedPng,
) {
    if png.bytes.len() > MAX_IMAGE_BYTES {
        eprintln!(
            "[clipbridge] image {}B exceeds {} bytes, skipping",
            png.bytes.len(),
            MAX_IMAGE_BYTES
        );
        return;
    }
    let Some(h) = pixel_hash_hex(&png.bytes) else { return };
    if remember_hash(recent_image_hashes, &h) {
        return; // own echo or re-encoded duplicate
    }
    let entry = build_history_entry(&png, device_name, "sent");
    if let Err(e) = publish_image(client.as_ref(), &png, device_name, entry.ts) {
        eprintln!("[clipbridge] sendImage failed: {e}");
        return;
    }
    store_history(image_history, image_bytes_map, image_tx, entry, png.bytes);
}

fn publish_image(
    client: Option<&Arc<Client>>,
    png: &NormalizedPng,
    device_name: &str,
    ts: u64,
) -> Result<(), String> {
    let c = client.ok_or_else(|| "client not started".to_string())?;
    c.send_image(
        png.bytes.clone(),
        png.mime.clone(),
        png.width,
        png.height,
        device_name.to_string(),
        ts,
    )
    .map_err(|e| format!("{e}"))
}

fn store_history(
    image_history: &Arc<Mutex<VecDeque<ImageHistoryEntry>>>,
    image_bytes_map: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
    image_tx: &mpsc::UnboundedSender<ImageHistoryEntry>,
    entry: ImageHistoryEntry,
    bytes: Vec<u8>,
) {
    if let Ok(mut h) = image_history.lock() {
        h.push_front(entry.clone());
        while h.len() > HISTORY_LIMIT {
            if let Some(evicted) = h.pop_back() {
                if let Ok(mut bs) = image_bytes_map.lock() {
                    bs.remove(&evicted.id);
                }
            }
        }
    }
    if let Ok(mut bs) = image_bytes_map.lock() {
        bs.insert(entry.id.clone(), bytes);
    }
    let _ = image_tx.send(entry);
}

/// Insert into the bounded LRU set. Returns true if the hash was already
/// present (caller should skip), false if newly inserted.
fn remember_hash(set: &Arc<Mutex<VecDeque<String>>>, hash: &str) -> bool {
    const CAP: usize = 32;
    let Ok(mut q) = set.lock() else { return false };
    if let Some(pos) = q.iter().position(|h| h == hash) {
        q.remove(pos);
        q.push_back(hash.to_string());
        return true;
    }
    q.push_back(hash.to_string());
    while q.len() > CAP {
        q.pop_front();
    }
    false
}

fn read_clipboard_text() -> Option<String> {
    // arboard sometimes fails transiently when another app holds the
    // clipboard open; treat that as "no change yet" and let the next event /
    // poll succeed.
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

/// Normalised image data ready for blob upload + history storage.
struct NormalizedPng {
    bytes: Vec<u8>,
    mime: String,
    width: u32,
    height: u32,
}

/// Read whatever image is currently on the clipboard, encoded as PNG.
/// Returns None when the clipboard has no image, the read fails, or the
/// PNG encoding fails (rare, but arboard hands us non-stride RGBA so a
/// malformed buffer would).
fn read_clipboard_image() -> Option<NormalizedPng> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let img = clipboard.get_image().ok()?;
    let width = img.width as u32;
    let height = img.height as u32;
    let bytes = img.bytes.into_owned();
    let buffer: ImageBuffer<Rgba<u8>, _> = ImageBuffer::from_raw(width, height, bytes)?;
    let mut png = Vec::with_capacity(buffer.as_raw().len() / 4);
    buffer.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).ok()?;
    Some(NormalizedPng {
        bytes: png,
        mime: "image/png".to_string(),
        width,
        height,
    })
}

/// Decode arbitrary image bytes (PNG/JPEG/etc.) → re-encode as PNG. Used
/// for the picker path where the user hands us a file from disk.
fn normalize_to_png(bytes: &[u8]) -> Option<NormalizedPng> {
    // Fast path — already PNG.
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        let img = image::load_from_memory(bytes).ok()?;
        let (w, h) = img.dimensions();
        return Some(NormalizedPng {
            bytes: bytes.to_vec(),
            mime: "image/png".to_string(),
            width: w,
            height: h,
        });
    }
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(NormalizedPng {
        bytes: png,
        mime: "image/png".to_string(),
        width,
        height,
    })
}

/// Write PNG bytes to the system clipboard as an image. Decodes to RGBA
/// so arboard can hand it to Windows as CF_DIBV5 / CF_DIB.
fn write_image_to_clipboard(png_bytes: &[u8]) -> Option<()> {
    let img = image::load_from_memory(png_bytes).ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let data = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: rgba.into_raw().into(),
    };
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.set_image(data).ok()
}

fn pixel_hash_hex(png_bytes: &[u8]) -> Option<String> {
    let img = image::load_from_memory(png_bytes).ok()?;
    let rgba = img.to_rgba8();
    Some(sha256_hex(rgba.as_raw()))
}

fn build_history_entry(
    png: &NormalizedPng,
    device_name: &str,
    direction: &'static str,
) -> ImageHistoryEntry {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let id = sha256_hex(&png.bytes);
    let thumbnail = thumbnail_data_url(&png.bytes, 96).unwrap_or_default();
    ImageHistoryEntry {
        id,
        mime: png.mime.clone(),
        width: png.width,
        height: png.height,
        size_bytes: png.bytes.len() as u64,
        device_name: device_name.to_string(),
        ts,
        direction,
        thumbnail,
    }
}

/// Downscale to ≤max_side, encode as PNG, base64 in a data: URL. Used to
/// keep IPC payloads to the frontend small (a few KB per image).
fn thumbnail_data_url(png_bytes: &[u8], max_side: u32) -> Option<String> {
    let img = image::load_from_memory(png_bytes).ok()?;
    let (w, h) = img.dimensions();
    let scale = (max_side as f32 / w.max(h) as f32).min(1.0);
    let new_w = ((w as f32 * scale) as u32).max(1);
    let new_h = ((h as f32 * scale) as u32).max(1);
    let thumb = img.thumbnail(new_w, new_h);
    let mut png = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(format!("data:image/png;base64,{}", B64.encode(&png)))
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Windows".to_string())
}

struct BridgeListener {
    state_tx: mpsc::UnboundedSender<UiState>,
    image_tx: mpsc::UnboundedSender<ImageHistoryEntry>,
    expected_echo: Arc<Mutex<Option<(String, Instant)>>>,
    recent_image_hashes: Arc<Mutex<VecDeque<String>>>,
    image_bytes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    image_history: Arc<Mutex<VecDeque<ImageHistoryEntry>>>,
    /// Back-reference for fetch_image on the receive path.
    /// Set by `Bridge::start` after `Client::new` returns. Behind a Mutex
    /// instead of OnceCell so we can clear it on stop without rebuilding
    /// the listener.
    client: Mutex<Option<Arc<Client>>>,
}

impl ClipListener for BridgeListener {
    fn on_clip(&self, payload: ClipPayload) {
        match payload.kind {
            ClipKind::Text => {
                // Mark this content as "expected echo" *before* writing so
                // the WM_CLIPBOARDUPDATE callback (which fires after
                // `set_text`) recognises its own write and skips republish.
                if let Ok(mut g) = self.expected_echo.lock() {
                    *g = Some((payload.content.clone(), Instant::now()));
                }
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    let _ = clipboard.set_text(&payload.content);
                }
            }
            ClipKind::Image => self.handle_remote_image(payload),
        }
    }

    fn on_state(&self, state: ConnectionState) {
        let mapped = match state {
            ConnectionState::Connecting => UiState::Connecting,
            ConnectionState::Connected => UiState::Connected,
            ConnectionState::Disconnected => UiState::Disconnected,
            ConnectionState::Error { message } => UiState::Error { message },
        };
        let _ = self.state_tx.send(mapped);
    }
}

impl BridgeListener {
    fn handle_remote_image(&self, payload: ClipPayload) {
        let Some(meta) = payload.image else { return };
        // Snapshot the client ref outside any spawned thread to keep
        // lifetimes simple — fetch_image is blocking but cheap to call
        // from a worker thread.
        let client = match self.client.lock() {
            Ok(g) => g.as_ref().cloned(),
            Err(_) => None,
        };
        let Some(client) = client else { return };

        let recent = self.recent_image_hashes.clone();
        let history = self.image_history.clone();
        let bytes_map = self.image_bytes.clone();
        let image_tx = self.image_tx.clone();
        let device_name = payload.device_name.clone();
        let ts = payload.ts;
        let meta_clone = meta.clone();

        std::thread::spawn(move || {
            let bytes = match client.fetch_image(meta_clone) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[clipbridge] fetch_image failed: {e}");
                    return;
                }
            };
            let Some(h) = pixel_hash_hex(&bytes) else { return };
            if remember_hash(&recent, &h) {
                return; // already on our clipboard / in history
            }
            // Mirror the publish-side history entry so the UI sees both
            // directions in a single feed.
            let png = NormalizedPng {
                bytes: bytes.clone(),
                mime: meta.mime_type.clone(),
                width: meta.width,
                height: meta.height,
            };
            let entry = ImageHistoryEntry {
                id: h.clone(),
                mime: png.mime.clone(),
                width: png.width,
                height: png.height,
                size_bytes: png.bytes.len() as u64,
                device_name,
                ts,
                direction: "received",
                thumbnail: thumbnail_data_url(&png.bytes, 96).unwrap_or_default(),
            };
            store_history(&history, &bytes_map, &image_tx, entry, bytes.clone());
            // Push to the system clipboard last so the WM_CLIPBOARDUPDATE
            // callback's hash check finds `h` already in the LRU.
            let _ = write_image_to_clipboard(&bytes);
        });

        // Drop the original `meta` to keep the borrow checker happy when
        // the closure captures `meta_clone`.
        let _ = meta;
    }
}
