import AVFoundation
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

/// Singleton owning the Rust `Client`, the pasteboard polling, and the
/// background-audio session that keeps us awake on TrollStore-signed
/// installs (private entitlements would obviate the audio trick, but the
/// silent-loop keeps the app alive even on stricter sandboxes).
final class BridgeCoordinator: ObservableObject {
    static let shared = BridgeCoordinator()

    @Published private(set) var status: BridgeStatus = .notPaired
    @Published private(set) var hasPairing: Bool = false

    private var client: Client?
    private var listener: Listener?
    private var pollTimer: Timer?
    private var lastChangeCount: Int = UIPasteboard.general.changeCount
    private var lastSentText: String?
    private var lastReceivedText: String?

    private var audioPlayer: AVAudioPlayer?

    private init() {}

    func bootstrap() {
        configureBackgroundAudio()
        if let cfg = PairingStore.load() {
            startCoordinator(with: cfg)
            hasPairing = true
        } else {
            status = .notPaired
            hasPairing = false
        }
    }

    func applicationDidBecomeActive() {
        // Coming back to foreground is the perfect moment to drain pasteboard
        // changes that may have happened while we were truly suspended. The
        // poll timer also runs on its own, but a manual tick here means an
        // immediate response.
        checkPasteboard()
    }

    // MARK: - Pairing lifecycle

    func savePairing(_ cfg: PairingConfig) {
        PairingStore.save(cfg)
        hasPairing = true
        startCoordinator(with: cfg)
    }

    func resetPairing() {
        stopCoordinator()
        PairingStore.clear()
        hasPairing = false
        status = .notPaired
    }

    private func startCoordinator(with cfg: PairingConfig) {
        stopCoordinator()
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

    private func stopCoordinator() {
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
        if text == lastReceivedText { return }
        if text == lastSentText { return }
        lastSentText = text

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
            self.lastReceivedText = payload.content
            UIPasteboard.general.string = payload.content
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

    // MARK: - Background audio (keeps us alive when iOS would otherwise suspend)

    private func configureBackgroundAudio() {
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(
                .playback,
                mode: .default,
                options: [.mixWithOthers, .allowBluetooth]
            )
            try session.setActive(true)
        } catch {
            // Not fatal — TrollStore entitlements alone may keep us alive.
            return
        }
        playSilentLoop()
    }

    private func playSilentLoop() {
        // Generate ~0.5s of silent PCM and play on infinite loop. The audio
        // session being active under .playback category is what actually
        // earns us the background runtime; the audio data being silent is
        // both ethical and unobtrusive.
        let sampleRate = 22_050.0
        let durationSeconds = 0.5
        let frameCount = AVAudioFrameCount(sampleRate * durationSeconds)
        guard
            let format = AVAudioFormat(
                standardFormatWithSampleRate: sampleRate,
                channels: 1
            ),
            let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frameCount)
        else { return }
        buffer.frameLength = frameCount
        if let data = buffer.floatChannelData?[0] {
            for i in 0..<Int(frameCount) { data[i] = 0 }
        }

        // AVAudioPlayer wants a file URL — write a tiny WAV to a temp file.
        let tmpURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("clipbridge-silence.wav")
        if !FileManager.default.fileExists(atPath: tmpURL.path) {
            do {
                let file = try AVAudioFile(forWriting: tmpURL, settings: format.settings)
                try file.write(from: buffer)
            } catch {
                return
            }
        }
        do {
            audioPlayer = try AVAudioPlayer(contentsOf: tmpURL)
            audioPlayer?.numberOfLoops = -1
            audioPlayer?.volume = 0
            audioPlayer?.play()
        } catch {
            return
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
