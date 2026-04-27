import CryptoKit
import Foundation
import UIKit

protocol KeyboardSyncDelegate: AnyObject {
    func keyboardSync(_ s: KeyboardSync, didUpdateStatus text: String)
    func keyboardSync(_ s: KeyboardSync, didReceiveClipPreview text: String)
}

/// Keyboard-extension equivalent of the main app's BridgeCoordinator.
///
/// Same wire format and same Rust `Client`, but with no audio session and no
/// background-task workarounds: the runloop in a UIInputViewController is
/// alive whenever the keyboard view is on screen, and that's exactly the
/// window where pasteboard sync is useful. When the user dismisses the
/// keyboard we tear everything down — no leaked sockets, no zombie timers.
final class KeyboardSync {
    weak var delegate: KeyboardSyncDelegate?

    /// Most recent text payload we received from the relay. The keyboard's
    /// "粘贴最新" button reads this directly so the user can inject the
    /// remote clip even if iOS hasn't propagated UIPasteboard.string yet.
    private(set) var latestRemoteClip: String?

    private var client: Client?
    private var listener: KbListener?
    private var pollTimer: Timer?
    private var lastChangeCount: Int = UIPasteboard.general.changeCount
    private var isRunning = false

    /// Recent SHA-256 hashes of text we've published or written. Same role
    /// as the main app's `seenHashes` — keeps us from re-publishing what
    /// the relay just delivered, and absorbs Universal Clipboard echoes
    /// when the user copies on Mac and the keyboard is also active here.
    /// Note: not shared with the main app's set (separate process); cross-
    /// process coord would need an App Group cache, deferred.
    private let seenHashes = KbRecentHashes()

    // MARK: - Lifecycle

    func start() {
        guard !isRunning else { return }
        guard let cfg = PairingStore.load() else {
            delegate?.keyboardSync(self, didUpdateStatus: "未配对 — 请先在主 App 扫码")
            return
        }
        guard let key = cfg.keyData else {
            delegate?.keyboardSync(self, didUpdateStatus: "密钥无效")
            return
        }
        let listener = KbListener(owner: self)
        self.listener = listener
        do {
            client = try Client(
                relayUrl: cfg.relayUrl,
                groupId: cfg.groupId,
                key: key,
                deviceId: PairingStore.deviceId(),
                listener: listener
            )
        } catch {
            delegate?.keyboardSync(self, didUpdateStatus: "启动失败: \(error)")
            return
        }
        isRunning = true
        delegate?.keyboardSync(self, didUpdateStatus: "连接中…")
        startPolling()
    }

    func stop() {
        isRunning = false
        pollTimer?.invalidate()
        pollTimer = nil
        client?.stop()
        client = nil
        listener = nil
    }

    /// Called by the VC when the user explicitly asks for a sync — eg.
    /// after they tap "复制选中". Lets us push immediately without waiting
    /// for the next 1Hz tick.
    func flushPasteboard() {
        checkPasteboard()
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
        guard pb.changeCount != lastChangeCount else { return }
        lastChangeCount = pb.changeCount

        guard let text = pb.string, !text.isEmpty else { return }

        let h = kbSha256Hex(text)
        if seenHashes.contains(h) { return }
        seenHashes.insert(h)

        let payload = ClipPayload(
            kind: .text,
            content: text,
            deviceName: UIDevice.current.name,
            ts: UInt64(Date().timeIntervalSince1970 * 1000),
            image: nil
        )
        try? client?.sendClip(payload: payload)
    }

    fileprivate func handleIncoming(_ payload: ClipPayload) {
        DispatchQueue.main.async {
            guard payload.kind == .text else { return }
            self.seenHashes.insert(kbSha256Hex(payload.content))
            UIPasteboard.general.string = payload.content
            // Same trick as the main app: bump our cursor past our own write
            // so the next poll tick doesn't echo it back to the relay.
            self.lastChangeCount = UIPasteboard.general.changeCount
            self.latestRemoteClip = payload.content
            let preview = payload.content.count > 80
                ? String(payload.content.prefix(80)) + "…"
                : payload.content
            self.delegate?.keyboardSync(self, didReceiveClipPreview: preview)
        }
    }

    fileprivate func handleState(_ state: ConnectionState) {
        DispatchQueue.main.async {
            let text: String
            switch state {
            case .connecting: text = "连接中…"
            case .connected: text = "已连接 · 同步中"
            case .disconnected: text = "已断开,正在重连"
            case .error(let m): text = "错误: \(m)"
            }
            self.delegate?.keyboardSync(self, didUpdateStatus: text)
        }
    }
}

private final class KbListener: ClipListener, @unchecked Sendable {
    weak var owner: KeyboardSync?
    init(owner: KeyboardSync) { self.owner = owner }
    func onClip(payload: ClipPayload) { owner?.handleIncoming(payload) }
    func onState(state: ConnectionState) { owner?.handleState(state) }
}

/// Keyboard-extension copy of the main app's `RecentHashes`. Same shape,
/// distinct type so it doesn't collide if both files end up linked into
/// the same target. Lives in this file so the keyboard target stays
/// minimal — no shared utility module to bring in.
final class KbRecentHashes {
    private let capacity: Int
    private let ttl: TimeInterval
    private var entries: [(hash: String, addedAt: Date)] = []
    private let queue = DispatchQueue(label: "com.clipbridge.kb.recent-hashes")

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

func kbSha256Hex(_ s: String) -> String {
    let digest = SHA256.hash(data: Data(s.utf8))
    return digest.map { String(format: "%02x", $0) }.joined()
}
