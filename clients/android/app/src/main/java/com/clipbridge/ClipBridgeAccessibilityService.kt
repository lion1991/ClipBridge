package com.clipbridge

import android.accessibilityservice.AccessibilityService
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.net.Uri
import android.net.wifi.WifiManager
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.clipbridge_core.Client
import uniffi.clipbridge_core.ClipKind
import uniffi.clipbridge_core.ClipListener
import uniffi.clipbridge_core.ClipPayload
import uniffi.clipbridge_core.ConnectionState
import uniffi.clipbridge_core.ImageMeta

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
 * Both paths funnel into `publish()`, which suppresses echoes of remote
 * writes (within `ECHO_WINDOW_MS`) and collapses near-simultaneous fires
 * from the two sources (within `SOURCE_DEDUPE_MS`), so they coexist.
 */
class ClipBridgeAccessibilityService : AccessibilityService() {

    private var client: Client? = null
    private var clipboard: ClipboardManager? = null
    private var clipListener: ClipboardManager.OnPrimaryClipChangedListener? = null
    private var prefsListener: SharedPreferences.OnSharedPreferenceChangeListener? = null
    // Held while a Client is alive, so the LAN transport's mDNS multicast
    // packets aren't dropped by Wi-Fi power save. Released when the client
    // is stopped (pairing change, service unbind) so we don't keep Wi-Fi
    // hot when there's no client to use it.
    private var multicastLock: WifiManager.MulticastLock? = null

    // Most recent content we wrote to the clipboard from a remote clip.
    // Any local change that matches this within ECHO_WINDOW_MS is treated as
    // our own write and not republished. Outside the window the user can
    // re-share the same text on purpose.
    @Volatile private var expectedEcho: String? = null
    @Volatile private var expectedEchoAt: Long = 0L
    // Short window to dedupe the multiple sources (clipboard listener,
    // Shizuku poller, copy toast) all firing for the same user copy.
    @Volatile private var lastPublished: String? = null
    @Volatile private var lastPublishedAt: Long = 0L
    private var lastSelection: String? = null
    private var lastSelectionAt: Long = 0L

    // Image-side dedup. Pixel-content hash of every image we've sent or
    // received recently — used to drop both our own write echoes (the
    // listener fires after we put the image on the clipboard) and any
    // re-encoded duplicates the system might produce when ferrying URIs
    // through ContentResolver. Bounded so a long-running session can't
    // grow it unbounded; entries beyond capacity get LRU-evicted.
    private val recentImageHashes = LinkedHashSet<String>()
    private val recentImageHashesCap = 32
    // Last image URI we observed via the Shizuku poller, dedup'd by URI
    // string. Tracks "is this the same clip we already saw?" between
    // ticks; cross-source dedup with the listener happens in publishImage
    // via `recentImageHashes`.
    private var lastPolledImageUri: String? = null

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private var pollerJob: Job? = null
    private var lanCountJob: Job? = null

    init {
        _stateFlow.value = UiConnState.Idle
    }

    override fun onServiceConnected() {
        super.onServiceConnected()
        Log.i(TAG, "accessibility service connected")
        instanceRef = this

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
        startLanCountPoller()
        startClient()
    }

    private fun startLanCountPoller() {
        lanCountJob?.cancel()
        lanCountJob = scope.launch {
            while (isActive) {
                val names = client?.lanPeers().orEmpty().sorted()
                _lanPeerNames.value = names
                _lanPeerCount.value = names.size
                delay(2_000)
            }
        }
    }

