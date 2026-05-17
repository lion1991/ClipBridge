package com.clipbridge

import java.io.File
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ClipBridgeAccessibilityServicePolicyTest {
    private val serviceSource: String
        get() = File("src/main/java/com/clipbridge/ClipBridgeAccessibilityService.kt")
            .readText()
    private val mainActivitySource: String
        get() = File("src/main/java/com/clipbridge/MainActivity.kt")
            .readText()

    @Test
    fun serviceConnectionDoesNotStartShizukuPolling() {
        val body = serviceSource.functionBody("override fun onServiceConnected()")

        assertFalse(
            "Shizuku clipboard reads must be remote-triggered, not started when the accessibility service connects.",
            body.contains("startShizukuPoller()"),
        )
    }

    @Test
    fun shizukuClipboardReadIsNotAnAlwaysOnLoop() {
        assertFalse(
            "Shizuku clipboard reads must not run as an always-on timer loop.",
            serviceSource.contains("while (isActive)") &&
                serviceSource.contains("delay(POLL_INTERVAL_MS)"),
        )
        assertTrue(
            "Remote clips should trigger an on-demand Shizuku clipboard read.",
            serviceSource.contains("triggerShizukuClipboardRead("),
        )
    }

    @Test
    fun screenStateControlsClientReconnectIdleMode() {
        assertTrue(
            "The accessibility service should observe screen on/off state.",
            serviceSource.contains("Intent.ACTION_SCREEN_OFF") &&
                serviceSource.contains("Intent.ACTION_SCREEN_ON"),
        )
        assertTrue(
            "Screen state should be forwarded to the Rust client reconnect policy.",
            serviceSource.contains("setReconnectIdleMode("),
        )
    }

    @Test
    fun screenOffSuspendsLanTransport() {
        assertTrue(
            "Screen-off standby should suspend Android LAN activity instead of leaving discovery/reconnect loops hot.",
            serviceSource.contains("setLanActive(false)") &&
                serviceSource.contains("releaseMulticastLock()"),
        )
    }

    @Test
    fun transferEventsOpenTemporaryLanWindow() {
        assertTrue(
            "Remote and local transfer activity should open a bounded LAN window instead of keeping LAN always-on.",
            serviceSource.contains("activateLanTemporarily(") &&
                serviceSource.contains("LAN_ACTIVE_WINDOW_MS"),
        )
    }

    @Test
    fun hostAppForegroundRequestsActiveReconnectWithoutChangingScreenOffStandby() {
        val resumeBody = mainActivitySource.functionBody("if (event == Lifecycle.Event.ON_RESUME)")
        val foregroundBody = serviceSource.functionBody("fun onHostAppForeground()")
        val screenOffBody = serviceSource.functionBody("private fun setReconnectIdleMode(enabled: Boolean)")

        assertTrue(
            "MainActivity resume should notify the accessibility service that the host app is foreground.",
            resumeBody.contains("onHostAppForeground()"),
        )
        assertTrue(
            "Foregrounding the host app should leave idle reconnect mode, activate LAN, refresh LAN now, and refresh recent clips.",
            foregroundBody.contains("setReconnectIdleMode(false)") &&
                foregroundBody.contains("setLanActive(true)") &&
                foregroundBody.contains("refreshLanNow()") &&
                foregroundBody.contains("fetchRecent()"),
        )
        assertTrue(
            "Screen-off standby must still suspend LAN activity.",
            screenOffBody.contains("setLanActive(false)"),
        )
    }
}

private fun String.functionBody(signature: String): String {
    val start = indexOf(signature)
    require(start >= 0) { "missing function: $signature" }
    val brace = indexOf('{', start)
    require(brace >= 0) { "missing function body: $signature" }
    var depth = 0
    for (i in brace until length) {
        when (this[i]) {
            '{' -> depth += 1
            '}' -> {
                depth -= 1
                if (depth == 0) return substring(brace + 1, i)
            }
        }
    }
    error("unterminated function body: $signature")
}
