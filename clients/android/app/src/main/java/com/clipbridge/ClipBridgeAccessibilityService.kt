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
import uniffi.clipbridge_core.Client
import uniffi.clipbridge_core.ClipKind
import uniffi.clipbridge_core.ClipListener
import uniffi.clipbridge_core.ClipPayload
import uniffi.clipbridge_core.ConnectionState

/**
 * Detects user-initiated copies via accessibility events. Android 10+ blocks
 * `ClipboardManager.getPrimaryClip()` from background apps even with an enabled
 * accessibility service, so we instead:
 *
 *   1. Track the most recently selected text from `TYPE_VIEW_SELECTED`,
 *      `TYPE_VIEW_LONG_CLICKED`, and `TYPE_VIEW_TEXT_SELECTION_CHANGED`. The
 *      text usually lives in `event.source.text` or `event.contentDescription`.
 *   2. Watch for `TYPE_NOTIFICATION_STATE_CHANGED` with a "copied" toast (from
 *      the source app or from `com.android.systemui`). When that fires, we
 *      publish the cached selection — that's our "user pressed Copy" signal.
 *
 * The legacy `OnPrimaryClipChangedListener` is also kept for the case where
 * ClipBridge itself is in the foreground (system grants reads then).
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

        startClient()
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
    }
}
