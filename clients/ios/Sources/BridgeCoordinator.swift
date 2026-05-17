import Combine
import CryptoKit
import UIKit
import UniformTypeIdentifiers
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
    /// Number of LAN peers (mDNS-discovered, fully-handshaked) the Rust
    /// `Client` currently has a direct TCP session to. Polled every 2s
    /// from `pollTimer` so SwiftUI can render a transport badge.
    @Published private(set) var lanPeerCount: UInt32 = 0
    /// Names of currently-connected LAN peers, sorted. Empty when LAN
    /// hasn't found anyone (or didn't start). Drives the StatusPill's
    /// "局域网: Mac mini, MacBook" suffix so the user can spot mesh
    /// asymmetry across devices at a glance.
    @Published private(set) var lanPeerNames: [String] = []
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
    @Published private(set) var lanFilePeers: [LanPeerRecord] = []
    @Published private(set) var fileTransferHistory: [FileTransferHistoryEntry] = []

    private static let recentLimit = 3
    private static let fileHistoryLimit = 40

    /// Hard cap on outbound image bytes — must match the relay's default
    /// `CLIPBRIDGE_BLOB_MAX_BYTES`. Going over fails fast with a status
    /// message rather than getting silently downscaled.
    private static let maxImageBytes = 32 * 1024 * 1024

    private var client: Client?
    private var listener: Listener?
    private var pollTimer: Timer?
    /// Trigger for the iOS Local Network privacy prompt. Without this the
    /// raw-socket mDNS in the Rust core gets silently blocked. See the
    /// type's docstring for the full rationale.
    private let lanPrimer = LocalNetworkPrimer()

    /// Most recent ConnectionState we got from the WS. Used to revert
    /// after a transient error (e.g. one BlobNotFound on a stale meta) so
    /// the status pill doesn't stay red while the connection is healthy.
    private var lastConnectionStatus: BridgeStatus = .disconnected
    private var transientErrorToken: UInt64 = 0

    /// "Don't touch the pasteboard until this date" — set after every
    /// outbound send AND every inbound write. Apple's Universal Clipboard
    /// independently re-delivers the same content within 1-2 seconds; if
    /// we read during that window iOS shows a `想从 "Mac" 粘贴` prompt
    /// (annoying) and we'd risk publishing the UC-encoded version back to
    /// the relay (creates a duplicate row on the source device, possibly
    /// looping). Inside the window we still bump lastChangeCount so we
    /// don't re-evaluate the same UC drop every poll tick — we just refuse
    /// to *read* the bytes, so no prompt and no echo publish.
    private var quietPasteboardUntil: Date = .distantPast
    private static let quietWindow: TimeInterval = 5
    // `handleIncoming` updates `lastChangeCount` after writing remote clips
    // so the poll tick skips them. We deliberately don't compare strings —
    // doing so would also block the user from re-copying the same text.
    private var lastChangeCount: Int = UIPasteboard.general.changeCount

    /// Off-main worker for HTTP-bound operations (blob upload / download).
    /// Serial so a slow upload can't be lapped by the next poll's upload of
    /// the same clip — also keeps the relay from seeing reordered PUTs.
    private let blobQueue = DispatchQueue(label: "com.clipbridge.blob", qos: .userInitiated)
    private let fileQueue = DispatchQueue(label: "com.clipbridge.files", qos: .userInitiated)

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
        // Kick off the local-network permission prompt regardless of
        // pairing state — without this iOS never shows the dialog and
        // mDNS discovery silently never works. Cheap to leave running.
        lanPrimer.start()
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
        lanFilePeers = []
        fileTransferHistory = []
    }

    /// Manually pull recent clips from the relay's 5-min cache. Used by
    /// the UI's pull-to-refresh; the relay also pushes Recent automatically
    /// on reconnect, so most of the time this is redundant.
    func refreshRecent() {
        try? client?.fetchRecent()
        refreshFileTransferState()
    }

    /// Public entry point for the picker-driven send: hand us raw bytes
    /// (any image format) and we'll decode → re-encode to PNG → upload via
    /// blob, exactly the same path as a clipboard-driven send. Bypasses
    /// UIPasteboard entirely so picking a photo doesn't clobber what the
    /// user might have on their clipboard.
    func sendImageBytes(_ bytes: Data) {
        guard let image = UIImage(data: bytes) else {
            DispatchQueue.main.async {
                self.status = .error("图片解码失败")
            }
            return
        }
        // Re-encode to PNG so receivers on Win/Android don't need a HEIC
        // decoder. Skip when source is already PNG.
        let (pngBytes, mime): (Data, String) = {
            if bytes.starts(with: [0x89, 0x50, 0x4e, 0x47]) {
                return (bytes, "image/png")
            }
            return (image.pngData() ?? bytes, "image/png")
        }()
        let clip = ClipboardImage(
            bytes: pngBytes,
            mime: mime,
            width: UInt32(image.size.width.rounded()),
            height: UInt32(image.size.height.rounded()),
            uiImage: image
        )
        sendImage(clip)
    }

    func sendFiles(urls: [URL], to targetIds: Set<String>) {
        let peers = lanFilePeers.filter { targetIds.contains($0.deviceId) }
        guard !peers.isEmpty else {
            showTransientError("请选择 LAN 设备")
            return
        }
        guard !urls.isEmpty else { return }

        for url in urls {
            guard isRegularFileURL(url) else {
                showTransientError("只能发送普通文件: \(url.lastPathComponent)")
                continue
            }
            queueFileSends(url: url, peers: peers)
        }
    }

    private func queueFileSends(url: URL, peers: [LanPeerRecord]) {
        let size = fileByteSize(url)
        let mime = UTType(filenameExtension: url.pathExtension)?.preferredMIMEType

        for peer in peers {
            let id = UUID()
            upsertFileTransferEntry(FileTransferHistoryEntry(
                id: id,
                direction: .sent,
                fileName: url.lastPathComponent,
                fileURL: url,
                deviceName: peer.displayName,
                sizeBytes: size,
                status: .sending
            ))

            fileQueue.async { [weak self] in
                guard let self else { return }
                guard let client = self.client else {
                    DispatchQueue.main.async {
                        self.updateFileTransferEntry(id: id, status: .failed("未连接"))
                    }
                    return
                }
                do {
                    let sent = try client.sendFileToPeer(
                        targetDeviceId: peer.deviceId,
                        sourcePath: url.path,
                        mimeType: mime
                    )
                    DispatchQueue.main.async {
                        self.updateFileTransferEntry(
                            id: id,
                            sizeBytes: sent.bytesSent,
                            status: .completed
                        )
                    }
                } catch {
                    let message = self.friendlyErrorMessage(error)
                    DispatchQueue.main.async {
                        self.updateFileTransferEntry(id: id, status: .failed(message))
                    }
                    self.showTransientError("文件发送失败: \(message)")
                }
            }
        }
    }

    /// Re-paste a previously-seen clip onto the local pasteboard from the
    /// row tap. Routes through the coordinator (rather than the row poking
    /// `UIPasteboard` directly) so we can fall back to a blob re-fetch when
    /// the in-memory cache lost its bytes — and so the write goes through
    /// the same multi-rep path that receive uses.
    func pasteFromHistory(_ payload: ClipPayload) {
        switch payload.kind {
        case .text:
            seenHashes.insert(sha256Hex(payload.content))
            UIPasteboard.general.string = payload.content
            lastChangeCount = UIPasteboard.general.changeCount
        case .image:
            if let bytes = ImageThumbCache.shared.fullData(forTs: payload.ts) {
                writeImageBytesToPasteboard(bytes)
                return
            }
            // Cache miss (long-running app, jetsam, evicted by newer
            // clips). Try to refetch from the relay's blob cache — works
            // for ~5 minutes, the relay's TTL.
            guard let meta = payload.image, !meta.sha256Hex.isEmpty else {
                // Sent-side stubs ship with an empty sha (we never get the
                // real meta back from Rust). Refetching would 400; just
                // silently no-op like before.
                return
            }
            blobQueue.async { [weak self] in
                guard let self, let client = self.client else { return }
                guard let bytes = try? client.fetchImage(meta: meta) else {
                    self.showTransientError("图片已过期 (relay 默认 5 分钟保留, 请源设备重新复制)")
                    return
                }
                if let img = UIImage(data: bytes) {
                    ImageThumbCache.shared.store(image: img, bytes: bytes, forTs: payload.ts)
                }
                DispatchQueue.main.async {
                    self.writeImageBytesToPasteboard(bytes)
                }
            }
        }
    }

    /// Single source of truth for writing image bytes to UIPasteboard. Sets
    /// raw PNG bytes literally (no UIImage round-trip — see fetchAndPaste
    /// for the reasoning) under both `public.png` and `public.image` so
    /// receiving apps that strict-match on either UTI find what they want.
    /// Some IM apps (WeChat, etc.) only check the parent type.
    private func writeImageBytesToPasteboard(_ bytes: Data) {
        // Both hashes: pixel hash for cross-encoding dedup, byte hash as
        // a cheap belt-and-suspenders for the no-re-encode case.
        if let ph = imagePixelHashHex(bytes) { seenHashes.insert(ph) }
        seenHashes.insert(sha256Hex(bytes))
        UIPasteboard.general.setItems([[
            "public.png": bytes,
            "public.image": bytes,
        ]])
        lastChangeCount = UIPasteboard.general.changeCount
        enterQuietWindow()
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
                deviceName: UIDevice.current.name,
                listener: listener
            )
            configureFileReceiving()
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
        lanFilePeers = []
    }

    var fileReceiveDirectory: URL {
        iOSFileReceiveDirectory()
    }

    private func configureFileReceiving() {
        let folder = fileReceiveDirectory
        do {
            try FileManager.default.createDirectory(
                at: folder,
                withIntermediateDirectories: true
            )
            client?.setFileReceiveDir(dir: folder.path)
        } catch {
            status = .error("文件接收目录不可用: \(error.localizedDescription)")
        }
    }

    // MARK: - Pasteboard

    private func startPolling() {
        pollTimer?.invalidate()
        var lanTick = 0
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.checkPasteboard()
            // LAN peer count changes asynchronously inside the Rust runtime;
            // poll every other tick (≈2s) which is plenty for a status badge.
            lanTick += 1
            if lanTick % 2 == 0 {
                let names = (self.client?.lanPeers() ?? []).sorted()
                let n = UInt32(names.count)
                if n != self.lanPeerCount {
                    self.lanPeerCount = n
                }
                if names != self.lanPeerNames {
                    self.lanPeerNames = names
                }
                self.refreshFileTransferState()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    private func refreshFileTransferState() {
        guard let client else {
            if !lanFilePeers.isEmpty { lanFilePeers = [] }
            return
        }

        let peers = client.lanPeerRecords()
            .filter { $0.candidateCount > 0 }
            .sorted {
                if $0.displayName == $1.displayName {
                    return $0.deviceId < $1.deviceId
                }
                return $0.displayName.localizedStandardCompare($1.displayName) == .orderedAscending
            }
        if peers != lanFilePeers {
            lanFilePeers = peers
        }

        let completed = client.takeReceivedFiles()
        guard !completed.isEmpty else { return }
        for record in completed {
            upsertFileTransferEntry(FileTransferHistoryEntry(received: record))
        }
    }

    private func checkPasteboard() {
        let pb = UIPasteboard.general
        let cc = pb.changeCount
        guard cc != lastChangeCount else { return }

        // Apple Universal Clipboard echo guard: if we recently sent or
        // received via ClipBridge, UC is almost certainly delivering the
        // same content to this device's pasteboard around now. Reading
        // would (a) trigger iOS's "想从 Mac 粘贴" prompt and (b) risk us
        // publishing a UC-re-encoded copy back to the relay (the source
        // device sees a duplicate). Bump lastChangeCount so the next poll
        // doesn't keep retrying, then bail.
        if Date() < quietPasteboardUntil {
            lastChangeCount = cc
            return
        }

        // Image first — screenshots set both image and text reps, but the
        // user almost always wants the picture (text rep is usually the
        // file URL or empty).
        if let image = readClipboardImage() {
            lastChangeCount = cc
            // Pixel hash, not byte hash — see `imagePixelHashHex` for why.
            // Falls back to byte hash if decode somehow fails.
            let h = imagePixelHashHex(image.bytes) ?? sha256Hex(image.bytes)
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
            enterQuietWindow()
            appendSent(payload)
        } catch {
            DispatchQueue.main.async { self.status = .error("发送失败: \(error)") }
        }
    }

    /// Open the no-read window for `quietWindow` seconds. Called after any
    /// outbound publish or inbound write so the UC echo is silently
    /// swallowed instead of triggering the iOS paste prompt + republish.
    private func enterQuietWindow() {
        let candidate = Date().addingTimeInterval(Self.quietWindow)
        if candidate > quietPasteboardUntil {
            quietPasteboardUntil = candidate
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
        // Cache thumbnail + raw bytes immediately so the sent-card thumbnail
        // can show up before the upload even starts, and tap-to-paste later
        // re-uses the exact bytes (no re-encode).
        ImageThumbCache.shared.store(image: image.uiImage, bytes: image.bytes, forTs: ts)

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
                self.showTransientError("图片发送失败: \(self.friendlyErrorMessage(error))")
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

    private func upsertFileTransferEntry(_ entry: FileTransferHistoryEntry) {
        if let index = fileTransferHistory.firstIndex(where: { $0.id == entry.id }) {
            fileTransferHistory.remove(at: index)
        }
        fileTransferHistory.insert(entry, at: 0)
        if fileTransferHistory.count > Self.fileHistoryLimit {
            fileTransferHistory.removeLast(fileTransferHistory.count - Self.fileHistoryLimit)
        }
    }

    private func updateFileTransferEntry(
        id: UUID,
        sizeBytes: UInt64? = nil,
        status: FileTransferStatus
    ) {
        guard let index = fileTransferHistory.firstIndex(where: { $0.id == id }) else { return }
        var updated = fileTransferHistory
        if let sizeBytes {
            updated[index].sizeBytes = sizeBytes
        }
        updated[index].status = status
        fileTransferHistory = updated
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
        enterQuietWindow()
        appendRecent(payload)
    }

    private func fetchAndPasteImage(payload: ClipPayload, meta: ImageMeta) {
        guard let client = self.client else { return }
        do {
            let bytes = try client.fetchImage(meta: meta)
            let h = imagePixelHashHex(bytes) ?? sha256Hex(bytes)
            // If this exact pixel content is already on our pasteboard
            // (e.g. UC beat us to it, or we already received the same
            // image via another route), still record the hash so the next
            // poll doesn't re-publish, but skip the redundant write — we'd
            // just bump changeCount for nothing and clobber any in-flight
            // user action on the same content.
            if seenHashes.contains(h) {
                seenHashes.insert(h)   // refresh TTL
                // Don't appendRecent either — duplicates of duplicates
                // would clutter the history card.
                return
            }
            // Decode once on the worker thread so SwiftUI doesn't pay the
            // cost on first display.
            guard let image = UIImage(data: bytes) else {
                throw NSError(domain: "ClipBridge", code: -1,
                              userInfo: [NSLocalizedDescriptionKey: "图片字节解码失败"])
            }
            ImageThumbCache.shared.store(image: image, bytes: bytes, forTs: payload.ts)
            DispatchQueue.main.async {
                // writeImageBytesToPasteboard inserts the hash before
                // writing — keeps poll dedup tight and centralizes the
                // multi-rep / setItems policy.
                self.writeImageBytesToPasteboard(bytes)
                self.appendRecent(payload)
            }
        } catch {
            showTransientError("图片接收失败: \(friendlyErrorMessage(error))")
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
            let mapped: BridgeStatus = switch state {
            case .connecting: .connecting
            case .connected: .connected
            case .disconnected: .disconnected
            case .error(let message): .error(message)
            }
            self.lastConnectionStatus = mapped
            self.status = mapped
        }
    }

    /// Show a non-fatal error in the status pill, then auto-revert to the
    /// last known WS connection state after `seconds`. Use for hiccups
    /// that don't reflect a broken connection (stale blob refs, single
    /// failed sends, etc.) — leaving them sticky makes the UI look like
    /// it's perpetually broken.
    private func showTransientError(_ message: String, seconds: TimeInterval = 4) {
        DispatchQueue.main.async {
            self.transientErrorToken &+= 1
            let token = self.transientErrorToken
            self.status = .error(message)
            DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { [weak self] in
                guard let self else { return }
                // Only clear if no newer error or state change has happened.
                guard self.transientErrorToken == token else { return }
                self.status = self.lastConnectionStatus
            }
        }
    }

    /// Translate raw FFI error descriptions into something a user can act
    /// on, instead of "BlobNotFound". Falls back to the raw string when we
    /// don't recognize the case.
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

/// Pixel-content hash for image bytes — invariant to encoder differences.
/// Decodes the image, draws into a 32-bit RGBA bitmap, hashes that.
///
/// PNG/JPEG/HEIC all have multiple valid encodings for the same pixels.
/// Apple's Universal Clipboard appears to round-trip images through Core
/// Image on at least one hop, producing byte-different but pixel-identical
/// files. Without pixel hashing, our dedup misses the UC echo and we
/// publish the "same" image as if it were new — visible as duplicate
/// rows in the recent-clips card.
///
/// Cost: one decode + one CGContext draw + sha256 of W·H·4 bytes. Typical
/// ~15ms for an iPhone screenshot. Acceptable at 1Hz poll cadence.
func imagePixelHashHex(_ data: Data) -> String? {
    guard let img = UIImage(data: data),
          let cg = img.cgImage
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
