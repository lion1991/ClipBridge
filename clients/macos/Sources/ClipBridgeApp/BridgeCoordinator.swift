import AppKit
import ClipbridgeCore
import CryptoKit

/// Owns the Rust `Client`, the pasteboard polling timer, and the bridge between
/// AppKit clipboard events and the Rust core.
///
/// Feedback-loop avoidance: write paths update `lastChangeCount` after pushing
/// the remote clip to the pasteboard, so the next poll tick sees the same
/// `changeCount` and skips. We deliberately do *not* compare strings —
/// that would also block the user from re-copying the same text on purpose.
enum BridgeStatus {
    case notPaired
    case connecting
    case connected
    case disconnected
    case error(String)
}

/// Hard cap on outbound image bytes. Keep in sync with the relay's default
/// `CLIPBRIDGE_BLOB_MAX_BYTES`. Exceeding clips are skipped with a status
/// message rather than being silently downscaled.
private let maxImageBytes = 32 * 1024 * 1024

/// Bounded TTL set of recently-seen content hashes. Used both to dedup our
/// own writes (prevent the next poll from re-publishing what we just got
/// from the relay) and to absorb Apple Universal Clipboard echoes — when
/// UC syncs the same image Mac↔iPhone in parallel with us, the second
/// arrival lands on `pb` with the same bytes and we'd otherwise re-publish
/// it through the relay (creating extra blob traffic and possibly a brief
/// pasteboard flicker).
///
/// Trade-off: re-copying the exact same content within `ttl` is suppressed.
/// In practice users re-copy to "force a re-sync", which is exactly what
/// our changeCount fast-path normally provides — but suppressing that bit
/// of friction is the deliberate cost of UC coexistence.
final class RecentHashes {
    private let capacity: Int
    private let ttl: TimeInterval
    private var entries: [(hash: String, addedAt: Date)] = []
    private let queue = DispatchQueue(label: "com.clipbridge.recent-hashes")

    init(capacity: Int = 32, ttl: TimeInterval = 5 * 60) {
        self.capacity = capacity
        self.ttl = ttl
    }

    func contains(_ hash: String) -> Bool {
        queue.sync {
            prune()
            return entries.contains { $0.hash == hash }
        }
    }

    func insert(_ hash: String) {
        queue.sync {
            prune()
            entries.removeAll { $0.hash == hash }
            entries.append((hash, Date()))
            if entries.count > capacity {
                entries.removeFirst(entries.count - capacity)
            }
        }
    }

    private func prune() {
        let cutoff = Date().addingTimeInterval(-ttl)
        entries.removeAll { $0.addedAt < cutoff }
    }
}

