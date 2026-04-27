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
            appendSent(payload)
        } catch {
            DispatchQueue.main.async { self.status = .error("发送失败: \(error)") }
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
        DispatchQueue.main.async {
            guard payload.kind == .text else { return }
            UIPasteboard.general.string = payload.content
            // Capture the post-write changeCount so the next poll tick treats
            // our own write as a no-op instead of re-publishing it.
            self.lastChangeCount = UIPasteboard.general.changeCount
            self.appendRecent(payload)
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
