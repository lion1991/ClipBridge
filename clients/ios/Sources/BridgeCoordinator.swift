import Combine
import CryptoKit
import UIKit
// Swift glue from clipbridge_core.swift is compiled into this same target,
// so the types (Client, ClipPayload, ClipListener, ConnectionState, …) are
// already in scope and don't need an explicit import.

enum BridgeStatus: Equatable {
    case notPaired
    case connecting
    case connected
    case disconnected
    case error(String)
}

/// Sync coordinator for the main ClipBridge app.
///
/// Holds an audio keepalive so the WebSocket survives backgrounding past
/// iOS's normal suspension window. Whether iOS 17+'s pasted gate then
/// allows pasteboard reads/writes from a non-foreground app is the open
/// question this build is meant to answer; the listener still buffers
/// incoming clips into `recentClips` regardless, so the user can see them
/// on next foreground even if pasted denies the in-background write.
final class BridgeCoordinator: ObservableObject {
    static let shared = BridgeCoordinator()

    private let audio = AudioKeepalive()

    @Published private(set) var status: BridgeStatus = .notPaired
    @Published private(set) var hasPairing: Bool = false
    /// Most recent inbound clips from other devices, newest first, capped
    /// at `recentLimit`. Lives only in memory — keyboard extension has its
    /// own copy. We could share via App Group later if we want unified
    /// history, but for now this just reflects what the main app saw while
    /// it was foreground.
    @Published private(set) var recentClips: [ClipPayload] = []
    /// Outgoing counterpart: clips this device pushed to the relay via the
    /// main app's pasteboard polling loop. Same cap and dedup as
    /// `recentClips`. Doesn't include sends that originate from the
    /// keyboard extension's own polling (separate process, separate state)
    /// — bridging those would need an App Group cache, deferred.
    @Published private(set) var sentClips: [ClipPayload] = []

    private static let recentLimit = 3

    /// Hard cap on outbound image bytes — must match the relay's default
    /// `CLIPBRIDGE_BLOB_MAX_BYTES`. Going over fails fast with a status
    /// message rather than getting silently downscaled.
    private static let maxImageBytes = 32 * 1024 * 1024

    private var client: Client?
    private var listener: Listener?
    private var pollTimer: Timer?
    // `handleIncoming` updates `lastChangeCount` after writing remote clips
    // so the poll tick skips them. We deliberately don't compare strings —
    // doing so would also block the user from re-copying the same text.
    private var lastChangeCount: Int = UIPasteboard.general.changeCount

    /// Off-main worker for HTTP-bound operations (blob upload / download).
    /// Serial so a slow upload can't be lapped by the next poll's upload of
    /// the same clip — also keeps the relay from seeing reordered PUTs.
    private let blobQueue = DispatchQueue(label: "com.clipbridge.blob", qos: .userInitiated)

    /// SHA-256 hashes of recently-seen clipboard content. Inserted on
    /// publish and on receive-write so the next poll round skips both our
    /// own echoes and Apple Universal Clipboard duplicates (which arrive
    /// out-of-band on the same device's UIPasteboard within 1-2s).
    private let seenHashes = RecentHashes()

    private init() {}

    func bootstrap() {
        hasPairing = PairingStore.load() != nil
        status = hasPairing ? .disconnected : .notPaired
        audio.start()
    }

    func applicationDidBecomeActive() {
        // Re-arm audio: the session can be deactivated by iOS during phone
        // calls, Siri, or mediaserverd respawns. Calling start() is
        // idempotent — only does work if something actually died.
        audio.start()
        guard let cfg = PairingStore.load() else {
            stopSync()
            hasPairing = false
            status = .notPaired
            return
        }
        hasPairing = true
        // Reuse existing client if it survived the last backgrounding.
        // Only build a fresh one if there isn't one (first launch, or
        // we lost it via interruption / pairing change).
        if client == nil {
            startSync(with: cfg)
        } else {
            // Audio keepalive kept the WebSocket alive across the last
            // backgrounding, but pasted's foreground gate likely denied any
            // pasteboard writes that happened while we were in background.
            // Re-pulling Recent now (we're foreground again, gate accepts)
            // re-runs handleIncoming and lands the latest clip on the OS
            // pasteboard. Cheap, idempotent — the relay just rebroadcasts
            // its 5-min cache.
            try? client?.fetchRecent()
        }
    }

    func applicationDidEnterBackground() {
        // Audio keepalive keeps us alive — DON'T stopSync. We want the
        // WebSocket to keep delivering clips (visible to the user as
        // updates to recentClips on next foreground). Whether the
        // pasteboard write inside handleIncoming actually lands while
        // we're backgrounded is the open question; the foreground gate
        // in pasted may deny it. Either way, leaving the client running
        // is strictly better than tearing it down.
    }

    // MARK: - Pairing lifecycle

    func savePairing(_ cfg: PairingConfig) {
        PairingStore.save(cfg)
        hasPairing = true
        startSync(with: cfg)
    }

