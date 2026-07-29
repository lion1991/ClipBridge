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
    fun screenStateControlsStandbyStateMachine() {
        assertTrue(
            "The accessibility service should observe screen on/off state.",
            serviceSource.contains("Intent.ACTION_SCREEN_OFF") &&
                serviceSource.contains("Intent.ACTION_SCREEN_ON"),
        )
        assertTrue(
            "Screen-off must enter standby; screen-on / present must leave standby.",
            serviceSource.contains("enterStandby()") &&
                serviceSource.contains("leaveStandby("),
        )
        assertTrue(
            "SCREEN_OFF should call enterStandby.",
            serviceSource.contains("Intent.ACTION_SCREEN_OFF -> enterStandby()"),
        )
    }

    @Test
    fun enterStandbyHardSuspendsRelayAndLan() {
        val body = serviceSource.functionBody("private fun enterStandby()")

        assertTrue(
            "Standby must hard-suspend the relay WebSocket via reconnect idle mode.",
            body.contains("setReconnectIdleMode(true)"),
        )
        assertTrue(
            "Standby must tear down LAN (core setLanActive(false) is real suspend after PR1).",
            body.contains("setLanActive(false)") &&
                body.contains("releaseMulticastLock()"),
        )
    }

    @Test
    fun leaveStandbyRestoresRelayAndAppliesLanNetworkPolicy() {
        val body = serviceSource.functionBody("private fun leaveStandby(reason: String, immediate: Boolean = false)")

        assertTrue(
            "Leaving standby should resume the relay, apply the current LAN network policy, and fetch recent.",
            body.contains("setReconnectIdleMode(false)") &&
                body.contains("applyLanTransportPolicy(") &&
                body.contains("fetchRecent()"),
        )
        assertTrue(
            "Leave-standby should be debounced to avoid lock/unlock reconnect storms.",
            serviceSource.contains("LEAVE_STANDBY_DEBOUNCE_MS"),
        )
        assertTrue(
            "Delayed standby transitions must run on the main dispatcher so they " +
                "serialize with enterStandby/leaveStandby from broadcast callbacks.",
            serviceSource.contains("scope.launch(Dispatchers.Main)"),
        )
    }

    @Test
    fun temporaryWakeWindowIsFullLeaveStandbyWithReentry() {
        val body = serviceSource.functionBody("private fun activateLanTemporarily(reason: String)")

        assertTrue(
            "Wake window must be a full leaveStandby (LAN + relay) so publish can leave the device.",
            body.contains("leaveStandby(") &&
                body.contains("immediate = true"),
        )
        assertTrue(
            "After the window, still-non-interactive devices re-enter standby.",
            body.contains("LAN_ACTIVE_WINDOW_MS") &&
                body.contains("enterStandby()"),
        )
        assertTrue(
            "Remote clip handler must not open a wake window for every receive.",
            serviceSource.contains("Deliberately no activateLanTemporarily"),
        )
    }

    @Test
    fun hostAppForegroundLeavesStandbyImmediately() {
        val resumeBody = mainActivitySource.functionBody("if (event == Lifecycle.Event.ON_RESUME)")
        val foregroundBody = serviceSource.functionBody("fun onHostAppForeground()")

        assertTrue(
            "MainActivity resume should notify the accessibility service that the host app is foreground.",
            resumeBody.contains("onHostAppForeground()"),
        )
        assertTrue(
            "Foregrounding the host app should leave standby immediately with full refresh.",
            foregroundBody.contains("leaveStandby(") &&
                foregroundBody.contains("immediate = true"),
        )
    }

    @Test
    fun mainActivityAutoEnablesAccessibilityWhenShizukuBecomesReady() {
        assertTrue(
            "Cold start should retry automatic accessibility enablement when Shizuku becomes READY, including binder-delayed starts.",
            mainActivitySource.contains("autoEnableAttemptToken") &&
                mainActivitySource.contains("ShizukuBridge.enableAccessibilityService("),
        )
    }

    @Test
    fun copyToastPrefersShizukuClipboardReadOverCachedAccessibilitySelection() {
        val body = serviceSource.functionBody("private fun maybeHandleCopyToast")

        assertTrue(
            "When Shizuku is ready, a copy toast should read the real system clipboard instead of publishing a cached UI selection.",
            body.contains("ShizukuBridge.State.READY") &&
                body.contains("triggerShizukuClipboardRead(\"copy toast\")"),
        )
        assertTrue(
            "The Shizuku read must happen before the accessibility selection fallback can publish.",
            body.indexOf("triggerShizukuClipboardRead(\"copy toast\")") <
                body.indexOf("publish(sel)"),
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
