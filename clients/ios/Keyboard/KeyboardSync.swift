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