    private fun startShizukuPoller() {
        pollerJob?.cancel()
        pollerJob = scope.launch {
            Log.i(TAG, "shizuku poller starting, initial state=${ShizukuBridge.state()}")
            var lastLoggedState: ShizukuBridge.State? = null
            var tickCount = 0
            // Poll-local change detection: only act when the read content differs
            // from what we saw last tick. Cross-source dedupe with the clipboard
            // listener and copy toast happens inside `publish*()`.
            var lastPolledText: String? = null
            while (isActive) {
                val state = ShizukuBridge.state()
                if (state != lastLoggedState) {
                    Log.i(TAG, "shizuku state -> $state")
                    lastLoggedState = state
                }
                if (state == ShizukuBridge.State.READY) {
                    val clip = ShizukuBridge.readPrimaryClip()
                    if (++tickCount % 10 == 0) {
                        Log.d(TAG, "shizuku tick $tickCount: clip=$clip")
                    }
                    when (clip) {
                        is ShizukuBridge.Clip.Text -> {
                            val text = clip.value
                            if (text.isNotEmpty() && text != lastPolledText) {
                                lastPolledText = text
                                withContext(Dispatchers.Main) {
                                    Log.i(TAG, "shizuku read NEW text: ${text.length} chars")
                                    publish(text)
                                }
                            }
                        }
                        is ShizukuBridge.Clip.ImageUri -> {
                            val key = clip.uri.toString()
                            if (key != lastPolledImageUri) {
                                lastPolledImageUri = key
                                Log.i(TAG, "shizuku read NEW image uri: $key")
                                val outbound = ImagePipeline.outboundFromUri(
                                    this@ClipBridgeAccessibilityService,
                                    clip.uri,
                                )
                                if (outbound != null) {
                                    withContext(Dispatchers.Main) { publishImage(outbound) }
                                }
                            }
                        }
                        null -> { /* Shizuku not ready or read failed */ }
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
        if (instanceRef === this) instanceRef = null
        pollerJob?.cancel()
        pollerJob = null
        lanCountJob?.cancel()
        lanCountJob = null
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
        releaseMulticastLock()
        _stateFlow.value = UiConnState.Idle
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

        // Image first: if the description advertises any image/* mime, take
        // that path. coerceToText on an image clip returns the URI as a
        // string, which we don't want to ship as a text clip.
        val desc = cd.description
        for (i in 0 until desc.mimeTypeCount) {
            if (desc.getMimeType(i).startsWith("image/")) {
                val uri = cd.getItemAt(0).uri ?: return
                scope.launch(Dispatchers.IO) {
                    val outbound = ImagePipeline.outboundFromUri(
                        this@ClipBridgeAccessibilityService,
                        uri,
                    ) ?: return@launch
                    withContext(Dispatchers.Main) { publishImage(outbound) }
                }
                return
            }
        }

        val text = cd.getItemAt(0).coerceToText(this).toString()
        if (text.isEmpty()) return
        publish(text)
    }

    private fun publish(text: String) {
        // If the system clipboard currently holds an image, suppress the
        // text publish entirely. Samsung Gallery (and similar) puts both
        // an image URI AND a text label (e.g. "幻灯片 1，共 1165 张")
        // on the clipboard when the user copies a photo; the accessibility
        // copy-toast handler captures the label and would otherwise
        // broadcast that as a meaningless text clip to other devices.
        // The image branch handles the actual content separately.
        clipboardHasImage()?.let { mime ->
            Log.i(TAG, "skip text publish: clipboard is $mime, image branch will handle")
            return
        }
        val now = System.currentTimeMillis()
        // Echo of a recent remote write — skip without consuming, so other
        // sources (poller, toast) firing for the same change all skip too.
        if (text == expectedEcho && now - expectedEchoAt < ECHO_WINDOW_MS) {
            Log.i(TAG, "skip: matches expectedEcho")
            return
        }
        // Multiple sources can fire for one user copy; collapse them.
        if (text == lastPublished && now - lastPublishedAt < SOURCE_DEDUPE_MS) {
            Log.i(TAG, "skip: duplicate within ${SOURCE_DEDUPE_MS}ms")
            return
        }
        lastPublished = text
        lastPublishedAt = now
        val payload = ClipPayload(
            kind = ClipKind.TEXT,
            content = text,
            deviceName = android.os.Build.MODEL ?: "Android",
            ts = System.currentTimeMillis().toULong(),
            image = null,
        )
        try {
            client?.sendClip(payload)
            Log.i(TAG, "published clip (${text.length} chars)")
        } catch (t: Throwable) {
            Log.e(TAG, "sendClip failed", t)
        }
    }

    /**
     * Image counterpart to `publish(text)`. Two callers:
     *
     *   - Clipboard listener / Shizuku poller (clipboard activity) — passes
     *     `dedup = true` so multiple sources firing for the same copy
     *     collapse, AND so a re-paste of an image we just received doesn't
     *     bounce back to the source device.
     *   - sendImageFromUri (picker / explicit user action) — passes
     *     `dedup = false` so the user can re-send the same image on
     *     purpose. Picker doesn't touch the system clipboard so there's
     *     no echo to suppress.
     */
    private fun publishImage(outbound: ImagePipeline.Outbound, dedup: Boolean = true) {
        if (outbound.bytes.size > ImagePipeline.MAX_IMAGE_BYTES) {
            val mb = outbound.bytes.size / 1024 / 1024
            Log.w(TAG, "image ${mb}MB exceeds ${ImagePipeline.MAX_IMAGE_BYTES} bytes, skipping")
            return
        }
        val h = ImagePipeline.pixelHashHex(outbound.bytes)
            ?: ImagePipeline.sha256Hex(outbound.bytes)
        if (dedup) {
            if (rememberImageHash(h)) {
                Log.i(TAG, "skip image: matches recent hash")
                return
            }
        }
        // Explicit sends still record the hash so the immediate clipboard
        // listener fire (if any) sees it and skips. Without this, a manual
        // copy of the just-sent image right after the picker would
        // re-publish via the listener.
        // Note: rememberImageHash returns true for "already present" but
        // we don't care about the return value here; we just want it in.
        if (!dedup) rememberImageHash(h)

        val deviceName = android.os.Build.MODEL ?: "Android"
        val ts = System.currentTimeMillis()
        // Surface in the UI history immediately — the upload may take a
        // second or two on slow uplinks and we don't want the row to lag.
        appendImageHistory(
            ImageHistoryEntry(
                id = h,
                bytes = outbound.bytes,
                mime = outbound.mime,
                width = outbound.width.toInt(),
                height = outbound.height.toInt(),
                deviceName = "$deviceName · 本机",
                tsMillis = ts,
                direction = ImageHistoryEntry.Direction.SENT,
            )
        )
        scope.launch(Dispatchers.IO) {
            try {
                client?.sendImage(
                    imageBytes = outbound.bytes,
                    mimeType = outbound.mime,
                    width = outbound.width,
                    height = outbound.height,
                    deviceName = deviceName,
                    ts = ts.toULong(),
                )
                Log.i(TAG, "published image (${outbound.bytes.size}B, " +
                    "${outbound.width}×${outbound.height})")
            } catch (t: Throwable) {
                Log.e(TAG, "sendImage failed", t)
            }
        }
    }

    /**
     * Returns the first image-typed mime currently advertised by the
     * system clipboard, or null if empty / non-image. Used by the text
     * publish path to skip when an image clip is what the user actually
     * intended.
     */
    private fun clipboardHasImage(): String? {
        val cd = clipboard?.primaryClip ?: return null
        val desc = cd.description
        for (i in 0 until desc.mimeTypeCount) {
            val mime = desc.getMimeType(i)
            if (mime.startsWith("image/")) return mime
        }
        return null
    }

    /**
     * Insert into the bounded LRU `recentImageHashes`. Returns true if the
     * hash was already present (so the caller knows to skip), false if
     * this is a new hash and we just inserted it.
     */
    @Synchronized
    private fun rememberImageHash(hash: String): Boolean {
        if (recentImageHashes.contains(hash)) {
            // Refresh recency.
            recentImageHashes.remove(hash)
            recentImageHashes.add(hash)
            return true
        }
        recentImageHashes.add(hash)
        while (recentImageHashes.size > recentImageHashesCap) {
            val oldest = recentImageHashes.iterator().next()
            recentImageHashes.remove(oldest)
        }
        return false
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
        val deviceName = android.os.Build.MODEL ?: "Android"
        acquireMulticastLock()
        client = try {
            Client(
                relayUrl = config.relayUrl,
                groupId = config.groupId,
                key = key,
                deviceId = deviceId,
                deviceName = deviceName,
                listener = object : ClipListener {
                    override fun onClip(payload: ClipPayload) {
                        handleRemoteClip(payload)
                    }

                    override fun onState(state: ConnectionState) {
                        Log.i(TAG, "state: $state")
                        _stateFlow.value = when (state) {
                            ConnectionState.Connecting -> UiConnState.Connecting
                            ConnectionState.Connected -> UiConnState.Connected
                            ConnectionState.Disconnected -> UiConnState.Disconnected
                            is ConnectionState.Error -> UiConnState.Error(state.message)
                        }
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
        releaseMulticastLock()
        startClient()
    }

    private fun acquireMulticastLock() {
        if (multicastLock?.isHeld == true) return
        try {
            val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
                ?: return
            val lock = wifi.createMulticastLock("clipbridge-mdns").apply {
                // Hold across rebinds — we'll release it explicitly in
                // onUnbind / restartClient.
                setReferenceCounted(false)
                acquire()
            }
            multicastLock = lock
            Log.i(TAG, "multicast lock acquired")
        } catch (t: Throwable) {
            // Missing CHANGE_WIFI_MULTICAST_STATE shouldn't happen (it's in
            // the manifest) but if it does we degrade to relay-only rather
            // than crashing the service.
            Log.w(TAG, "failed to acquire multicast lock; LAN may be limited", t)
        }
    }

    private fun releaseMulticastLock() {
        multicastLock?.let {
            if (it.isHeld) {
                try { it.release() } catch (_: Throwable) {}
            }
        }
        multicastLock = null
    }

    /**
     * Public entry point for the picker-driven send: takes a URI from the
     * PickVisualMedia activity result, reads bytes via ContentResolver
     * (foreground activity has temp permission), and routes through the
     * same `publishImage` path as a clipboard-driven send.
     */
    fun sendImageFromUri(uri: Uri) {
        scope.launch(Dispatchers.IO) {
            val outbound = ImagePipeline.outboundFromUri(
                this@ClipBridgeAccessibilityService,
                uri,
            ) ?: run {
                Log.w(TAG, "sendImageFromUri: outbound was null for $uri")
                return@launch
            }
            // Explicit user action — bypass dedup so re-picking the same
            // image actually re-sends.
            withContext(Dispatchers.Main) { publishImage(outbound, dedup = false) }
        }
    }

    private fun handleRemoteClip(payload: ClipPayload) {
        when (payload.kind) {
            ClipKind.TEXT -> {
                Log.i(TAG, "remote text clip (${payload.content.length} chars)")
                // Mark as expected echo *before* writing so the resulting
                // OnPrimaryClipChangedListener / Shizuku tick recognises it.
                expectedEcho = payload.content
                expectedEchoAt = System.currentTimeMillis()
                clipboard?.setPrimaryClip(
                    ClipData.newPlainText("ClipBridge", payload.content)
                )
            }
            ClipKind.IMAGE -> handleRemoteImage(payload)
        }
    }

    private fun handleRemoteImage(payload: ClipPayload) {
        val meta = payload.image
        if (meta == null) {
            Log.w(TAG, "image clip missing meta, skipping")
            return
        }
        Log.i(TAG, "remote image clip (${meta.width}×${meta.height}, ${meta.sizeBytes}B)")
        scope.launch(Dispatchers.IO) {
            val bytes = try {
                client?.fetchImage(meta)
            } catch (t: Throwable) {
                Log.w(TAG, "fetchImage failed: ${t.message}")
                null
            } ?: return@launch

            val h = ImagePipeline.pixelHashHex(bytes) ?: ImagePipeline.sha256Hex(bytes)
            if (rememberImageHash(h)) {
                // Already on the clipboard from an earlier path (e.g. our
                // own poll picked up the same image moments ago). Skip
                // the redundant write — clobbering would just bump
                // changeCount for nothing.
                Log.i(TAG, "skip remote image: matches recent hash")
                return@launch
            }
            withContext(Dispatchers.Main) {
                val cb = clipboard ?: return@withContext
                val ok = ImagePipeline.writeImageToClipboard(
                    cb,
                    this@ClipBridgeAccessibilityService,
                    bytes,
                    meta.mimeType,
                )
                if (!ok) Log.w(TAG, "writeImageToClipboard returned false")
                else Log.i(TAG, "wrote remote image to clipboard")
                appendImageHistory(
                    ImageHistoryEntry(
                        id = h,
                        bytes = bytes,
                        mime = meta.mimeType,
                        width = meta.width.toInt(),
                        height = meta.height.toInt(),
                        deviceName = payload.deviceName,
                        tsMillis = payload.ts.toLong(),
                        direction = ImageHistoryEntry.Direction.RECEIVED,
                    )
                )
            }
        }
    }

    companion object {
        private const val TAG = "ClipBridge"
        private const val POLL_INTERVAL_MS = 2_000L
        // See `expectedEcho` doc above the field.
        private const val ECHO_WINDOW_MS = 10_000L
        // Long enough to cover one Shizuku poll interval after a listener fire,
        // short enough that an intentional re-copy of the same text still goes.
        private const val SOURCE_DEDUPE_MS = 3_000L

        // In-process state for the UI to observe. AS and Activity share the
        // same process (no android:process attribute on either component) so
        // a plain MutableStateFlow is the cheapest reactive bridge.
        private val _stateFlow = MutableStateFlow<UiConnState>(UiConnState.Idle)
        val stateFlow: StateFlow<UiConnState> = _stateFlow.asStateFlow()

        // LAN peer count, polled from the Rust core every couple of seconds
        // by the service. The UI binds this to render a "局域网:N / 仅中继"
        // badge alongside the existing connection state.
        private val _lanPeerCount = MutableStateFlow(0)
        val lanPeerCount: StateFlow<Int> = _lanPeerCount.asStateFlow()

        // Device names of currently-connected LAN peers, polled from the
        // Rust core. UI surfaces these so the user can spot mesh asymmetry
        // ("Android sees Mac+iPhone but Mac only sees Android").
        private val _lanPeerNames = MutableStateFlow<List<String>>(emptyList())
        val lanPeerNames: StateFlow<List<String>> = _lanPeerNames.asStateFlow()

        // Image traffic history surfaced to the UI's image transfer card.
        // Newest first, capped at HISTORY_LIMIT. Sent and received both
        // appear here so the user can save / re-share their own outbound
        // images too (no separate "sent" tab on Android — keeps the
        // mobile UI compact).
        private const val HISTORY_LIMIT = 12
        private val _imageHistory = MutableStateFlow<List<ImageHistoryEntry>>(emptyList())
        val imageHistory: StateFlow<List<ImageHistoryEntry>> = _imageHistory.asStateFlow()

        internal fun appendImageHistory(entry: ImageHistoryEntry) {
            val combined = (listOf(entry) + _imageHistory.value)
                .distinctBy { it.id }
                .take(HISTORY_LIMIT)
            _imageHistory.value = combined
        }

        internal fun clearImageHistory() {
            _imageHistory.value = emptyList()
        }

        // Weak handle to the live AccessibilityService instance for the UI
        // to call into (sendImageFromUri). Not WeakRef because the service
        // sets/clears it explicitly on connect/unbind, no GC race window.
        @Volatile
        private var instanceRef: ClipBridgeAccessibilityService? = null
        fun activeService(): ClipBridgeAccessibilityService? = instanceRef
    }
}

/// Compact metadata + bytes for one image, surfaced to the UI. Bytes live
/// here so "保存到相册" / "分享" don't need to refetch from the relay.
data class ImageHistoryEntry(
    val id: String,             // sha256 of bytes — also acts as dedup key
    val bytes: ByteArray,
    val mime: String,
    val width: Int,
    val height: Int,
    val deviceName: String,
    val tsMillis: Long,
    val direction: Direction,
) {
    enum class Direction { RECEIVED, SENT }

    val sizeLabel: String
        get() {
            val kb = (bytes.size / 1024).coerceAtLeast(1)
            return if (kb >= 1024) String.format("%.1f MB", kb / 1024.0) else "$kb KB"
        }

    override fun equals(other: Any?): Boolean = other is ImageHistoryEntry && other.id == id
    override fun hashCode(): Int = id.hashCode()
}

/// What the UI displays. Distinct from `uniffi.clipbridge_core.ConnectionState`
/// so the UI doesn't need to import the FFI types.
sealed class UiConnState {
    data object Idle : UiConnState()
    data object Connecting : UiConnState()
    data object Connected : UiConnState()
    data object Disconnected : UiConnState()
    data class Error(val message: String) : UiConnState()
}
