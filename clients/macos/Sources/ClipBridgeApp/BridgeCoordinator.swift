import AppKit
import ClipbridgeCore
import CryptoKit
import UniformTypeIdentifiers

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

/// Pixel-content hash for image bytes — invariant to encoder differences.
/// Decodes the image, draws it into a known 32-bit RGBA bitmap, hashes that.
///
/// Why not just hash the file bytes? PNG / JPEG / HEIC all have multiple
/// valid encodings of the same pixel grid (different compression levels,
/// metadata blocks, idat chunk sizes). Apple's Universal Clipboard appears
/// to round-trip images through CoreImage on at least one hop, producing
/// byte-different but pixel-identical files. Without pixel hashing, our
/// dedup misses the UC echo and we publish the "same" image as if it were
/// new — visible as duplicate rows in the transfer window's recent lists.
///
/// Cost: one decode + one CGContext draw + sha256 of W·H·4 bytes. For a
/// typical 1MP screenshot ~10ms; for a full-screen 14" MBP retina image
/// ~80ms. Acceptable at our 0.5Hz poll cadence.
func imagePixelHashHex(_ data: Data) -> String? {
    guard let rep = NSBitmapImageRep(data: data),
          let cg = rep.cgImage
    else { return nil }
    let width = cg.width
    let height = cg.height
    guard width > 0, height > 0 else { return nil }
    let bytesPerRow = width * 4
    var buffer = Data(count: height * bytesPerRow)
    let colorSpace = CGColorSpaceCreateDeviceRGB()
    let bitmapInfo: UInt32 =
        CGImageAlphaInfo.premultipliedLast.rawValue
        | CGBitmapInfo.byteOrder32Big.rawValue
    let drewOK: Bool = buffer.withUnsafeMutableBytes { raw -> Bool in
        guard let ctx = CGContext(
            data: raw.baseAddress,
            width: width,
            height: height,
            bitsPerComponent: 8,
            bytesPerRow: bytesPerRow,
            space: colorSpace,
            bitmapInfo: bitmapInfo
        ) else { return false }
        ctx.draw(cg, in: CGRect(x: 0, y: 0, width: width, height: height))
        return true
    }
    guard drewOK else { return nil }
    return sha256Hex(buffer)
}

final class BridgeCoordinator: ObservableObject {
    private let config: PairingConfig
    private let onStateChange: (BridgeStatus) -> Void
    private var client: Client?
    private var listener: Listener?
    private var pollTimer: Timer?

    private var lastChangeCount: Int = NSPasteboard.general.changeCount
    private var pasteboardImageScanInFlight = false

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

    /// Most recent ConnectionState — used to revert after a transient error
    /// auto-clears. See `showTransientError`.
    private var lastConnectionStatus: BridgeStatus = .disconnected
    private var transientErrorToken: UInt64 = 0

    /// "Don't touch the pasteboard until this date" — set after every
    /// outbound send AND every inbound write. Apple's Universal Clipboard
    /// re-delivers the same content to this device's pasteboard within
    /// 1-2 seconds; reading inside that window risks publishing the UC-
    /// re-encoded copy back to the relay (creates a duplicate row on the
    /// other device, possibly looping). Inside the window we still bump
    /// lastChangeCount so we don't keep re-evaluating the UC drop.
    private var quietPasteboardUntil: Date = .distantPast
    private static let quietWindow: TimeInterval = 5

    private func enterQuietWindow() {
        let candidate = Date().addingTimeInterval(Self.quietWindow)
        if candidate > quietPasteboardUntil {
            quietPasteboardUntil = candidate
        }
    }

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

    /// Number of LAN peers we currently have a direct mDNS-discovered TCP
    /// session to. 0 means LAN didn't find anyone (or didn't start at all
    /// because Local Network permission was declined). Drives the menu-bar
    /// transport badge.
    var lanPeerCount: UInt32 {
        client?.lanPeerCount() ?? 0
    }

    /// Names of currently-connected LAN peers. Used to render the menu
    /// row as "局域网: Mac mini, iPhone" so mesh asymmetry is visible.
    var lanPeerNames: [String] {
        client?.lanPeers() ?? []
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
                deviceName: Self.deviceName,
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
        let changeCount = pb.changeCount
        guard changeCount != lastChangeCount else { return }

        // Apple Universal Clipboard echo guard. We've just sent or
        // received a clip via ClipBridge; UC is independently delivering
        // (probably the same) content to this pasteboard within 1-2s.
        // Reading + publishing now would either bounce our own clip back
        // to the source device (visible as a duplicate row) or cycle
        // through repeated re-encodings. Skip the read entirely.
        if Date() < quietPasteboardUntil {
            lastChangeCount = changeCount
            return
        }

        // Image first: screenshots set both image and text reps, but the
        // user pretty much always wants the picture, not its filename.
        //
        // Decoding TIFF/PNG and computing the pixel hash can take tens or
        // hundreds of milliseconds for large screenshots or Universal
        // Clipboard-promised data. Keep that work off the AppKit thread so
        // the menu-bar app does not beachball during the 0.5Hz poll.
        let types = pb.types ?? []
        if types.contains(.png) || types.contains(.tiff) {
            guard !pasteboardImageScanInFlight else { return }
            lastChangeCount = changeCount
            processPasteboardImageAsync(changeCount: changeCount)
            return
        }

        lastChangeCount = changeCount
        if let text = pb.string(forType: .string), !text.isEmpty {
            processPasteboardText(text)
        }
    }