    func resetPairing() {
        stopSync()
        PairingStore.clear()
        hasPairing = false
        status = .notPaired
        // Old recents belong to the previous group; clear so the next
        // pairing's first connect populates from a clean slate.
        recentClips = []
        sentClips = []
    }

    /// Manually pull recent clips from the relay's 5-min cache. Used by
    /// the UI's pull-to-refresh; the relay also pushes Recent automatically
    /// on reconnect, so most of the time this is redundant.
    func refreshRecent() {
        try? client?.fetchRecent()
    }

    private func startSync(with cfg: PairingConfig) {
        stopSync()
        guard let key = cfg.keyData else {
            status = .error("密钥无效")
            return
        }
        let listener = Listener(coordinator: self)
        self.listener = listener
        do {
            client = try Client(
                relayUrl: cfg.relayUrl,
                groupId: cfg.groupId,
                key: key,
                deviceId: PairingStore.deviceId(),
                listener: listener
            )
            status = .connecting
        } catch {
            status = .error("客户端启动失败: \(error)")
            return
        }
        startPolling()
    }

    private func stopSync() {
        pollTimer?.invalidate()
        pollTimer = nil
        client?.stop()
        client = nil
        listener = nil
    }

    // MARK: - Pasteboard

    private func startPolling() {
        pollTimer?.invalidate()
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            self?.checkPasteboard()
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    private func checkPasteboard() {
        let pb = UIPasteboard.general
        let cc = pb.changeCount
        guard cc != lastChangeCount else { return }

        // Image first — screenshots set both image and text reps, but the
        // user almost always wants the picture (text rep is usually the
        // file URL or empty).
        if let image = readClipboardImage() {
            lastChangeCount = cc
            let h = sha256Hex(image.bytes)
            if seenHashes.contains(h) { return }
            seenHashes.insert(h)
            sendImage(image)
            return
        }

        if let text = pb.string, !text.isEmpty {
            lastChangeCount = cc
            let h = sha256Hex(text)
            if seenHashes.contains(h) { return }
            seenHashes.insert(h)
            sendText(text)
            return
        }

        // We saw a new changeCount but couldn't extract anything (likely
        // background read denial, or a type we don't handle). Don't bump
        // lastChangeCount — let the next foreground poll try again. The
        // worst case is busy-polling once per second on an unsupported
        // pasteboard type, which is harmless.
    }

    private func sendText(_ text: String) {
        let payload = ClipPayload(
            kind: .text,
            content: text,
            deviceName: UIDevice.current.name,
            ts: UInt64(Date().timeIntervalSince1970 * 1000),
            image: nil
        )
        do {
            try client?.sendClip(payload: payload)
            appendSent(payload)
        } catch {
            DispatchQueue.main.async { self.status = .error("发送失败: \(error)") }
        }
    }

    private func sendImage(_ image: ClipboardImage) {
        guard image.bytes.count <= Self.maxImageBytes else {
            let mb = image.bytes.count / 1024 / 1024
            DispatchQueue.main.async {
                self.status = .error("图片 \(mb)MB 超过 32MB 上限,未发送")
            }
            return
        }
        let deviceName = UIDevice.current.name
        let ts = UInt64(Date().timeIntervalSince1970 * 1000)
        // Cache the UIImage right now so the sent-card thumbnail can show
        // up immediately, before the upload even starts.
        ImageThumbCache.shared.store(image.uiImage, forTs: ts)

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
                // Build a synthetic ClipPayload mirroring what the wire
                // would carry, so the sent-card UI is consistent with the
                // received-card. We don't have the real ImageMeta back from
                // Rust (send_image returns ()), but for the local UI we
                // only need kind/ts/deviceName + the cached thumbnail.
                let stub = ClipPayload(
                    kind: .image,
                    content: "",
                    deviceName: deviceName,
                    ts: ts,
                    image: ImageMeta(
                        mimeType: image.mime,
                        width: image.width,
                        height: image.height,
                        sizeBytes: UInt64(image.bytes.count),
                        sha256Hex: "",   // local-only, never read back
                        nonceB64: ""
                    )
                )
                self.appendSent(stub)
            } catch {
                DispatchQueue.main.async {
                    self.status = .error("图片发送失败: \(error)")
                }
            }
        }
    }

    /// Mirror of `appendRecent` for the send direction. Records what we
    /// just successfully handed to the Rust client so the UI can show the
    /// last few outbound clips. We intentionally don't trust the relay
    /// echo for this — relay filters by sender_device_id and never echoes
    /// our own clips back, so the listener never sees them.
    private func appendSent(_ payload: ClipPayload) {
        DispatchQueue.main.async {
            var combined = self.sentClips
            if !combined.contains(payload) {
                combined.append(payload)
            }
            combined.sort { $0.ts > $1.ts }
            self.sentClips = Array(combined.prefix(Self.recentLimit))
        }
    }

    fileprivate func handleIncoming(payload: ClipPayload) {
        switch payload.kind {
        case .text:
            DispatchQueue.main.async { self.writeIncomingText(payload) }
        case .image:
            guard let meta = payload.image else { return }
            blobQueue.async { [weak self] in
                self?.fetchAndPasteImage(payload: payload, meta: meta)
            }
        }
    }

    private func writeIncomingText(_ payload: ClipPayload) {
        seenHashes.insert(sha256Hex(payload.content))
        UIPasteboard.general.string = payload.content
        // Capture the post-write changeCount so the next poll tick treats
        // our own write as a no-op instead of re-publishing it.
        lastChangeCount = UIPasteboard.general.changeCount
        appendRecent(payload)
    }

    private func fetchAndPasteImage(payload: ClipPayload, meta: ImageMeta) {
        guard let client = self.client else { return }
        do {
            let bytes = try client.fetchImage(meta: meta)
            let h = sha256Hex(bytes)
            // If this exact image is already on our pasteboard (e.g. UC
            // beat us to it), still record the hash so the next poll
            // doesn't re-publish, but skip the redundant write — we'd
            // just bump changeCount for nothing and clobber any in-flight
            // user action on the same content.
            if seenHashes.contains(h) {
                seenHashes.insert(h)   // refresh TTL
                DispatchQueue.main.async { self.appendRecent(payload) }
                return
            }
            // Decode once on the worker thread so SwiftUI doesn't pay the
            // cost on first display.
            guard let image = UIImage(data: bytes) else {
                throw NSError(domain: "ClipBridge", code: -1,
                              userInfo: [NSLocalizedDescriptionKey: "图片字节解码失败"])
            }
            ImageThumbCache.shared.store(image, forTs: payload.ts)
            seenHashes.insert(h)
            DispatchQueue.main.async {
                UIPasteboard.general.image = image
                self.lastChangeCount = UIPasteboard.general.changeCount
                self.appendRecent(payload)
            }
        } catch {
            DispatchQueue.main.async {
                self.status = .error("图片接收失败: \(error)")
            }
        }
    }

    /// Adds an incoming payload to `recentClips` if it's from another device,
    /// dedup'd by full payload equality (relay re-broadcasts the same Recent
    /// set on every reconnect so we'd otherwise stack duplicates), then
    /// sorts ts-desc and trims to `recentLimit`.
    private func appendRecent(_ payload: ClipPayload) {
        // Filter out our own outbound clips so the user doesn't see what
        // they just copied locally echoed back. Heuristic by deviceName —
        // if two devices share UIDevice.current.name we'd show our own,
        // acceptable edge case.
        guard payload.deviceName != UIDevice.current.name else { return }

        var combined = recentClips
        if !combined.contains(payload) {
            combined.append(payload)
        }
        combined.sort { $0.ts > $1.ts }
        recentClips = Array(combined.prefix(Self.recentLimit))
    }

    fileprivate func handleState(_ state: ConnectionState) {
        DispatchQueue.main.async {
            switch state {
            case .connecting: self.status = .connecting
            case .connected: self.status = .connected
            case .disconnected: self.status = .disconnected
            case .error(let message): self.status = .error(message)
            }
        }
    }
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