func sha256Hex(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

func sha256Hex(_ s: String) -> String {
    sha256Hex(Data(s.utf8))
}

final class BridgeCoordinator: ObservableObject {
    private let config: PairingConfig
    private let onStateChange: (BridgeStatus) -> Void
    private var client: Client?
    private var listener: Listener?
    private var pollTimer: Timer?

    private var lastChangeCount: Int = NSPasteboard.general.changeCount

    /// Off-main worker for HTTP-bound operations (blob upload / download).
    /// Serial so a slow upload can't be lapped by the next poll's upload of
    /// the same clip — also keeps the relay from seeing reordered PUTs.
    private let blobQueue = DispatchQueue(label: "com.clipbridge.blob", qos: .userInitiated)

    /// SHA-256 hashes of recently-seen clipboard content. Inserted on
    /// publish and on receive-write so the next poll round skips both our
    /// own echoes and Universal Clipboard duplicates.
    private let seenHashes = RecentHashes()

    /// Image traffic history surfaced to the dedicated transfer window.
    /// Newest first, capped at `imageHistoryLimit`. Only includes binary
    /// image clips (text is not surfaced — the menu-bar app doesn't have
    /// a text history view today).
    @Published private(set) var receivedImages: [ImageHistoryEntry] = []
    @Published private(set) var sentImages: [ImageHistoryEntry] = []
    private static let imageHistoryLimit = 12

    private static let deviceId: String = {
        let key = "com.clipbridge.device_id"
        if let id = UserDefaults.standard.string(forKey: key) { return id }
        let id = UUID().uuidString
        UserDefaults.standard.set(id, forKey: key)
        return id
    }()

    private static let deviceName: String = {
        Host.current().localizedName ?? "Mac"
    }()

    init(config: PairingConfig, onStateChange: @escaping (BridgeStatus) -> Void) {
        self.config = config
        self.onStateChange = onStateChange
    }

    func start() {
        guard let key = config.keyData else {
            onStateChange(.error("密钥无效"))
            return
        }
        let listener = Listener(coordinator: self)
        self.listener = listener
        do {
            client = try Client(
                relayUrl: config.relayUrl,
                groupId: config.groupId,
                key: key,
                deviceId: Self.deviceId,
                listener: listener
            )
        } catch {
            onStateChange(.error("客户端错误:\(error)"))
            return
        }
        startPolling()
    }

    func stop() {
        pollTimer?.invalidate()
        pollTimer = nil
        client?.stop()
        client = nil
        listener = nil
    }

    private func startPolling() {
        pollTimer?.invalidate()
        let timer = Timer(timeInterval: 0.5, repeats: true) { [weak self] _ in
            self?.checkPasteboard()
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    private func checkPasteboard() {
        let pb = NSPasteboard.general
        guard pb.changeCount != lastChangeCount else { return }
        lastChangeCount = pb.changeCount

        // Image first: screenshots set both image and text reps, but the
        // user pretty much always wants the picture, not its filename.
        if let image = readClipboardImage() {
            let h = sha256Hex(image.bytes)
            if seenHashes.contains(h) { return }
            seenHashes.insert(h)
            sendImage(image)
            return
        }

        if let text = pb.string(forType: .string), !text.isEmpty {
            let h = sha256Hex(text)
            if seenHashes.contains(h) { return }
            seenHashes.insert(h)
            sendText(text)
        }
    }

    private func sendText(_ text: String) {
        let payload = ClipPayload(
            kind: .text,
            content: text,
            deviceName: Self.deviceName,
            ts: nowMillis(),
            image: nil
        )
        do {
            try client?.sendClip(payload: payload)
        } catch {
            onStateChange(.error("发送失败:\(error)"))
        }
    }

    /// Public entry point for the image-transfer window: send raw image
    /// bytes that the user explicitly handed us (drag-drop or file picker)
    /// without round-tripping through the system pasteboard. Returns
    /// synchronously after queueing — the actual upload happens off the
    /// caller's thread.
    func sendImageFromFile(at url: URL) {
        do {
            let bytes = try Data(contentsOf: url)
            guard let image = NSImage(data: bytes) else {
                onStateChange(.error("无法解码 \(url.lastPathComponent)"))
                return
            }
            // Re-encode to PNG so receivers don't need a HEIC/TIFF/etc
            // decoder. Skip when the source is already PNG to avoid a
            // pointless decode→encode round trip that changes bytes.
            let (pngBytes, mime): (Data, String) = {
                if url.pathExtension.lowercased() == "png" { return (bytes, "image/png") }
                if let rep = NSBitmapImageRep(data: bytes),
                   let png = rep.representation(using: .png, properties: [:])
                {
                    return (png, "image/png")
                }
                return (bytes, "application/octet-stream")
            }()
            let clip = ClipboardImage(
                bytes: pngBytes,
                mime: mime,
                width: UInt32(image.size.width.rounded()),
                height: UInt32(image.size.height.rounded())
            )
            sendImage(clip)
        } catch {
            onStateChange(.error("读取失败:\(error.localizedDescription)"))
        }
    }

    private func sendImage(_ image: ClipboardImage) {
        guard image.bytes.count <= maxImageBytes else {
            let mb = image.bytes.count / 1024 / 1024
            onStateChange(.error("图片 \(mb)MB 超过 32MB 上限,未发送"))
            return
        }
        let deviceName = Self.deviceName
        let ts = nowMillis()
        // Surface in the sent-history list immediately so the transfer
        // window's row appears before the upload finishes (often takes a
        // second or two for a multi-MB image on slow uplinks).
        appendSent(ImageHistoryEntry(
            bytes: image.bytes,
            mime: image.mime,
            width: image.width,
            height: image.height,
            deviceName: deviceName,
            ts: ts
        ))
        blobQueue.async { [weak self] in
            guard let self, let client = self.client else { return }
            do {
                try client.sendImage(
                    imageBytes: image.bytes,
                    mimeType: image.mime,
                    width: image.width,
                    height: image.height,
                    deviceName: deviceName,
                    ts: ts
                )
            } catch {
                DispatchQueue.main.async {
                    self.onStateChange(.error("图片发送失败:\(error)"))
                }
            }
        }
    }

    fileprivate func handleIncoming(payload: ClipPayload) {
        switch payload.kind {
        case .text:
            DispatchQueue.main.async { self.writeText(payload.content) }
        case .image:
            guard let meta = payload.image else { return }
            blobQueue.async { [weak self] in
                self?.fetchAndPasteImage(payload: payload, meta: meta)
            }
        }
    }

    private func writeText(_ text: String) {
        guard !text.isEmpty else { return }
        seenHashes.insert(sha256Hex(text))
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
        // Capture the post-write changeCount so the next poll tick treats
        // our own write as a no-op instead of re-publishing it.
        lastChangeCount = pb.changeCount
    }

    private func fetchAndPasteImage(payload: ClipPayload, meta: ImageMeta) {
        guard let client = self.client else { return }
        do {
            let bytes = try client.fetchImage(meta: meta)
            let entry = ImageHistoryEntry(
                bytes: bytes,
                mime: meta.mimeType,
                width: meta.width,
                height: meta.height,
                deviceName: payload.deviceName,
                ts: payload.ts
            )
            DispatchQueue.main.async {
                self.writeImage(bytes)
                self.appendReceived(entry)
            }
            // Auto-save outside the main queue — file I/O is sync and we
            // don't want the pasteboard write to wait on it.
            autoSaveIfConfigured(entry)
        } catch {
            DispatchQueue.main.async {
                self.onStateChange(.error("图片接收失败:\(error)"))
            }
        }
    }

    private func writeImage(_ data: Data) {
        seenHashes.insert(sha256Hex(data))
        let pb = NSPasteboard.general
        pb.clearContents()
        // Write raw PNG bytes literally. Going through `writeObjects([NSImage])`
        // would make the pasteboard hold an NSImage, and the next poll's
        // `data(forType: .png)` would get an *re-encoded* PNG (different
        // bytes) — sha256 dedup would miss and we'd republish in a loop,
        // ping-ponging the same image at 0.5Hz with each end re-encoding.
        pb.setData(data, forType: .png)
        // Also synthesize raw TIFF bytes for legacy AppKit consumers that
        // only accept .tiff. NSBitmapImageRep takes the PNG buffer happily.
        // Stored as bytes too, so it round-trips the same way as PNG.
        if let rep = NSBitmapImageRep(data: data),
           let tiff = rep.representation(using: .tiff, properties: [:])
        {
            pb.setData(tiff, forType: .tiff)
        }
        lastChangeCount = pb.changeCount
    }

    fileprivate func handleState(_ state: ConnectionState) {
        let mapped: BridgeStatus = switch state {
        case .connecting: .connecting
        case .connected: .connected
        case .disconnected: .disconnected
        case .error(let message): .error(message)
        }
        DispatchQueue.main.async { self.onStateChange(mapped) }
    }

    // MARK: - Image history & auto-save

    private func appendReceived(_ entry: ImageHistoryEntry) {
        receivedImages.insert(entry, at: 0)
        if receivedImages.count > Self.imageHistoryLimit {
            receivedImages.removeLast(receivedImages.count - Self.imageHistoryLimit)
        }
    }

    private func appendSent(_ entry: ImageHistoryEntry) {
        DispatchQueue.main.async {
            self.sentImages.insert(entry, at: 0)
            if self.sentImages.count > Self.imageHistoryLimit {
                self.sentImages.removeLast(self.sentImages.count - Self.imageHistoryLimit)
            }
        }
    }

    /// Drop the in-memory image lists. Called when the user resets pairing
    /// — old images belong to the previous group and the new pairing should
    /// start with a clean history.
    func clearImageHistory() {
        DispatchQueue.main.async {
            self.receivedImages.removeAll()
            self.sentImages.removeAll()
        }
    }

    private func autoSaveIfConfigured(_ entry: ImageHistoryEntry) {
        guard let folder = AppSettings.imageAutoSaveFolder else { return }
        do {
            try FileManager.default.createDirectory(
                at: folder, withIntermediateDirectories: true
            )
            let url = folder.appendingPathComponent(entry.suggestedFilename)
            try entry.bytes.write(to: url)
        } catch {
            DispatchQueue.main.async {
                self.onStateChange(.error("自动保存失败:\(error.localizedDescription)"))
            }
        }
    }
}

/// One image's worth of metadata + bytes for the transfer-window UI. Holds
/// the full payload (capped at 12 entries to bound memory; a typical
/// screenshot is <2 MB so the realistic ceiling is ~24 MB).
struct ImageHistoryEntry: Identifiable, Equatable {
    let id = UUID()
    let bytes: Data
    let mime: String
    let width: UInt32
    let height: UInt32
    let deviceName: String
    let ts: UInt64

    static func == (lhs: ImageHistoryEntry, rhs: ImageHistoryEntry) -> Bool {
        lhs.id == rhs.id
    }

    var sizeLabel: String {
        let kb = max(1, bytes.count / 1024)
        return kb >= 1024
            ? String(format: "%.1f MB", Double(kb) / 1024.0)
            : "\(kb) KB"
    }

    var dimsLabel: String { "\(width)×\(height)" }

    var date: Date { Date(timeIntervalSince1970: TimeInterval(ts) / 1000) }

    /// Filesystem-safe suggested name with a stable ts prefix so files
    /// listed in Finder sort the same way as the in-app history.
    var suggestedFilename: String {
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyyMMdd-HHmmss"
        let stamp = formatter.string(from: date)
        let slug = deviceName
            .replacingOccurrences(of: "/", with: "-")
            .replacingOccurrences(of: " ", with: "_")
        let ext: String = {
            switch mime {
            case "image/png": return "png"
            case "image/jpeg": return "jpg"
            case "image/heic": return "heic"
            default: return "img"
            }
        }()
        return "ClipBridge-\(stamp)-\(slug).\(ext)"
    }
}

/// Lightweight wrapper around UserDefaults for the menu-bar app's
/// preferences. Currently just the auto-save target folder.
enum AppSettings {
    private static let autoSaveFolderKey = "imageAutoSaveFolder"

    /// File-URL the user picked via the folder panel in the transfer
    /// window. Stored as a string path; the unsandboxed Mac app can read
    /// arbitrary paths so we don't need a security-scoped bookmark.
    static var imageAutoSaveFolder: URL? {
        get {
            guard let s = UserDefaults.standard.string(forKey: autoSaveFolderKey),
                  !s.isEmpty else { return nil }
            return URL(fileURLWithPath: s, isDirectory: true)
        }
        set {
            UserDefaults.standard.set(newValue?.path, forKey: autoSaveFolderKey)
        }
    }
}

private struct ClipboardImage {
    let bytes: Data
    let mime: String
    let width: UInt32
    let height: UInt32
}

/// Read whatever image rep is on the pasteboard and normalize to PNG bytes.
/// Returns nil when no usable image is present, even if PNG/TIFF types are
/// advertised but empty (some apps register types as part of a drag promise
/// without actually providing data).
private func readClipboardImage() -> ClipboardImage? {
    let pb = NSPasteboard.general
    let types = pb.types ?? []

    var pngData: Data?
    if types.contains(.png), let data = pb.data(forType: .png), !data.isEmpty {
        pngData = data
    } else if types.contains(.tiff),
              let tiff = pb.data(forType: .tiff),
              !tiff.isEmpty,
              let rep = NSBitmapImageRep(data: tiff),
              let png = rep.representation(using: .png, properties: [:])
    {
        // Both PNG and TIFF are lossless, so re-encoding TIFF→PNG is fine
        // for fidelity. We standardize on PNG so receivers don't need a
        // TIFF decoder (matters for non-Apple platforms).
        pngData = png
    }

    guard let data = pngData, let image = NSImage(data: data) else { return nil }
    let size = image.size
    return ClipboardImage(
        bytes: data,
        mime: "image/png",
        width: UInt32(size.width.rounded()),
        height: UInt32(size.height.rounded())
    )
}

private func nowMillis() -> UInt64 {
    UInt64(Date().timeIntervalSince1970 * 1000)
}

private final class Listener: ClipListener, @unchecked Sendable {
    weak var coordinator: BridgeCoordinator?
    init(coordinator: BridgeCoordinator) { self.coordinator = coordinator }

    func onClip(payload: ClipPayload) {
        coordinator?.handleIncoming(payload: payload)
    }
    func onState(state: ConnectionState) {
        coordinator?.handleState(state)
    }
}
