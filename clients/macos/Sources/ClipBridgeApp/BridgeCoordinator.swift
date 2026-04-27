import AppKit
import ClipbridgeCore

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
            sendImage(image)
            return
        }

        if let text = pb.string(forType: .string), !text.isEmpty {
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
        let pb = NSPasteboard.general
        pb.clearContents()
        // Hand AppKit an NSImage so consumers (Preview, Notes, Slack,
        // Photoshop, …) get whichever pasteboard type they ask for —
        // PNG, TIFF, or the lazy NSImage rep.
        if let image = NSImage(data: data) {
            pb.writeObjects([image])
        } else {
            // Non-image bytes we can't decode. Last-resort: stash as PNG so
            // at least Cmd-V into a file viewer pulls something out.
            pb.setData(data, forType: .png)
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
