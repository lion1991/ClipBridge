import Combine
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

/// Foreground-only sync coordinator for the main ClipBridge app.
///
/// We deliberately don't fight iOS's background-suspension model here. While
/// the main app is foregrounded, this connects to the relay and polls the
/// pasteboard the same way every other client does; on backgrounding it
/// tears the client down. Long-running cross-app sync is the keyboard
/// extension's job — it has runtime whenever the user is typing.
final class BridgeCoordinator: ObservableObject {
    static let shared = BridgeCoordinator()

    @Published private(set) var status: BridgeStatus = .notPaired
    @Published private(set) var hasPairing: Bool = false

    private var client: Client?
    private var listener: Listener?
    private var pollTimer: Timer?
    // `handleIncoming` updates `lastChangeCount` after writing remote clips
    // so the poll tick skips them. We deliberately don't compare strings —
    // doing so would also block the user from re-copying the same text.
    private var lastChangeCount: Int = UIPasteboard.general.changeCount

    private init() {}

    func bootstrap() {
        hasPairing = PairingStore.load() != nil
        status = hasPairing ? .disconnected : .notPaired
    }

    func applicationDidBecomeActive() {
        guard let cfg = PairingStore.load() else {
            stopSync()
            hasPairing = false
            status = .notPaired
            return
        }
        hasPairing = true
        startSync(with: cfg)
    }

    func applicationDidEnterBackground() {
        // Keyboard extension takes over from here whenever the user types in
        // any app. Nothing to keep alive in the main app.
        stopSync()
        if hasPairing {
            status = .disconnected
        }
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
        guard pb.changeCount != lastChangeCount else { return }
        lastChangeCount = pb.changeCount

        guard let text = pb.string, !text.isEmpty else { return }

        let payload = ClipPayload(
            kind: .text,
            content: text,
            deviceName: UIDevice.current.name,
            ts: UInt64(Date().timeIntervalSince1970 * 1000)
        )
        do {
            try client?.sendClip(payload: payload)
        } catch {
            DispatchQueue.main.async { self.status = .error("发送失败: \(error)") }
        }
    }

    fileprivate func handleIncoming(payload: ClipPayload) {
        DispatchQueue.main.async {
            guard payload.kind == .text else { return }
            UIPasteboard.general.string = payload.content
            // Capture the post-write changeCount so the next poll tick treats
            // our own write as a no-op instead of re-publishing it.
            self.lastChangeCount = UIPasteboard.general.changeCount
        }
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
