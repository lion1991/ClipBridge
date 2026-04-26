package com.clipbridge

import android.accessibilityservice.AccessibilityService
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.clipbridge_core.Client
import uniffi.clipbridge_core.ClipKind
import uniffi.clipbridge_core.ClipListener
import uniffi.clipbridge_core.ClipPayload
import uniffi.clipbridge_core.ConnectionState

/**
 * Two paths to picking up clipboard changes on Android 10+, where background
 * `ClipboardManager.getPrimaryClip()` is blocked:
 *
 *   - **Shizuku poller (preferred)**: every 2s, ask the IClipboard system
 *     service through Shizuku's shell-uid binder for the current primary clip.
 *     Catches every kind of copy (system menu, external keyboard, programmatic
 *     setPrimaryClip, etc.).
 *   - **Accessibility events (fallback)**: cache the latest text selection and
 *     publish it when a "copied" toast fires. Works without Shizuku but misses
 *     copies that don't go through the long-press toolbar.
 *
 * Both paths funnel into `publish()` which dedupes by `lastSent` /
 * `lastReceived`, so they happily coexist.
 */
class ClipBridgeAccessibilityService : AccessibilityService() {

    private var client: Client? = null
    private var clipboard: ClipboardManager? = null
    private var clipListener: ClipboardManager.OnPrimaryClipChangedListener? = null
    private var prefsListener: SharedPreferences.OnSharedPreferenceChangeListener? = null

    private var lastSent: String? = null
    private var lastReceived: String? = null
    private var lastSelection: String? = null
    private var lastSelectionAt: Long = 0L

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private var pollerJob: Job? = null

    override fun onServiceConnected() {
        super.onServiceConnected()
        Log.i(TAG, "accessibility service connected")

        clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipListener = ClipboardManager.OnPrimaryClipChangedListener {
            handleLocalClipboardChange()
        }
        clipboard!!.addPrimaryClipChangedListener(clipListener)

        val prefs = getSharedPreferences(PairingStore.PREFS, Context.MODE_PRIVATE)
        prefsListener = SharedPreferences.OnSharedPreferenceChangeListener { _, key ->
            if (key == PairingStore.KEY_PAIRING) {
                Log.i(TAG, "pairing config changed, restarting client")
                restartClient()
            }
        }
        prefs.registerOnSharedPreferenceChangeListener(prefsListener)

        ShizukuBridge.register()
        startShizukuPoller()
        startClient()
    }

    private fun startShizukuPoller() {
        pollerJob?.cancel()
        pollerJob = scope.launch {
            Log.i(TAG, "shizuku poller starting, initial state=${ShizukuBridge.state()}")
            var lastLoggedState: ShizukuBridge.State? = null
            var tickCount = 0
            while (isActive) {
                val state = ShizukuBridge.state()
                if (state != lastLoggedState) {
                    Log.i(TAG, "shizuku state -> $state")
                    lastLoggedState = state
                }
                if (state == ShizukuBridge.State.READY) {
                    val text = ShizukuBridge.readPrimaryClipText()
                    if (++tickCount % 10 == 0) {
                        Log.d(TAG, "shizuku tick $tickCount: text len=${text?.length ?: -1}")
                    }
                    if (!text.isNullOrEmpty()) {
                        withContext(Dispatchers.Main) {
                            if (text != lastReceived && text != lastSent) {
                                Log.i(TAG, "shizuku read NEW: ${text.length} chars")
                                publish(text)
                            }
                        }
                    }
                }
                delay(POLL_INTERVAL_MS)
            }
        }
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        val e = event ?: return
        when (e.eventType) {
            AccessibilityEvent.TYPE_VIEW_SELECTED,
            AccessibilityEvent.TYPE_VIEW_LONG_CLICKED,
            AccessibilityEvent.TYPE_VIEW_TEXT_SELECTION_CHANGED -> rememberSelection(e)

            AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED -> maybeHandleCopyToast(e)
        }
    }

    override fun onInterrupt() {}

    override fun onUnbind(intent: Intent?): Boolean {
        Log.i(TAG, "accessibility service unbinding")
        pollerJob?.cancel()
        pollerJob = null
        scope.cancel()
        ShizukuBridge.unregister()
        clipListener?.let { clipboard?.removePrimaryClipChangedListener(it) }
        clipListener = null
        prefsListener?.let {
            getSharedPreferences(PairingStore.PREFS, Context.MODE_PRIVATE)
                .unregisterOnSharedPreferenceChangeListener(it)
        }
        prefsListener = null
        client?.stop()
        client = null
        return super.onUnbind(intent)
    }

