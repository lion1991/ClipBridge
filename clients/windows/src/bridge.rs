//! Owns the `clipbridge_core::Client` and bridges Windows clipboard events
//! to it. Counterpart of the Mac `BridgeCoordinator` and the Android
//! `ClipBridgeAccessibilityService`.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use clipbridge_core::{sha256_hex, Client, ClipKind, ClipListener, ClipPayload, ConnectionState};
use directories::UserDirs;
use image::{GenericImageView, ImageBuffer, ImageFormat, Rgba};
use serde::Serialize;
use tokio::sync::mpsc;

#[cfg(windows)]
use crate::clipboard_listener::ClipboardListener;
use crate::pairing::{PairingConfig, Store};

#[cfg(windows)]
use std::{ffi::OsString, os::windows::ffi::OsStringExt};

#[cfg(windows)]
use windows::Win32::{
    System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    },
    UI::Shell::{DragQueryFileW, HDROP},
};

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

#[derive(Debug, Clone, Serialize)]
pub struct LanPeerDto {
    pub device_id: String,
    pub display_name: String,
    pub candidate_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileTransferHistoryEntry {
    pub id: String,
    pub file_name: String,
    pub device_name: String,
    pub size_bytes: u64,
    pub ts: u64,
    /// "sent" or "received" — UI grouping key.
    pub direction: &'static str,
    /// "sending", "sent", "received", or "failed".
    pub status: &'static str,
    pub path: Option<String>,
    pub message: Option<String>,
}

impl FileTransferHistoryEntry {
    fn pending(id: String, file_name: String, device_name: String, size_bytes: u64) -> Self {
        Self {
            id,
            file_name,
            device_name,
            size_bytes,
            ts: now_millis(),
            direction: "sent",
            status: "sending",
            path: None,
            message: None,
        }
    }
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
    file_tx: mpsc::UnboundedSender<FileTransferHistoryEntry>,
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
    file_history: Arc<Mutex<VecDeque<FileTransferHistoryEntry>>>,

