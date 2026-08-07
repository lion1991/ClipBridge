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
    private val shizukuSource: String
        get() = File("src/main/java/com/clipbridge/ShizukuBridge.kt")
            .readText()
    private val clipboardUserServiceSource: String
        get() = File("src/main/java/com/clipbridge/ClipboardUserService.kt")
            .takeIf(File::isFile)
            ?.readText()
            .orEmpty()
    private val clipboardUserServiceAidl: String
        get() = File("src/main/aidl/com/clipbridge/IClipboardUserService.aidl")
            .takeIf(File::isFile)
            ?.readText()
            .orEmpty()
    private val clipboardTextListenerAidl: String
        get() = File("src/main/aidl/com/clipbridge/IClipboardTextListener.aidl")
            .takeIf(File::isFile)
            ?.readText()
            .orEmpty()

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
        val resumeBody = mainActivitySource.functionBody("Lifecycle.Event.ON_RESUME ->")
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
    fun hostAppForegroundActivelyReconnectsInsteadOfReportingAnError() {
        val foregroundBody = serviceSource.functionBody("fun onHostAppForeground()")
        val ensureBody = serviceSource.functionBody("private fun ensureConnected(reason: String)")

        assertTrue(
            "Android grants no dependable background lifetime, so foregrounding must " +
                "actively re-establish the link and keep watching, not just leave standby.",
            foregroundBody.contains("ensureConnected(") &&
                foregroundBody.contains("startForegroundReconnectWatchdog()"),
        )
        assertTrue(
            "A client that never started (unpaired at connect, or a throwing constructor) " +
                "is only ever retried here — nothing else rebuilds it.",
            ensureBody.contains("startClient()"),
        )
        assertTrue(
            "An existing client must be told to redial now rather than waiting out its backoff.",
            ensureBody.contains("reconnectNow()"),
        )
    }

    @Test
    fun foregroundReconnectWatchdogStopsWhenTheAppLeaves() {
        val watchdog =
            serviceSource.functionBody("private fun startForegroundReconnectWatchdog()")
        val backgroundBody = serviceSource.functionBody("fun onHostAppBackground()")
        val pauseBody = mainActivitySource.functionBody("Lifecycle.Event.ON_PAUSE ->")

        assertTrue(
            "The watchdog must be gated on the foreground flag so it never polls in the background.",
            watchdog.contains("hostAppForeground"),
        )
        assertTrue(
            "Retries must back off so a dead network can't turn a long foreground session into a dial loop.",
            watchdog.contains("nextForegroundReconnectDelayMs("),
        )
        assertTrue(
            "ON_PAUSE must cancel the watchdog.",
            pauseBody.contains("onHostAppBackground()") &&
                backgroundBody.contains("foregroundReconnectJob?.cancel()"),
        )
    }

    @Test
    fun networkSwitchNudgesTheRelayButNotDuringStandby() {
        val body = serviceSource.functionBody("private fun requestRelayReconnect(reason: String)")

        assertTrue(
            "A default-network switch leaves the old socket open but dead; nudge the core " +
                "instead of waiting out its idle timeout.",
            serviceSource.contains("requestRelayReconnect(\"default network available\")") &&
                body.contains("reconnectNow()"),
        )
        assertTrue(
            "Waking the radio for a screen-off network blip is exactly the background " +
                "traffic standby exists to remove.",
            body.contains("if (reconnectIdleMode) return"),
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
                body.contains("triggerShizukuClipboardRead(") &&
                body.contains("reason = \"copy toast\""),
        )
        assertTrue(
            "The Shizuku read must happen before the accessibility selection fallback can publish.",
            body.indexOf("triggerShizukuClipboardRead(") <
                body.indexOf("publish(sel)"),
        )
    }

    @Test
    fun clipboardListenerFallsBackToShizukuWhenBackgroundReadIsDenied() {
        val body = serviceSource.functionBody("private fun handleLocalClipboardChange()")
        val shizukuBody = serviceSource.functionBody("private fun triggerShizukuClipboardRead")

        assertTrue(
            "A background clipboard notification must fall back to Shizuku when ClipboardManager cannot return the clip.",
            body.contains("triggerShizukuClipboardRead(") &&
                body.contains("\"clipboard listener\""),
        )
        assertTrue(
            "An active Samsung event monitor must suppress the duplicate standard read that can clear permission-bearing SemClipData.",
            body.contains("ShizukuBridge.isClipboardMonitorActive()") &&
                body.indexOf("ShizukuBridge.isClipboardMonitorActive()") <
                body.indexOf("triggerShizukuClipboardRead("),
        )
        assertTrue(
            "If privileged clipboard access also fails, the listener must retain a recent Accessibility selection fallback.",
            body.contains("fallbackText = recentSelectionForClipboardFallback()") &&
                shizukuBody.contains("publishClipboardFallback("),
        )
        assertFalse(
            "A Shizuku read must not suppress the same text forever; publish() already provides time-bounded echo and source deduplication.",
            shizukuBody.contains("text != lastShizukuText"),
        )
    }

    @Test
    fun rootShizukuClipboardReadUsesShellIdentityUserService() {
        val body = shizukuSource.functionBody("suspend fun readPrimaryClip(): Clip?")

        assertTrue(
            "Root-mode Shizuku must use a short-lived service that drops to shell UID instead of giving up on the real clipboard.",
            body.contains("Shizuku.getUid()") &&
                body.contains("SHIZUKU_SHELL_UID") &&
                body.contains("readPrimaryClipThroughShellUserService()"),
        )
    }

    @Test
    fun rootClipboardUserServiceDropsIdentityAndCanBeDestroyed() {
        assertTrue(
            "The root UserService must drop both gid and uid to shell before reading ClipboardService.",
            clipboardUserServiceSource.contains("Os.setgid(SHELL_UID)") &&
                clipboardUserServiceSource.contains("Os.setuid(SHELL_UID)") &&
                clipboardUserServiceSource.indexOf("Os.setgid(SHELL_UID)") <
                clipboardUserServiceSource.indexOf("Os.setuid(SHELL_UID)"),
        )
        assertTrue(
            "A nested Binder call must clear the app caller identity so ClipboardService observes the service's shell UID.",
            clipboardUserServiceSource.contains("Binder.clearCallingIdentity()") &&
                clipboardUserServiceSource.contains("com.android.shell"),
        )
        assertTrue(
            "The UserService must implement Shizuku's reserved destroy transaction and exit after an on-demand read.",
            clipboardUserServiceAidl.contains("void destroy() = 16777114") &&
                clipboardUserServiceSource.contains("override fun destroy()") &&
            clipboardUserServiceSource.contains("exitProcess(0)"),
        )
    }

    @Test
    fun samsungClipboardMonitorHandlesPayloadAndNullEventsAndStopsInStandby() {
        val enterStandby = serviceSource.functionBody("private fun enterStandby()")
        val leaveStandby =
            serviceSource.functionBody(
                "private fun leaveStandby(reason: String, immediate: Boolean = false)",
            )

        assertTrue(
            "Samsung clipboard monitoring must exist only while the screen is active.",
            leaveStandby.contains("startClipboardMonitor(") &&
                enterStandby.contains("stopClipboardMonitor()"),
        )
        assertTrue(
            "The Samsung listener must extract SemClipData when Samsung supplies an event payload.",
            clipboardUserServiceSource.contains("addClipboardEventListener") &&
                clipboardUserServiceSource.contains("onClipboardEvent") &&
                clipboardUserServiceSource.contains("extractSamsungText("),
        )
        assertTrue(
            "One UI can send a null SemClipData event, so the root monitor should first read as Android's system identity, which owns READ_CLIPBOARD_IN_BACKGROUND.",
            clipboardUserServiceSource.contains("readPrimaryClipAsSystem()") &&
                clipboardUserServiceSource.contains("withEffectiveUid(SYSTEM_UID)") &&
                clipboardUserServiceSource.contains("packageName = SYSTEM_PACKAGE") &&
                clipboardUserServiceSource.contains("Os.seteuid(uid)") &&
                clipboardUserServiceSource.contains("Binder.clearCallingIdentity()") &&
                clipboardUserServiceSource.contains("Os.seteuid(ROOT_UID)"),
        )
        assertTrue(
            "Samsung can clear the framework clipboard before dispatching its event; the event worker must then read the newly persisted HoneyBoard row with bounded retries.",
            clipboardUserServiceSource.contains("readRecentHoneyboardText(") &&
                clipboardUserServiceSource.contains("ClipItem.db") &&
                clipboardUserServiceSource.contains("SQLiteDatabase.OPEN_READONLY") &&
                clipboardUserServiceSource.contains("Executors.newSingleThreadExecutor") &&
                clipboardUserServiceSource.contains("HONEYBOARD_QUERY_ATTEMPTS") &&
                clipboardUserServiceSource.contains("Thread.sleep(HONEYBOARD_QUERY_RETRY_MS)"),
        )
        assertFalse(
            "The HoneyBoard fallback must remain event-driven and bounded, never an always-on poller.",
            clipboardUserServiceSource.contains("while (true)") ||
                clipboardUserServiceSource.contains("while (isActive)"),
        )
        assertFalse(
            "System identity removes the focus race; accessibility and AIDL must not maintain a stale foreground-package identity.",
            clipboardUserServiceSource.contains("readPrimaryClipAsForegroundSource") ||
                clipboardUserServiceSource.contains("resolvePackageUid") ||
                serviceSource.contains("updateClipboardSource(") ||
                shizukuSource.contains("updateClipboardSource(") ||
                clipboardUserServiceAidl.contains("updateClipboardSource("),
        )
        assertFalse(
            "The Samsung fallback must not use getPrimarySemClip, which re-reads as shell and can clear permission-bearing clips.",
            clipboardUserServiceSource.contains("getPrimarySemClip"),
        )
        assertTrue(
            "The UserService must chunk clipboard text across AIDL so long Samsung clips stay below Binder's transaction limit.",
            clipboardUserServiceAidl.contains("startClipboardMonitor") &&
                clipboardTextListenerAidl.contains("onClipboardTextChunk") &&
                clipboardTextListenerAidl.contains("long transferId") &&
                clipboardTextListenerAidl.contains("int chunkIndex") &&
                clipboardTextListenerAidl.contains("int chunkCount") &&
                clipboardUserServiceSource.contains("AIDL_TEXT_CHUNK_CHARS") &&
                clipboardUserServiceSource.contains("onClipboardTextChunk(") &&
                shizukuSource.contains("ClipboardTextAssembler") &&
                shizukuSource.contains(".daemon(false)"),
        )
        assertFalse(
            "The UserService must not send the complete clipboard as one Binder String transaction.",
            clipboardTextListenerAidl.contains("onClipboardText(String text)") ||
                clipboardUserServiceAidl.contains("String readPrimaryClipText()"),
        )
        assertTrue(
            "The one-shot root clipboard read must use the same chunk listener instead of returning one large String.",
            clipboardUserServiceAidl.contains(
                "boolean readPrimaryClipText(IClipboardTextListener listener)",
            ) &&
                shizukuSource.contains("ClipboardTextAssembler()"),
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
