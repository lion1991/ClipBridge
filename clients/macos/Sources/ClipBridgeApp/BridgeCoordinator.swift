import AppKit
import ClipbridgeCore

/// Owns the Rust `Client`, the pasteboard polling timer, and the bridge between
/// AppKit clipboard events and the Rust core.
///
/// Feedback-loop avoidance: we remember the last clip we *received* from the
/// network and skip publishing it back when the local pasteboard polls match it.
enum BridgeStatus {
    case notPaired
    case connecting
    case connected
    case disconnected
    case error(String)
}

final class BridgeCoordinator {
    private let config: PairingConfig
    private let onStateChange: (BridgeStatus) -> Void
    private var client: Client?
    private var listener: Listener?
    private var pollTimer: Timer?

    private var lastChangeCount: Int = NSPasteboard.general.changeCount
    private var lastSentText: String?
    private var lastReceivedText: String?

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

        guard let text = pb.string(forType: .string), !text.isEmpty else { return }
        // Skip if this is exactly what we just wrote from a remote clip.
        if text == lastReceivedText { return }
        // Skip if it's the same text we already published.
        if text == lastSentText { return }
        lastSentText = text

        let payload = ClipPayload(
            kind: .text,
            content: text,
            deviceName: Self.deviceName,
            ts: UInt64(Date().timeIntervalSince1970 * 1000)
        )
        do {
            try client?.sendClip(payload: payload)
        } catch {
            onStateChange(.error("发送失败:\(error)"))
        }
    }

    fileprivate func handleIncoming(payload: ClipPayload) {
        DispatchQueue.main.async {
            guard payload.kind == .text else { return }
            let pb = NSPasteboard.general
            self.lastReceivedText = payload.content
            pb.clearContents()
            pb.setString(payload.content, forType: .string)
            self.lastChangeCount = pb.changeCount
        }
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