private struct ClipboardImage {
    let bytes: Data
    let mime: String
    let width: UInt32
    let height: UInt32
    /// Pre-decoded copy used to seed `ImageThumbCache` on the send path so
    /// the UI doesn't have to re-decode for the thumbnail.
    let uiImage: UIImage
}

/// Bounded TTL set of recently-seen content hashes. Used both to dedup our
/// own writes (prevent the next poll from re-publishing what we just got
/// from the relay) and to absorb Apple Universal Clipboard echoes — when
/// UC syncs the same content Mac↔iPhone in parallel with us, the second
/// arrival lands on `pb` with the same bytes and we'd otherwise re-publish
/// it through the relay, creating extra blob traffic and a brief
/// pasteboard flicker.
///
/// Trade-off: re-copying the exact same content within `ttl` is suppressed,
/// since we can't tell apart "user re-copied to force a re-sync" from "UC
/// just delivered the same bytes again". 5-min TTL is the compromise.
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

/// Read whatever image rep is on the pasteboard and normalize to PNG bytes.
/// Returns nil when no usable image is present (some apps advertise image
/// types as part of a drag promise without actually providing data).
private func readClipboardImage() -> ClipboardImage? {
    let pb = UIPasteboard.general

    // Prefer raw PNG when the pasteboard actually has one — saves a
    // round-trip through UIImage decoding/re-encoding, which would
    // otherwise discard color profiles for some screenshots.
    if let png = pb.data(forPasteboardType: "public.png"),
       !png.isEmpty,
       let img = UIImage(data: png)
    {
        return ClipboardImage(
            bytes: png,
            mime: "image/png",
            width: UInt32(img.size.width.rounded()),
            height: UInt32(img.size.height.rounded()),
            uiImage: img
        )
    }

    // Fall back to the high-level image accessor (covers HEIC, JPEG,
    // synthesized images from drag/drop). Re-encode to PNG so receivers
    // on Android / Windows don't need a HEIC decoder.
    if let img = pb.image, let png = img.pngData() {
        return ClipboardImage(
            bytes: png,
            mime: "image/png",
            width: UInt32(img.size.width.rounded()),
            height: UInt32(img.size.height.rounded()),
            uiImage: img
        )
    }

    return nil
}
