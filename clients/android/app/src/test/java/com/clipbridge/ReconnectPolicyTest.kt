package com.clipbridge

import org.junit.Assert.assertEquals
import org.junit.Test

class ReconnectPolicyTest {

    @Test
    fun foregroundAndDisconnectedRetriesInsteadOfReporting() {
        assertEquals(
            ForegroundReconnectAction.RECONNECT_NOW,
            foregroundReconnectAction(
                hostAppForeground = true,
                reconnectIdleMode = false,
                hasClient = true,
                isConnected = false,
            ),
        )
    }

    @Test
    fun foregroundWithoutClientBuildsOne() {
        assertEquals(
            ForegroundReconnectAction.START_CLIENT,
            foregroundReconnectAction(
                hostAppForeground = true,
                reconnectIdleMode = false,
                hasClient = false,
                isConnected = false,
            ),
        )
    }

    @Test
    fun connectedNeedsNoAction() {
        assertEquals(
            ForegroundReconnectAction.NONE,
            foregroundReconnectAction(
                hostAppForeground = true,
                reconnectIdleMode = false,
                hasClient = true,
                isConnected = true,
            ),
        )
    }

    @Test
    fun backgroundedNeverReconnects() {
        assertEquals(
            ForegroundReconnectAction.NONE,
            foregroundReconnectAction(
                hostAppForeground = false,
                reconnectIdleMode = false,
                hasClient = true,
                isConnected = false,
            ),
        )
    }

    @Test
    fun standbyOwnsTheTransport() {
        // Screen-off hard suspend must not be undone by a stale watchdog tick.
        assertEquals(
            ForegroundReconnectAction.NONE,
            foregroundReconnectAction(
                hostAppForeground = true,
                reconnectIdleMode = true,
                hasClient = true,
                isConnected = false,
            ),
        )
        assertEquals(
            ForegroundReconnectAction.NONE,
            foregroundReconnectAction(
                hostAppForeground = true,
                reconnectIdleMode = true,
                hasClient = false,
                isConnected = false,
            ),
        )
    }

    @Test
    fun retriesBackOffAndClampAtTheCeiling() {
        assertEquals(
            10_000L,
            nextForegroundReconnectDelayMs(
                currentMs = 5_000L,
                retried = true,
                minMs = 5_000L,
                maxMs = 30_000L,
            ),
        )
        assertEquals(
            30_000L,
            nextForegroundReconnectDelayMs(
                currentMs = 20_000L,
                retried = true,
                minMs = 5_000L,
                maxMs = 30_000L,
            ),
        )
        assertEquals(
            30_000L,
            nextForegroundReconnectDelayMs(
                currentMs = 30_000L,
                retried = true,
                minMs = 5_000L,
                maxMs = 30_000L,
            ),
        )
    }

    @Test
    fun aQuietTickResetsToTheShortInterval() {
        assertEquals(
            5_000L,
            nextForegroundReconnectDelayMs(
                currentMs = 30_000L,
                retried = false,
                minMs = 5_000L,
                maxMs = 30_000L,
            ),
        )
    }
}