    private func processPasteboardImageAsync(changeCount: Int) {
        pasteboardImageScanInFlight = true
        blobQueue.async { [weak self] in
            let image = readClipboardImage()
            let hash = image.map { imagePixelHashHex($0.bytes) ?? sha256Hex($0.bytes) }
            DispatchQueue.main.async {
                guard let self else { return }
                self.pasteboardImageScanInFlight = false
                guard NSPasteboard.general.changeCount == changeCount else { return }
                guard let image, let hash else {
                    if let text = NSPasteboard.general.string(forType: .string), !text.isEmpty {
                        self.processPasteboardText(text)
                    }
                    return
                }
                if self.seenHashes.contains(hash) { return }
                self.seenHashes.insert(hash)
                self.sendImage(image)
            }
        }
    }

    private func processPasteboardText(_ text: String) {
        let h = sha256Hex(text)
        if seenHashes.contains(h) { return }
        seenHashes.insert(h)
        sendText(text)
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
            enterQuietWindow()
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
            guard let clip = clipboardImage(from: bytes, sourceExtension: url.pathExtension) else {
                onStateChange(.error("无法解码 \(url.lastPathComponent)"))
                return
            }
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
                DispatchQueue.main.async { self.enterQuietWindow() }
            } catch {
                self.showTransientError("图片发送失败: \(self.friendlyErrorMessage(error))")
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
        enterQuietWindow()
    }

    private func fetchAndPasteImage(payload: ClipPayload, meta: ImageMeta) {
        guard let client = self.client else { return }
        do {
            let bytes = try client.fetchImage(meta: meta)
            let pixelHash = imagePixelHashHex(bytes)
            let h = pixelHash ?? sha256Hex(bytes)
            // Same content already on our pasteboard / in our history — skip
            // the write and the history append. Catches the UC bounce-back
            // where iPhone re-encoded our image and broadcast it back.
            if seenHashes.contains(h) {
                seenHashes.insert(h)   // refresh TTL
                return
            }
            seenHashes.insert(h)
            let entry = ImageHistoryEntry(
                bytes: bytes,
                mime: meta.mimeType,
                width: meta.width,
                height: meta.height,
                deviceName: payload.deviceName,
                ts: payload.ts
            )
            let tiff = tiffRep(from: bytes)
            DispatchQueue.main.async {
                self.applyImageToPasteboard(png: bytes, tiff: tiff, pixelHash: pixelHash)
                self.appendReceived(entry)
            }
            // Auto-save outside the main queue — file I/O is sync and we
            // don't want the pasteboard write to wait on it.
            autoSaveIfConfigured(entry)
        } catch {
            showTransientError("图片接收失败: \(friendlyErrorMessage(error))")
        }
    }

    /// Re-place a history entry's image on the system pasteboard for a
    /// manual ⌘V (the "再粘贴" button). This must route through the
    /// coordinator rather than a raw pasteboard write in the view:
    ///
    ///  1. The TIFF re-encode (`NSBitmapImageRep` → uncompressed TIFF) is
    ///     heavy and, done synchronously on the SwiftUI button action, would
    ///     freeze the whole app on large images. We do it on `blobQueue`.
    ///  2. Without the seenHashes / lastChangeCount / quiet-window guards
    ///     below, the 0.5Hz poll would treat the re-paste as a fresh local
    ///     copy and re-send the image to every peer (duplicate rows on the
    ///     other device, possibly looping via Universal Clipboard).
    func rePasteImageToClipboard(_ data: Data) {
        blobQueue.async { [weak self] in
            let pixelHash = imagePixelHashHex(data)
            let tiff = self?.tiffRep(from: data)
            DispatchQueue.main.async {
                self?.applyImageToPasteboard(png: data, tiff: tiff, pixelHash: pixelHash)
            }
        }
    }

    /// PNG bytes → uncompressed TIFF bytes for legacy AppKit consumers that
    /// only accept `.tiff`. `nil` if the buffer won't decode. Decoding +
    /// re-encoding here is the expensive part; keep callers off the main
    /// thread for anything user-sized.
    private func tiffRep(from png: Data) -> Data? {
        guard let rep = NSBitmapImageRep(data: png) else { return nil }
        return rep.representation(using: .tiff, properties: [:])
    }

