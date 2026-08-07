package com.clipbridge

/** What the foreground reconnect watchdog should do on one tick. */
internal enum class ForegroundReconnectAction {
    /** Leave the transport alone: connected, backgrounded, or standby owns it. */
    NONE,

    /** There is no client at all — build one. */
    START_CLIENT,

    /** A client exists but isn't connected — ask it to redial immediately. */
    RECONNECT_NOW,
}

/**
 * Android hands the app no dependable background lifetime, so a session that
 * died while the user was away is routine. Whenever the app is in front and
 * the link is not up, we act rather than report.
 *
 * Two cases deliberately do nothing:
 *
 *   - Screen-off standby (`reconnectIdleMode`) owns the transport; reconnecting
 *     underneath it would undo the hard suspend. Only reachable as a brief race
 *     between a lock and the watchdog tick.
 *   - Backgrounded, which the watchdog also checks before sleeping — this is
 *     the belt to that suspenders, since the flag can flip mid-tick.
 */
internal fun foregroundReconnectAction(
    hostAppForeground: Boolean,
    reconnectIdleMode: Boolean,
    hasClient: Boolean,
    isConnected: Boolean,
): ForegroundReconnectAction = when {
    !hostAppForeground -> ForegroundReconnectAction.NONE
    isConnected -> ForegroundReconnectAction.NONE
    reconnectIdleMode -> ForegroundReconnectAction.NONE
    !hasClient -> ForegroundReconnectAction.START_CLIENT
    else -> ForegroundReconnectAction.RECONNECT_NOW
}

/**
 * Gap before the watchdog's next tick. Ticks that actually retried double the
 * gap up to [maxMs] so a genuinely dead network doesn't turn a long foreground
 * session into a dial loop; anything else (connected, or standby holding the
 * transport) drops back to [minMs] so the next real outage is caught quickly.
 */
internal fun nextForegroundReconnectDelayMs(
    currentMs: Long,
    retried: Boolean,
    minMs: Long,
    maxMs: Long,
): Long = if (retried) (currentMs * 2).coerceAtMost(maxMs) else minMs
