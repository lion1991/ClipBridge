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

final class BridgeCoordinator {
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

    private func sendImage(_ image: ClipboardImage) {
        guard image.bytes.count <= maxImageBytes else {
            let mb = image.bytes.count / 1024 / 1024
            onStateChange(.error("图片 \(mb)MB 超过 32MB 上限,未发送"))
            return
        }
        let deviceName = Self.deviceName
        let ts = nowMillis()
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
                self?.fetchAndPasteImage(meta: meta)
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

    private func fetchAndPasteImage(meta: ImageMeta) {
        guard let client = self.client else { return }
        do {
            let bytes = try client.fetchImage(meta: meta)
            DispatchQueue.main.async { self.writeImage(bytes) }
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
