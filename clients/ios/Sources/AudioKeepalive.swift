import AVFoundation
import Foundation

/// Holds an active `.playback` AVAudioSession + an inaudible PCM loop so
/// the app process keeps runtime in the background. Combined with the
/// `audio` UIBackgroundModes plist key this lets the WebSocket stay alive
/// past iOS's normal ~5–30s background-suspension window.
///
/// What's UNKNOWN at the time of writing this comment: whether iOS 17+'s
/// pasted gate (`Not allowing bundle ID … access to the pasteboard while
/// it's not foreground`) treats an audio-kept-alive backgrounded app as
/// "foreground enough". The spike daemon (no UI scene at all) was denied.
/// A backgrounded app has an INACTIVE scene — possibly tolerated, possibly
/// not. If pasted still denies, we at least buffer incoming clips in
/// `recentClips` (visible on next foreground); if pasted allows, we have
/// full background sync.
final class AudioKeepalive {
    private var player: AVAudioPlayer?
    private var observersRegistered = false

    /// Idempotent. Safe to call repeatedly — re-arms whichever subset of
    /// {observers, session, player} got torn down by audio interruptions
    /// (incoming call, Siri, mediaserverd respawn, …).
    func start() {
        registerObservers()
        activateSession()
        ensurePlaying()
    }

    private func registerObservers() {
        guard !observersRegistered else { return }
        let nc = NotificationCenter.default
        nc.addObserver(
            self, selector: #selector(handleInterruption(_:)),
            name: AVAudioSession.interruptionNotification, object: nil
        )
        nc.addObserver(
            self, selector: #selector(handleMediaServicesReset(_:)),
            name: AVAudioSession.mediaServicesWereResetNotification, object: nil
        )
        observersRegistered = true
    }

    private func activateSession() {
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
            try session.setActive(true, options: [])
        } catch {
            // Best-effort; next start() / interruption-end will retry.
        }
    }

    private func ensurePlaying() {
        if player == nil {
            player = makeSilentPlayer()
        }
        if player?.isPlaying == false {
            player?.play()
        }
    }

    private func makeSilentPlayer() -> AVAudioPlayer? {
        // ~0.5s of effectively-inaudible PCM, looped forever.
        // Two non-obvious choices baked in here:
        //   1. Sample data is non-zero (±1e-4). iOS occasionally treats
        //      all-zero buffers as "not really playing" and reclaims our
        //      runtime; a tiny dither defeats that heuristic without being
        //      audible (~ -80 dBFS is below any phone speaker's noise floor).
        //   2. Player volume is 1.0. volume == 0 is the OTHER heuristic
        //      iOS uses to suspend "silent" apps; sample amplitude is
        //      already low enough that volume can stay at 1.0.
        let sampleRate = 22_050.0
        let durationSeconds = 0.5
        let frameCount = AVAudioFrameCount(sampleRate * durationSeconds)
        guard
            let format = AVAudioFormat(standardFormatWithSampleRate: sampleRate, channels: 1),
            let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frameCount)
        else { return nil }
        buffer.frameLength = frameCount
        if let data = buffer.floatChannelData?[0] {
            let amp: Float = 1e-4
            for i in 0..<Int(frameCount) {
                data[i] = (i & 1) == 0 ? amp : -amp
            }
        }
        let tmpURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("clipbridge-silence.wav")
        // Always rewrite — a half-written file from a killed launch would
        // make AVAudioPlayer init throw forever and silently strand us.
        try? FileManager.default.removeItem(at: tmpURL)
        do {
            let file = try AVAudioFile(forWriting: tmpURL, settings: format.settings)
            try file.write(from: buffer)
            let player = try AVAudioPlayer(contentsOf: tmpURL)
            player.numberOfLoops = -1
            player.volume = 1.0
            player.prepareToPlay()
            return player
        } catch {
            return nil
        }
    }

    @objc private func handleInterruption(_ note: Notification) {
        guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
              let type = AVAudioSession.InterruptionType(rawValue: raw)
        else { return }
        if type == .ended {
            // Phone call / Siri ended — re-arm everything.
            activateSession()
            ensurePlaying()
        }
    }

    @objc private func handleMediaServicesReset(_ note: Notification) {
        // mediaserverd respawned — our player handle is now stale.
        player?.stop()
        player = nil
        activateSession()
        ensurePlaying()
    }
}