    // Native clipboard listener (Windows only). Held here so its Drop
    // signals the worker thread to stop on `stop()`.
    #[cfg(windows)]
    native_listener: Option<ClipboardListener>,
}

impl Bridge {
    pub fn new(
        state_tx: mpsc::UnboundedSender<UiState>,
        image_tx: mpsc::UnboundedSender<ImageHistoryEntry>,
        file_tx: mpsc::UnboundedSender<FileTransferHistoryEntry>,
    ) -> Self {
        Self {
            client: None,
            listener: None,
            poller: None,
            poll_stop: Arc::new(Mutex::new(false)),
            expected_echo: Arc::new(Mutex::new(None)),
            state_tx,
            image_tx,
            file_tx,
            recent_image_hashes: Arc::new(Mutex::new(VecDeque::new())),
            image_bytes: Arc::new(Mutex::new(HashMap::new())),
            image_history: Arc::new(Mutex::new(VecDeque::new())),
            file_history: Arc::new(Mutex::new(VecDeque::new())),
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
            device_name(),
            listener.clone() as Arc<dyn ClipListener>,
        )
        .map_err(|e| format!("客户端启动失败:{e}"))?;

        let receive_dir = default_file_receive_dir();
        if let Err(e) = fs::create_dir_all(&receive_dir) {
            eprintln!(
                "[clipbridge] failed to create file receive dir {}: {e}",
                receive_dir.display()
            );
        } else {
            client.set_file_receive_dir(receive_dir.display().to_string());
        }

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

    /// Public entry point for picker / drag-drop driven send. Bypasses
    /// the clipboard entirely (so the user's current clipboard isn't
    /// touched) AND bypasses the recent-hashes dedup (an explicit user
    /// action should re-send the same image on purpose; the dedup is for
    /// echo prevention on the clipboard-listener path only).
    ///
    /// We still insert the hash so that any clipboard-listener fire
    /// happening to land in the same instant doesn't double-publish.
    pub fn send_image_bytes(&self, bytes: Vec<u8>) -> Result<ImageHistoryEntry, String> {
        let device_name = device_name();
        let png = normalize_to_png(&bytes).ok_or_else(|| "图片解码失败".to_string())?;
        if png.bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "图片 {}MB 超过 32MB 上限",
                png.bytes.len() / 1024 / 1024
            ));
        }
        if let Some(h) = pixel_hash_hex(&png.bytes) {
            // Insert (return value intentionally ignored — we publish either
            // way), but don't gate on the dedup result.
            let _ = remember_hash(&self.recent_image_hashes, &h);
        }
        let entry = build_history_entry(&png, &device_name, "sent");
        publish_image(self.client.as_ref(), &png, &device_name, entry.ts)?;
        store_history(
            &self.image_history,
            &self.image_bytes,
            &self.image_tx,
            entry.clone(),
            png.bytes,
        );
        Ok(entry)
    }

    pub fn send_image_paths(&self, paths: Vec<PathBuf>) -> Result<Vec<ImageHistoryEntry>, String> {
        let device_name = device_name();
        let image_paths = supported_drag_image_paths(paths.iter());
        if image_paths.is_empty() {
            return Err("没有可发送的图片".to_string());
        }

        let mut entries = Vec::new();
        for path in image_paths {
            let Some(png) = normalize_image_file_at_path(&path) else {
                continue;
            };
            if let Some(h) = pixel_hash_hex(&png.bytes) {
                let _ = remember_hash(&self.recent_image_hashes, &h);
            }
            let entry = build_history_entry(&png, &device_name, "sent");
            publish_image(self.client.as_ref(), &png, &device_name, entry.ts)?;
            store_history(
                &self.image_history,
                &self.image_bytes,
                &self.image_tx,
                entry.clone(),
                png.bytes,
            );
            entries.push(entry);
        }

        if entries.is_empty() {
            return Err("没有可发送的图片".to_string());
        }
        Ok(entries)
    }

    /// Snapshot of the in-process image history for the UI to render
    /// after a hot reload / window close-and-reopen.
    pub fn recent_images(&self) -> Vec<ImageHistoryEntry> {
        self.image_history
            .lock()
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Names of currently-connected LAN peers (HashMap iteration order).
    /// UI sorts and renders inline so the user can see *which* peer
    /// each device thinks it's talking to.
    pub fn lan_peer_names(&self) -> Vec<String> {
        self.client
            .as_ref()
            .map(|c| c.lan_peers())
            .unwrap_or_default()
    }

    pub fn lan_file_peers(&self) -> Vec<LanPeerDto> {
        self.drain_received_files();
        self.client
            .as_ref()
            .map(|c| {
                let mut peers: Vec<_> = c
                    .lan_peer_records()
                    .into_iter()
                    .map(|p| LanPeerDto {
                        device_id: p.device_id,
                        display_name: p.display_name,
                        candidate_count: p.candidate_count,
                    })
                    .collect();
                peers.sort_by(|a, b| {
                    a.display_name
                        .cmp(&b.display_name)
                        .then_with(|| a.device_id.cmp(&b.device_id))
                });
                peers
            })
            .unwrap_or_default()
    }

    pub fn file_receive_dir(&self) -> String {
        default_file_receive_dir().display().to_string()
    }

    pub fn recent_file_transfers(&self) -> Vec<FileTransferHistoryEntry> {
        self.drain_received_files();
        self.file_history
            .lock()
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn send_files_to_peers(
        &self,
        paths: Vec<PathBuf>,
        target_device_ids: Vec<String>,
    ) -> Result<Vec<FileTransferHistoryEntry>, String> {
        let client = self
            .client
            .as_ref()
            .cloned()
            .ok_or_else(|| "客户端未启动".to_string())?;
        let targets: Vec<_> = target_device_ids
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .collect();
        if targets.is_empty() {
            return Err("先选择接收设备".to_string());
        }

        let peer_names: HashMap<_, _> = client
            .lan_peer_records()
            .into_iter()
            .map(|p| (p.device_id, p.display_name))
            .collect();
        let mut final_entries = Vec::new();

        for path in paths {
            let name = file_display_name(&path);
            let metadata = fs::metadata(&path);
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);

            for target in &targets {
                let target_name = peer_names
                    .get(target)
                    .cloned()
                    .unwrap_or_else(|| target.chars().take(12).collect());
                let local_id = uuid::Uuid::new_v4().to_string();
                let pending = FileTransferHistoryEntry::pending(
                    local_id.clone(),
                    name.clone(),
                    target_name.clone(),
                    size,
                );
                store_file_history(&self.file_history, &self.file_tx, pending);

                let final_entry = match metadata.as_ref() {
                    Ok(meta) if meta.is_file() => match client.send_file_to_peer(
                        target.clone(),
                        path.display().to_string(),
                        None,
                    ) {
                        Ok(sent) => FileTransferHistoryEntry {
                            id: local_id,
                            file_name: sent.file_name,
                            device_name: target_name,
                            size_bytes: sent.bytes_sent,
                            ts: now_millis(),
                            direction: "sent",
                            status: "sent",
                            path: None,
                            message: None,
                        },
                        Err(e) => FileTransferHistoryEntry {
                            id: local_id,
                            file_name: name.clone(),
                            device_name: target_name,
                            size_bytes: size,
                            ts: now_millis(),
                            direction: "sent",
                            status: "failed",
                            path: None,
                            message: Some(e.to_string()),
                        },
                    },
                    Ok(_) => FileTransferHistoryEntry {
                        id: local_id,
                        file_name: name.clone(),
                        device_name: target_name,
                        size_bytes: 0,
                        ts: now_millis(),
                        direction: "sent",
                        status: "failed",
                        path: None,
                        message: Some("只能发送普通文件".to_string()),
                    },
                    Err(e) => FileTransferHistoryEntry {
                        id: local_id,
                        file_name: name.clone(),
                        device_name: target_name,
                        size_bytes: 0,
                        ts: now_millis(),
                        direction: "sent",
                        status: "failed",
                        path: None,
                        message: Some(format!("读取失败:{e}")),
                    },
                };
                store_file_history(&self.file_history, &self.file_tx, final_entry.clone());
                final_entries.push(final_entry);
            }
        }

        Ok(final_entries)
    }

    fn drain_received_files(&self) -> Vec<FileTransferHistoryEntry> {
        let Some(client) = self.client.as_ref() else {
            return Vec::new();
        };
        let entries: Vec<_> = client
            .take_received_files()
            .into_iter()
            .map(|file| FileTransferHistoryEntry {
                id: file.transfer_id,
                file_name: file.file_name,
                device_name: "LAN 设备".to_string(),
                size_bytes: file.size_bytes,
                ts: now_millis(),
                direction: "received",
                status: "received",
                path: Some(file.path),
                message: None,
            })
            .collect();
        for entry in &entries {
            store_file_history(&self.file_history, &self.file_tx, entry.clone());
        }
        entries
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
    let Some(h) = pixel_hash_hex(&png.bytes) else {
        return;
    };
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

fn store_file_history(
    file_history: &Arc<Mutex<VecDeque<FileTransferHistoryEntry>>>,
    file_tx: &mpsc::UnboundedSender<FileTransferHistoryEntry>,
    entry: FileTransferHistoryEntry,
) {
    if let Ok(mut h) = file_history.lock() {
        if let Some(pos) = h.iter().position(|e| e.id == entry.id) {
            h.remove(pos);
        }
        h.push_front(entry.clone());
        while h.len() > HISTORY_LIMIT {
            h.pop_back();
        }
    }
    let _ = file_tx.send(entry);
}

fn file_display_name(path: &Path) -> String {
    let raw = path.to_string_lossy();
    raw.rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("file")
        .to_string()
}

pub(crate) fn pathbufs_from_drag_strings(paths: Vec<String>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter_map(|path| {
            let trimmed = path.trim();
            (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
        })
        .collect()
}

fn supported_drag_image_paths<'a, I>(paths: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = &'a PathBuf>,
{
    paths
        .into_iter()
        .filter(|path| is_supported_clipboard_image_file(path))
        .cloned()
        .collect()
}

fn default_file_receive_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|u| u.download_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("ClipBridge")
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    read_clipboard_bitmap_image().or_else(read_clipboard_image_file)
}

fn read_clipboard_bitmap_image() -> Option<NormalizedPng> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let img = clipboard.get_image().ok()?;
    let width = img.width as u32;
    let height = img.height as u32;
    let bytes = img.bytes.into_owned();
    let buffer: ImageBuffer<Rgba<u8>, _> = ImageBuffer::from_raw(width, height, bytes)?;
    let mut png = Vec::with_capacity(buffer.as_raw().len() / 4);
    buffer
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(NormalizedPng {
        bytes: png,
        mime: "image/png".to_string(),
        width,
        height,
    })
}

#[cfg(windows)]
fn read_clipboard_image_file() -> Option<NormalizedPng> {
    normalize_first_image_file(clipboard_file_paths_from_hdrop()?.iter())
}

#[cfg(not(windows))]
fn read_clipboard_image_file() -> Option<NormalizedPng> {
    None
}

#[cfg(windows)]
fn normalize_first_image_file<'a, I>(paths: I) -> Option<NormalizedPng>
where
    I: IntoIterator<Item = &'a PathBuf>,
{
    paths
        .into_iter()
        .find_map(|path| normalize_image_file_at_path(path.as_path()))
}

fn normalize_image_file_at_path(path: &Path) -> Option<NormalizedPng> {
    if !is_supported_clipboard_image_file(path) {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    normalize_to_png(&bytes)
}

fn is_supported_clipboard_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"))
        .unwrap_or(false)
}

#[cfg(windows)]
struct ClipboardGuard;

#[cfg(windows)]
impl ClipboardGuard {
    fn open() -> Option<Self> {
        unsafe { OpenClipboard(None).ok()? };
        Some(Self)
    }
}

#[cfg(windows)]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

#[cfg(windows)]
fn clipboard_file_paths_from_hdrop() -> Option<Vec<PathBuf>> {
    const CF_HDROP_FORMAT: u32 = 15;

    let _guard = ClipboardGuard::open()?;
    unsafe {
        IsClipboardFormatAvailable(CF_HDROP_FORMAT).ok()?;
        let handle = GetClipboardData(CF_HDROP_FORMAT).ok()?;
        let hdrop = HDROP(handle.0);
        if hdrop.is_invalid() {
            return None;
        }

        let count = DragQueryFileW(hdrop, u32::MAX, None);
        if count == 0 {
            return None;
        }

        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let len = DragQueryFileW(hdrop, index, None);
            if len == 0 {
                continue;
            }
            let mut buffer = vec![0u16; len as usize + 1];
            let written = DragQueryFileW(hdrop, index, Some(&mut buffer));
            if written == 0 {
                continue;
            }
            paths.push(PathBuf::from(OsString::from_wide(
                &buffer[..written as usize],
            )));
        }

        (!paths.is_empty()).then_some(paths)
    }
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
            let Some(h) = pixel_hash_hex(&bytes) else {
                return;
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "clipbridge-windows-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_pixel(width, height, Rgba([42u8, 90, 210, 255]));
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .expect("encode test png");
        png
    }

    #[test]
    fn normalizes_copied_png_file_from_path() {
        let dir = temp_dir("png-file");
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("copied.png");
        let original = png_bytes(3, 2);
        fs::write(&path, &original).expect("write png");

        let png = normalize_image_file_at_path(&path).expect("image file should decode");

        assert_eq!(png.mime, "image/png");
        assert_eq!((png.width, png.height), (3, 2));
        assert_eq!(png.bytes, original);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_display_name_uses_path_basename() {
        let path = PathBuf::from(r"C:\Users\matt\Downloads\report.pdf");

        assert_eq!(file_display_name(&path), "report.pdf");
    }

    #[test]
    fn drag_image_paths_keep_supported_images_only() {
        let paths = [
            PathBuf::from(r"C:\drop\photo.PNG"),
            PathBuf::from(r"C:\drop\notes.pdf"),
            PathBuf::from(r"C:\drop\scan.jpeg"),
            PathBuf::from(r"C:\drop\archive"),
        ];

        let names: Vec<_> = supported_drag_image_paths(paths.iter())
            .into_iter()
            .map(|path| file_display_name(&path))
            .collect();

        assert_eq!(names, vec!["photo.PNG", "scan.jpeg"]);
    }

    #[test]
    fn drag_path_strings_ignore_blank_entries() {
        let paths = pathbufs_from_drag_strings(vec![
            String::from(" "),
            String::from(r"C:\drop\report.pdf"),
            String::new(),
        ]);

        assert_eq!(paths, vec![PathBuf::from(r"C:\drop\report.pdf")]);
    }

    #[test]
    fn file_history_upsert_replaces_existing_entry() {
        let history = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending = FileTransferHistoryEntry::pending(
            "local-1".into(),
            "report.pdf".into(),
            "SM-S9380".into(),
            4096,
        );
        let sent = FileTransferHistoryEntry {
            status: "sent",
            ..pending.clone()
        };

        store_file_history(&history, &tx, pending);
        store_file_history(&history, &tx, sent);

        let snapshot: Vec<_> = history.lock().unwrap().iter().cloned().collect();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].status, "sent");
        assert_eq!(snapshot[0].file_name, "report.pdf");
    }
}