    /// Guarded pasteboard write shared by inbound delivery and manual
    /// re-paste. Must be called on the main thread.
    private func applyImageToPasteboard(png data: Data, tiff: Data?, pixelHash: String?) {
        // Insert the pixel hash so the next poll's pixel-hash check still
        // matches — and falls through to byte hash as a belt-and-suspenders
        // for the case where the same bytes literally come back (no UC
        // re-encoding) and the receive path's pixel-hash already sat in
        // the set.
        if let pixelHash { seenHashes.insert(pixelHash) }
        seenHashes.insert(sha256Hex(data))
        let pb = NSPasteboard.general
        pb.clearContents()
        // Write raw PNG bytes literally. Going through `writeObjects([NSImage])`
        // would make the pasteboard hold an NSImage, and the next poll's
        // `data(forType: .png)` would get an *re-encoded* PNG (different
        // bytes) — sha256 dedup would miss and we'd republish in a loop,
        // ping-ponging the same image at 0.5Hz with each end re-encoding.
        pb.setData(data, forType: .png)
        if let tiff { pb.setData(tiff, forType: .tiff) }
        lastChangeCount = pb.changeCount
        enterQuietWindow()
    }

    fileprivate func handleState(_ state: ConnectionState) {
        let mapped: BridgeStatus = switch state {
        case .connecting: .connecting
        case .connected: .connected
        case .disconnected: .disconnected
        case .error(let message): .error(message)
        }
        DispatchQueue.main.async {
            self.lastConnectionStatus = mapped
            self.onStateChange(mapped)
        }
    }

    /// Show a non-fatal error, then auto-revert to the last known WS state.
    /// For hiccups (stale blob refs, single failed send) — sticky errors
    /// would make the menu-bar look perpetually broken even after recovery.
    private func showTransientError(_ message: String, seconds: TimeInterval = 4) {
        DispatchQueue.main.async {
            self.transientErrorToken &+= 1
            let token = self.transientErrorToken
            self.onStateChange(.error(message))
            DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { [weak self] in
                guard let self else { return }
                guard self.transientErrorToken == token else { return }
                self.onStateChange(self.lastConnectionStatus)
            }
        }
    }

    private func friendlyErrorMessage(_ error: Error) -> String {
        let text = String(describing: error)
        if text.contains("BlobNotFound") {
            return "图片已过期 (relay 默认 5 分钟保留, 请源设备重新复制)"
        }
        if text.contains("BlobTooLarge") {
            return "图片超过 relay 单条上限"
        }
        return text
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

private let filenamesPasteboardType = NSPasteboard.PasteboardType("NSFilenamesPboardType")

/// Read whatever image rep is on the pasteboard and normalize to PNG bytes.
/// Returns nil when no usable image is present, even if PNG/TIFF types are
/// advertised but empty (some apps register types as part of a drag promise
/// without actually providing data).
private func readClipboardImage() -> ClipboardImage? {
    let pb = NSPasteboard.general
    let types = pb.types ?? []

    // Finder file-copy pasteboards often include both `public.file-url` and
    // `public.png`. The PNG can be the Finder document icon preview rather
    // than the actual file bytes, so prefer the real file URL when present.
    if let fromFile = readImageFileFromPasteboard(pb, types: types) {
        return fromFile
    }

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

    guard let data = pngData else { return nil }
    return clipboardImage(from: data, sourceExtension: "png")
}

private func readImageFileFromPasteboard(
    _ pb: NSPasteboard,
    types: [NSPasteboard.PasteboardType]
) -> ClipboardImage? {
    var urls: [URL] = []

    if types.contains(.fileURL),
       let s = pb.string(forType: .fileURL),
       let url = URL(string: s),
       url.isFileURL {
        urls.append(url)
    }

    if let items = pb.pasteboardItems {
        for item in items {
            guard let s = item.string(forType: .fileURL),
                  let url = URL(string: s),
                  url.isFileURL else { continue }
            urls.append(url)
        }
    }

    if let paths = pb.propertyList(forType: filenamesPasteboardType) as? [String] {
        urls.append(contentsOf: paths.map { URL(fileURLWithPath: $0) })
    }

    var seen = Set<String>()
    for url in urls where seen.insert(url.path).inserted {
        guard let type = UTType(filenameExtension: url.pathExtension),
              type.conforms(to: .image),
              let data = try? Data(contentsOf: url),
              let image = clipboardImage(from: data, sourceExtension: url.pathExtension) else {
            continue
        }
        return image
    }

    return nil
}

private func clipboardImage(from data: Data, sourceExtension: String?) -> ClipboardImage? {
    guard let image = NSImage(data: data) else { return nil }
    let normalized: (bytes: Data, mime: String) = {
        if sourceExtension?.lowercased() == "png" {
            return (data, "image/png")
        }
        if let rep = NSBitmapImageRep(data: data),
           let png = rep.representation(using: .png, properties: [:]) {
            return (png, "image/png")
        }
        if let tiff = image.tiffRepresentation,
           let rep = NSBitmapImageRep(data: tiff),
           let png = rep.representation(using: .png, properties: [:]) {
            return (png, "image/png")
        }
        return (data, "application/octet-stream")
    }()
    let size = image.size
    return ClipboardImage(
        bytes: normalized.bytes,
        mime: normalized.mime,
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
