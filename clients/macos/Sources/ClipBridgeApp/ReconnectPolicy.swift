import Foundation

/// What the connection watchdog should do on one tick.
enum ReconnectAction: Equatable {
    /// The link is up — leave the core alone.
    case none

    /// There is no core client at all. The constructor threw (or the key
    /// failed to decode) and nothing ever retried it, so the menu sat on
    /// whatever status was showing with nothing behind it.
    case startClient

    /// A client exists and is still reporting state, it just isn't connected:
    /// cut its backoff short instead of waiting out up to 30s of it.
    case reconnectNow

    /// A client exists but the core has gone quiet while disconnected — its
    /// worker thread is dead or wedged. Nothing short of a new client helps.
    case restartClient
}

/// Tuning for the watchdog. Mirrors the Android foreground watchdog
/// (5s→30s) so both hosts recover on the same cadence.
enum ReconnectTuning {
    static let minDelay: TimeInterval = 5
    static let maxDelay: TimeInterval = 30

    /// How long the core may report nothing at all, while not connected,
    /// before we treat its worker as gone. A live worker emits a state on
    /// every dial attempt, and its slowest possible cycle is a 15s connect
    /// cap plus a 30s backoff — so silence past this is not a slow network,
    /// it is nobody home.
    static let coreSilenceLimit: TimeInterval = 120
}

/// Decide one watchdog tick. Pure so the escalation order (retry → replace)
/// is testable without a menu bar, a network, or a core.
///
/// `workerAlive` is the core's own answer (`Client.isRunning()`); a worker
/// that unwound reports its crash and then never speaks again, so redialing
/// it is pointless — only a new client recovers. Silence is the same verdict
/// arrived at the slow way, for a worker that is stuck rather than gone.
func reconnectAction(
    hasClient: Bool,
    workerAlive: Bool,
    isConnected: Bool,
    sinceLastState: TimeInterval,
    silenceLimit: TimeInterval = ReconnectTuning.coreSilenceLimit
) -> ReconnectAction {
    if isConnected { return .none }
    guard hasClient else { return .startClient }
    if !workerAlive { return .restartClient }
    return sinceLastState >= silenceLimit ? .restartClient : .reconnectNow
}

/// Gap before the next tick. Ticks that actually retried double the gap up to
/// `max` so a genuinely dead network doesn't turn into a dial loop; a tick
/// with nothing to do drops back to `min` so the next real outage is caught
/// quickly.
func nextWatchdogDelay(
    current: TimeInterval,
    retried: Bool,
    min minDelay: TimeInterval = ReconnectTuning.minDelay,
    max maxDelay: TimeInterval = ReconnectTuning.maxDelay
) -> TimeInterval {
    retried ? Swift.min(current * 2, maxDelay) : minDelay
}