    private fun rememberSelection(event: AccessibilityEvent) {
        val src: AccessibilityNodeInfo? = event.source
        try {
            // 1) For TYPE_VIEW_TEXT_SELECTION_CHANGED on EditText, prefer the
            //    substring computed via fromIndex/toIndex.
            if (event.eventType == AccessibilityEvent.TYPE_VIEW_TEXT_SELECTION_CHANGED) {
                val full = src?.text?.toString()
                if (!full.isNullOrEmpty()) {
                    val start = event.fromIndex.coerceAtLeast(0)
                    val end = event.toIndex.coerceIn(start, full.length)
                    if (end > start) {
                        captureSelection(full.substring(start, end), "selection_changed")
                        return
                    }
                }
            }
            // 2) Otherwise the selected text is whatever's in the source
            //    node — TextViews report the full message text on long-press.
            val text = src?.text?.toString()
                ?: event.contentDescription?.toString()
                ?: event.text?.joinToString("")
            if (!text.isNullOrEmpty()) {
                captureSelection(text, "${AccessibilityEvent.eventTypeToString(event.eventType)}")
            }
        } finally {
            src?.recycle()
        }
    }

    private fun captureSelection(text: String, source: String) {
        val trimmed = text.trim()
        if (trimmed.isEmpty()) return
        lastSelection = trimmed
        lastSelectionAt = System.currentTimeMillis()
        Log.d(TAG, "remember selection ($source, ${trimmed.length} chars)")
    }

    private fun maybeHandleCopyToast(event: AccessibilityEvent) {
        val text = event.text?.joinToString(" ") ?: return
        if (!looksLikeCopyToast(text)) return

        val sel = lastSelection
        if (sel.isNullOrEmpty()) {
            Log.w(TAG, "copy toast '$text' but no recent selection cached")
            return
        }
        // Ignore stale selections (e.g. user copied something an hour ago).
        if (System.currentTimeMillis() - lastSelectionAt > 30_000) {
            Log.w(TAG, "selection too old (${System.currentTimeMillis() - lastSelectionAt}ms), ignoring")
            return
        }
        Log.i(TAG, "copy detected via toast='$text', publishing ${sel.length} chars")
        publish(sel)
    }

    /**
     * Match common localized "Copied"/"Copied to clipboard" toasts. A loose
     * substring match on a few stems is intentional — better to occasionally
     * re-publish the same clip (deduped by `lastSent`) than to miss copies.
     */
    private fun looksLikeCopyToast(text: String): Boolean {
        val lower = text.lowercase()
        return lower.contains("copied") ||
                text.contains("已复制") || text.contains("已複製") ||
                text.contains("已剪切") || text.contains("已剪下") ||
                text.contains("コピー") || text.contains("복사")
    }

    private fun handleLocalClipboardChange() {
        val cb = clipboard ?: return
        val cd = cb.primaryClip ?: return
        if (cd.itemCount == 0) return
        val text = cd.getItemAt(0).coerceToText(this).toString()
        if (text.isEmpty()) return
        publish(text)
    }

    private fun publish(text: String) {
        if (text == lastReceived) {
            Log.i(TAG, "skip: matches lastReceived")
            return
        }
        if (text == lastSent) {
            Log.i(TAG, "skip: matches lastSent")
            return
        }
        lastSent = text
        val payload = ClipPayload(
            kind = ClipKind.TEXT,
            content = text,
            deviceName = android.os.Build.MODEL ?: "Android",
            ts = System.currentTimeMillis().toULong(),
        )
        try {
            client?.sendClip(payload)
            Log.i(TAG, "published clip (${text.length} chars)")
        } catch (t: Throwable) {
            Log.e(TAG, "sendClip failed", t)
        }
    }

    private fun startClient() {
        val config = PairingStore.load(this) ?: run {
            Log.w(TAG, "no pairing config; client idle until user pairs")
            return
        }
        val key = config.keyBytes()
        if (key == null || key.size != 32) {
            Log.w(TAG, "invalid key in pairing config")
            return
        }
        val deviceId = PairingStore.deviceId(this)
        client = try {
            Client(
                relayUrl = config.relayUrl,
                groupId = config.groupId,
                key = key,
                deviceId = deviceId,
                listener = object : ClipListener {
                    override fun onClip(payload: ClipPayload) {
                        handleRemoteClip(payload)
                    }

                    override fun onState(state: ConnectionState) {
                        Log.i(TAG, "state: $state")
                    }
                },
            )
        } catch (t: Throwable) {
            Log.e(TAG, "failed to start client", t)
            null
        }
    }

    private fun restartClient() {
        client?.stop()
        client = null
        startClient()
    }

    private fun handleRemoteClip(payload: ClipPayload) {
        if (payload.kind != ClipKind.TEXT) return
        Log.i(TAG, "remote clip received (${payload.content.length} chars)")
        lastReceived = payload.content
        clipboard?.setPrimaryClip(ClipData.newPlainText("ClipBridge", payload.content))
    }

    companion object {
        private const val TAG = "ClipBridge"
        private const val POLL_INTERVAL_MS = 2_000L
    }
}
