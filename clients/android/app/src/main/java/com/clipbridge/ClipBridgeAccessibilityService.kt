package com.clipbridge

import android.accessibilityservice.AccessibilityService
import android.content.BroadcastReceiver
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.SharedPreferences
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Environment
import android.os.PowerManager
import android.net.wifi.WifiManager
import android.provider.OpenableColumns
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
import java.io.File
import java.util.UUID

/**
 * Three paths to picking up clipboard changes on Android 10+, where background
 * `ClipboardManager.getPrimaryClip()` is blocked:
 *
 *   - **Samsung event monitor**: while the screen is interactive on a rooted
 *     Samsung device, consume SemClipData when supplied by semclipboard. When
 *     One UI sends only a change notification, synchronously read as the
 *     Android system identity before Samsung clears permission-bearing clips.
 *   - **On-demand Shizuku read (preferred)**: when the clipboard listener's
 *     background read is denied, after a copy toast, or after a remote write,
 *     ask the IClipboard system service through Shizuku's shell-uid binder for
 *     the current primary clip. This catches system clipboard state without
 *     keeping an always-on polling loop alive in standby.
 *   - **Accessibility events (fallback)**: cache the latest text selection and
 *     publish it when a "copied" toast fires. Works without Shizuku but misses
 *     copies that don't go through the long-press toolbar.
 *
 * All paths funnel into `publish()`, which suppresses echoes of remote
 * writes (within `ECHO_WINDOW_MS`) and collapses near-simultaneous fires
 * from the sources (within `SOURCE_DEDUPE_MS`), so they coexist.
 */
class ClipBridgeAccessibilityService : AccessibilityService() {

    private var client: Client? = null
    private var clipboard: ClipboardManager? = null
    private var clipListener: ClipboardManager.OnPrimaryClipChangedListener? = null
    private var prefsListener: SharedPreferences.OnSharedPreferenceChangeListener? = null
    private var screenReceiver: BroadcastReceiver? = null
    private var connectivityManager: ConnectivityManager? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var shizukuStateListener: ShizukuBridge.StateListener? = null
    // Held only while Android LAN mode is active. Screen-off standby releases
    // it; transfer activity opens a short LAN window so mDNS/TCP LAN paths can
    // wake briefly without keeping Wi-Fi multicast hot all night.
    private var multicastLock: WifiManager.MulticastLock? = null

    // Most recent content we wrote to the clipboard from a remote clip.
    // Any local change that matches this within ECHO_WINDOW_MS is treated as
    // our own write and not republished. Outside the window the user can
    // re-share the same text on purpose.
    @Volatile private var expectedEcho: String? = null
    @Volatile private var expectedEchoAt: Long = 0L
    // Short window to dedupe the multiple sources (clipboard listener,
    // Shizuku read, copy toast) all firing for the same user copy.
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
    // Last image URI observed via an on-demand Shizuku read. Text deduplication
    // stays time-bounded in publish(), so intentionally copying the same text
    // later is never suppressed forever.
    private var lastPolledImageUri: String? = null

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private var shizukuReadJob: Job? = null
    private var lanCountJob: Job? = null
    private var lanActiveJob: Job? = null
    private var leaveStandbyDebounceJob: Job? = null
    private var foregroundReconnectJob: Job? = null
    @Volatile private var reconnectIdleMode: Boolean = false
    @Volatile private var lanActive: Boolean = true
    @Volatile private var hasLanNetwork: Boolean = false
    // True between the host activity's ON_RESUME and ON_PAUSE. Gates the
    // foreground reconnect watchdog so it never runs while the user is away.
    @Volatile private var hostAppForeground: Boolean = false

    // Latest remote image clip received while the screen was off. Fetched
    // on screen-on; cleared whenever a newer clip (remote text or a local
    // copy) makes flushing it wrong.
    @Volatile private var pendingRemoteImage: ClipPayload? = null

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
        registerNetworkStateCallback()
        registerScreenStateReceiver()
        shizukuStateListener = ShizukuBridge.StateListener { state ->
            if (state == ShizukuBridge.State.READY && !isScreenOffForStandby()) {
                startClipboardMonitor()
            } else {
                ShizukuBridge.stopClipboardMonitor()
            }
        }.also(ShizukuBridge::addStateListener)
        startLanCountPoller()
        startClient()
    }

    private fun registerScreenStateReceiver() {
        if (screenReceiver != null) return
        screenReceiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                when (intent?.action) {
                    Intent.ACTION_SCREEN_OFF -> enterStandby()
                    Intent.ACTION_SCREEN_ON,
                    Intent.ACTION_USER_PRESENT -> leaveStandby(reason = intent.action ?: "screen on")
                }
            }
        }
        registerReceiver(
            screenReceiver,
            IntentFilter().apply {
                addAction(Intent.ACTION_SCREEN_OFF)
                addAction(Intent.ACTION_SCREEN_ON)
                addAction(Intent.ACTION_USER_PRESENT)
            },
        )
        if (isScreenOffForStandby()) {
            enterStandby()
        } else {
            // Set the initial LAN policy before Client starts so a
            // mobile-only launch does not retain the default true state.
            leaveStandby(reason = "service connected", immediate = true)
        }
    }

    private fun unregisterScreenStateReceiver() {
        screenReceiver?.let {
            try { unregisterReceiver(it) } catch (_: Throwable) {}
        }
        screenReceiver = null
    }

    private fun isScreenOffForStandby(): Boolean {
        val pm = getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return false
        return !pm.isInteractive
    }

    private fun registerNetworkStateCallback() {
        if (networkCallback != null) return
        val manager = getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
            ?: return
        connectivityManager = manager
        hasLanNetwork = manager.activeNetwork
            ?.let(manager::getNetworkCapabilities)
            ?.isLanCapable()
            ?: false
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                refreshLanNetworkAvailability("default network available")
                requestRelayReconnect("default network available")
            }

            override fun onCapabilitiesChanged(
                network: Network,
                capabilities: NetworkCapabilities,
            ) {
                refreshLanNetworkAvailability("default network capabilities changed")
            }

            override fun onLost(network: Network) {
                refreshLanNetworkAvailability("default network lost")
            }
        }
        networkCallback = callback
        try {
            manager.registerDefaultNetworkCallback(callback)
        } catch (t: Throwable) {
            networkCallback = null
            Log.w(TAG, "network callback registration failed: ${t.message}")
        }
    }

    private fun unregisterNetworkStateCallback() {
        val callback = networkCallback ?: return
        try {
            connectivityManager?.unregisterNetworkCallback(callback)
        } catch (_: Throwable) {
        }
        networkCallback = null
        connectivityManager = null
    }

    private fun refreshLanNetworkAvailability(reason: String) {
        scope.launch(Dispatchers.Main) {
            val manager = connectivityManager ?: return@launch
            hasLanNetwork = manager.activeNetwork
                ?.let(manager::getNetworkCapabilities)
                ?.isLanCapable()
                ?: false
            applyLanTransportPolicy(reason = reason, refresh = hasLanNetwork)
        }
    }

    private fun applyLanTransportPolicy(reason: String, refresh: Boolean) {
        val enabled = shouldEnableLanTransport(
            isTransportAwake = !reconnectIdleMode,
            hasLanNetwork = hasLanNetwork,
        )
        val changed = lanActive != enabled
        lanActive = enabled
        client?.setLanActive(enabled)
        if (enabled) {
            if (client != null) acquireMulticastLock()
            if (refresh) {
                try {
                    client?.refreshLanNow()
                } catch (t: Throwable) {
                    Log.w(TAG, "LAN refresh failed after network change: ${t.message}")
                }
            }
        } else {
            _lanFilePeers.value = emptyList()
            _lanPeerNames.value = emptyList()
            _lanPeerCount.value = 0
            releaseMulticastLock()
        }
        if (changed) {
            Log.i(TAG, "LAN transport active=$enabled: $reason")
        }
    }

    /**
     * STANDBY: hard-suspend relay WS + fully tear down LAN (mDNS, TCP
     * listener, peer sessions). Accessibility service stays up.
     */
    private fun enterStandby() {
        leaveStandbyDebounceJob?.cancel()
        leaveStandbyDebounceJob = null
        lanActiveJob?.cancel()
        lanActiveJob = null
        reconnectIdleMode = true
        lanActive = false
        client?.setReconnectIdleMode(true)
        client?.setLanActive(false)
        ShizukuBridge.stopClipboardMonitor()
        _lanFilePeers.value = emptyList()
        _lanPeerNames.value = emptyList()
        _lanPeerCount.value = 0
        releaseMulticastLock()
        Log.i(TAG, "entered standby (relay hard-suspend + LAN down)")
    }

    /**
     * ACTIVE: reconnect relay, restore LAN, refresh advertise. Debounced so
     * rapid lock/unlock thrash does not storm reconnects.
     */
    private fun leaveStandby(reason: String, immediate: Boolean = false) {
        leaveStandbyDebounceJob?.cancel()
        // Cancel any temporary wake re-entry timer; a real leave (unlock /
        // foreground) must not fall back into standby after the window.
        // Temporary wake itself re-arms this job after calling leaveStandby.
        lanActiveJob?.cancel()
        lanActiveJob = null
        val run = {
            reconnectIdleMode = false
            client?.setReconnectIdleMode(false)
            applyLanTransportPolicy(reason = reason, refresh = true)
            if (isScreenOffForStandby()) {
                ShizukuBridge.stopClipboardMonitor()
            } else {
                startClipboardMonitor()
            }
            try {
                client?.fetchRecent()
            } catch (t: Throwable) {
                Log.w(TAG, "leaveStandby fetch failed: ${t.message}")
            }
            pendingRemoteImage?.let {
                pendingRemoteImage = null
                Log.i(TAG, "screen on, fetching deferred remote image")
                handleRemoteImage(it)
            }
            Log.i(TAG, "left standby: $reason")
        }
        if (immediate) {
            leaveStandbyDebounceJob = null
            run()
            return
        }
        // Main-confined: enterStandby runs on main (broadcast callbacks), and
        // once past delay() the body is no longer cancellable — dispatching it
        // anywhere else lets a concurrent lock interleave with it and end up
        // re-activated (LAN + multicast held) while the screen is off.
        leaveStandbyDebounceJob = scope.launch(Dispatchers.Main) {
            delay(LEAVE_STANDBY_DEBOUNCE_MS)
            leaveStandbyDebounceJob = null
            run()
        }
    }

    private fun startClipboardMonitor() {
        ShizukuBridge.startClipboardMonitor { text ->
            scope.launch(Dispatchers.Main) {
                Log.i(TAG, "Samsung clipboard event (${text.length} chars)")
                publish(text)
            }
        }
    }

    /**
     * Temporary full wake while the screen is still off (local copy / file
     * send). Opens both LAN and relay so publish/upload can leave the
     * device, then re-enters standby if still non-interactive.
     */
    private fun activateLanTemporarily(reason: String) {
        leaveStandby(reason = "wake window: $reason", immediate = true)
        lanActiveJob?.cancel()
        if (!isScreenOffForStandby()) return

        // Main-confined for the same reason as the leave-standby debounce:
        // the expiry enterStandby must serialize with an unlock's
        // leaveStandby instead of interleaving from another thread.
        lanActiveJob = scope.launch(Dispatchers.Main) {
            Log.i(TAG, "temporary full-wake window opened: $reason")
            delay(LAN_ACTIVE_WINDOW_MS)
            lanActiveJob = null
            if (isScreenOffForStandby()) {
                enterStandby()
            }
        }
    }

    /**
     * The app came to the foreground. Android grants no dependable background
     * lifetime, so arriving here disconnected is the normal case rather than an
     * error worth showing the user: leave standby, then actively re-establish
     * the link instead of waiting out whatever backoff the core is in.
     */
    fun onHostAppForeground() {
        hostAppForeground = true
        leaveStandby(reason = "host app foreground", immediate = true)
        ensureConnected("host app foreground")
        startForegroundReconnectWatchdog()
    }

    /** The app went away; stop the watchdog so it can't poll from the background. */
    fun onHostAppBackground() {
        hostAppForeground = false
        foregroundReconnectJob?.cancel()
        foregroundReconnectJob = null
    }

    /** User tapped the status pill. Same path as a foreground transition. */
    fun requestManualReconnect() {
        leaveStandby(reason = "manual reconnect", immediate = true)
        ensureConnected("manual reconnect")
    }

    /**
     * Bring the transport back up, whatever state it's in:
     *
     *   - No client at all (never paired when the service started, or the
     *     constructor threw) — build one now. Nothing else retries this, so
     *     without it a single early failure leaves the app dead until the
     *     pairing config changes or the service rebinds.
     *   - Client present — [Client.reconnectNow] cancels a pending reconnect
     *     backoff, and probes the socket if the session merely *looks* alive.
     *
     * Main-confined, like the standby transitions, so it serializes with
     * [startClient] / [restartClient] instead of racing them for `client`.
     */
    private fun ensureConnected(reason: String) {
        scope.launch(Dispatchers.Main) {
            val existing = client
            if (existing == null) {
                Log.i(TAG, "no client at $reason; starting one")
                startClient()
                return@launch
            }
            Log.i(TAG, "requesting reconnect: $reason")
            existing.reconnectNow()
        }
    }

    /**
     * While the app is foregrounded and still not connected, keep asking. The
     * core's own backoff climbs to 30s, which is a long time to stare at a
     * "reconnecting" pill; nudging it puts the retry period back under this
     * interval. Backs off as attempts fail so a genuinely dead network doesn't
     * turn a long foreground session into a dial loop, and stops entirely the
     * moment the app is backgrounded.
     */
    private fun startForegroundReconnectWatchdog() {
        foregroundReconnectJob?.cancel()
        foregroundReconnectJob = scope.launch(Dispatchers.Main) {
            var interval = FOREGROUND_RECONNECT_MIN_MS
            while (isActive && hostAppForeground) {
                delay(interval)
                val action = foregroundReconnectAction(
                    hostAppForeground = hostAppForeground,
                    reconnectIdleMode = reconnectIdleMode,
                    hasClient = client != null,
                    isConnected = _stateFlow.value == UiConnState.Connected,
                )
                when (action) {
                    ForegroundReconnectAction.NONE -> {}
                    ForegroundReconnectAction.START_CLIENT -> {
                        Log.i(TAG, "foreground with no client; starting")
                        startClient()
                    }
                    ForegroundReconnectAction.RECONNECT_NOW -> {
                        Log.i(TAG, "foreground but ${_stateFlow.value}; retrying connect")
                        client?.reconnectNow()
                    }
                }
                interval = nextForegroundReconnectDelayMs(
                    currentMs = interval,
                    retried = action != ForegroundReconnectAction.NONE,
                    minMs = FOREGROUND_RECONNECT_MIN_MS,
                    maxMs = FOREGROUND_RECONNECT_MAX_MS,
                )
            }
        }
    }

    /**
     * Nudge the relay session after a transport change. A default-network
     * switch (Wi-Fi ⇄ cellular) leaves the old socket open but dead, so
     * without this it takes the core's 60s idle timeout plus a backoff to
     * notice. Skipped in standby — waking the radio for a screen-off network
     * blip is exactly the traffic the standby work was meant to remove.
     */
    private fun requestRelayReconnect(reason: String) {
        if (reconnectIdleMode) return
        // Main-confined for the same reason as refreshLanNetworkAvailability:
        // ConnectivityManager callbacks arrive on their own thread, and every
        // write to `client` happens on main.
        scope.launch(Dispatchers.Main) {
            client?.reconnectNow()
            Log.i(TAG, "relay reconnect requested: $reason")
        }
    }

    private fun startLanCountPoller() {
        lanCountJob?.cancel()
        lanCountJob = scope.launch {
            while (isActive) {
                if (lanActive) {
                    try {
                        val records = client?.lanPeerRecords().orEmpty()
                            .map {
                                LanFilePeer(
                                    deviceId = it.deviceId,
                                    displayName = it.displayName,
                                    candidateCount = it.candidateCount.toInt(),
                                )
                            }
                            .sortedWith(compareBy<LanFilePeer> { it.displayName }.thenBy { it.deviceId })
                        _lanFilePeers.value = records

                        val names = client?.lanPeers().orEmpty().sorted()
                        val displayNames = names.ifEmpty { records.map { it.displayName } }
                        _lanPeerNames.value = displayNames
                        _lanPeerCount.value = displayNames.size

                        val received = client?.takeReceivedFiles().orEmpty()
                        if (received.isNotEmpty()) {
                            received.forEach { file ->
                                appendFileTransferHistory(
                                    FileTransferHistoryEntry(
                                        id = file.transferId,
                                        fileName = file.fileName,
                                        deviceName = "LAN 设备",
                                        sizeBytes = file.sizeBytes,
                                        direction = FileTransferDirection.RECEIVED,
                                        status = FileTransferStatus.RECEIVED,
                                        path = file.path,
                                    )
                                )
                            }
                        }
                    } catch (t: Throwable) {
                        Log.w(TAG, "LAN/file poll failed: ${t.message}")
                    }
                    delay(LAN_COUNT_ACTIVE_INTERVAL_MS)
                } else {
                    _lanFilePeers.value = emptyList()
                    _lanPeerNames.value = emptyList()
                    _lanPeerCount.value = 0
                    delay(LAN_COUNT_IDLE_INTERVAL_MS)
                }
            }
        }
    }

    private fun triggerShizukuClipboardRead(
        reason: String,
        fallbackText: String? = null,
    ) {
        if (shizukuReadJob?.isActive == true) return
        shizukuReadJob = scope.launch {
            val state = ShizukuBridge.state()
            if (state != ShizukuBridge.State.READY) {
                Log.d(TAG, "skip Shizuku clipboard read ($reason): state=$state")
                publishClipboardFallback(reason, fallbackText)
                return@launch
            }
            when (val clip = ShizukuBridge.readPrimaryClip()) {
                is ShizukuBridge.Clip.Text -> {
                    val text = clip.value
                    if (text.isNotEmpty()) {
                        withContext(Dispatchers.Main) {
                            Log.i(TAG, "Shizuku read text after $reason: ${text.length} chars")
                            publish(text)
                        }
                    }
                }
                is ShizukuBridge.Clip.ImageUri -> {
                    val key = clip.uri.toString()
                    if (key != lastPolledImageUri) {
                        lastPolledImageUri = key
                        Log.i(TAG, "Shizuku read image uri after $reason: $key")
                        val outbound = ImagePipeline.outboundFromUri(
                            this@ClipBridgeAccessibilityService,
                            clip.uri,
                        )
                        if (outbound != null) {
                            withContext(Dispatchers.Main) { publishImage(outbound) }
                        }
                    }
                }
                null -> publishClipboardFallback(reason, fallbackText)
            }
        }
    }

    private suspend fun publishClipboardFallback(reason: String, text: String?) {
        if (text.isNullOrEmpty()) {
            Log.w(TAG, "clipboard unavailable after $reason; no recent text selection")
            return
        }
        withContext(Dispatchers.Main) {
            Log.i(TAG, "publishing recent selection after $reason: ${text.length} chars")
            publish(text)
        }
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        val e = event ?: return
        if (shouldRememberAccessibilitySelection(e.eventType)) {
            rememberSelection(e)
        } else if (e.eventType == AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED) {
            maybeHandleCopyToast(e)
        }
    }

    override fun onInterrupt() {}

    override fun onUnbind(intent: Intent?): Boolean {
        Log.i(TAG, "accessibility service unbinding")
        if (instanceRef === this) instanceRef = null
        shizukuReadJob?.cancel()
        shizukuReadJob = null
        lanActiveJob?.cancel()
        lanActiveJob = null
        leaveStandbyDebounceJob?.cancel()
        leaveStandbyDebounceJob = null
        foregroundReconnectJob?.cancel()
        foregroundReconnectJob = null
        hostAppForeground = false
        lanCountJob?.cancel()
        lanCountJob = null
        unregisterNetworkStateCallback()
        shizukuStateListener?.let(ShizukuBridge::removeStateListener)
        shizukuStateListener = null
        ShizukuBridge.stopClipboardMonitor()
        scope.cancel()
        ShizukuBridge.unregister()
        clipListener?.let { clipboard?.removePrimaryClipChangedListener(it) }
        clipListener = null
        prefsListener?.let {
            getSharedPreferences(PairingStore.PREFS, Context.MODE_PRIVATE)
                .unregisterOnSharedPreferenceChangeListener(it)
        }
        prefsListener = null
        unregisterScreenStateReceiver()
        client?.stop()
        client = null
        releaseMulticastLock()
        _stateFlow.value = UiConnState.Idle
        _lanFilePeers.value = emptyList()
        _fileReceiveDir.value = ""
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

    private fun recentSelectionForClipboardFallback(): String? {
        val selection = lastSelection ?: return null
        val ageMillis = System.currentTimeMillis() - lastSelectionAt
        if (ageMillis > SELECTION_FRESH_MS) {
            Log.w(TAG, "selection too old (${ageMillis}ms), ignoring")
            return null
        }
        return selection
    }

    private fun maybeHandleCopyToast(event: AccessibilityEvent) {
        val text = event.text?.joinToString(" ") ?: return
        if (!looksLikeCopyToast(text)) return

        if (ShizukuBridge.state() == ShizukuBridge.State.READY) {
            triggerShizukuClipboardRead(
                reason = "copy toast",
                fallbackText = recentSelectionForClipboardFallback(),
            )
            return
        }

        val sel = recentSelectionForClipboardFallback()
        if (sel.isNullOrEmpty()) {
            Log.w(TAG, "copy toast '$text' but no recent selection cached")
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
        val cd = try {
            cb.primaryClip
        } catch (t: Throwable) {
            Log.w(TAG, "direct clipboard read failed; falling back to Shizuku", t)
            null
        }
        if (cd == null) {
            // Android 10+ returns null for background apps even though the
            // listener itself still fires. Samsung's clipboard overlay does
            // not reliably emit an accessibility copy-toast event, so waiting
            // for that separate fallback silently loses Android-origin clips.
            if (ShizukuBridge.isClipboardMonitorActive()) {
                // Samsung's SemClipData event carries the text directly.
                // Re-reading its permission-bearing framework ClipData can
                // fail URI grants and make ClipboardService clear the clip.
                Log.d(TAG, "Samsung clipboard monitor active; skip duplicate standard read")
                return
            }
            triggerShizukuClipboardRead(
                reason = "clipboard listener",
                fallbackText = recentSelectionForClipboardFallback(),
            )
            return
        }
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
        // sources (Shizuku read, toast) firing for the same change all skip too.
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
        // A fresh local copy outranks any image fetch deferred while locked.
        pendingRemoteImage = null
        val payload = ClipPayload(
            kind = ClipKind.TEXT,
            content = text,
            deviceName = android.os.Build.MODEL ?: "Android",
            ts = System.currentTimeMillis().toULong(),
            image = null,
        )
        try {
            activateLanTemporarily("local text")
            client?.sendClip(payload)
            Log.i(TAG, "published clip (${text.length} chars)")
        } catch (t: Throwable) {
            Log.e(TAG, "sendClip failed", t)
        }
    }

    /**
     * Image counterpart to `publish(text)`. Two callers:
     *
     *   - Clipboard listener / Shizuku read (clipboard activity) — passes
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

        // A fresh local copy outranks any image fetch deferred while locked.
        pendingRemoteImage = null
        activateLanTemporarily("local image")
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
        client?.let { configureFileReceiving() }
        // Apply current standby flags to the fresh client. Core overloads
        // these to mean hard WS suspend + real LAN tear-down / resume.
        client?.setReconnectIdleMode(reconnectIdleMode)
        client?.setLanActive(lanActive)
        if (lanActive) {
            acquireMulticastLock()
        } else {
            releaseMulticastLock()
            _lanFilePeers.value = emptyList()
            _lanPeerNames.value = emptyList()
            _lanPeerCount.value = 0
        }
    }

    private fun restartClient() {
        client?.stop()
        client = null
        _lanFilePeers.value = emptyList()
        _fileReceiveDir.value = ""
        releaseMulticastLock()
        startClient()
    }

    private fun configureFileReceiving() {
        val base = getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS) ?: filesDir
        val dir = File(base, "ClipBridge")
        if (!dir.isDirectory && !dir.mkdirs()) {
            Log.w(TAG, "failed to create file receive dir: ${dir.absolutePath}")
            return
        }
        _fileReceiveDir.value = dir.absolutePath
        try {
            client?.setFileReceiveDir(dir.absolutePath)
            Log.i(TAG, "file receive dir: ${dir.absolutePath}")
        } catch (t: Throwable) {
            Log.w(TAG, "setFileReceiveDir failed: ${t.message}")
        }
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

    fun sendFilesToPeers(uris: List<Uri>, targetDeviceIds: List<String>) {
        val targets = targetDeviceIds.distinct().filter { it.isNotBlank() }
        if (uris.isEmpty() || targets.isEmpty()) return

        activateLanTemporarily("file send")
        scope.launch(Dispatchers.IO) {
            val peerNames = _lanFilePeers.value.associate { it.deviceId to it.displayName }
            for (uri in uris) {
                val cached = try {
                    copyUriToTransferCache(uri)
                } catch (t: Throwable) {
                    Log.w(TAG, "failed to stage file for send: ${t.message}")
                    null
                }
                if (cached == null) {
                    appendFileTransferHistory(
                        FileTransferHistoryEntry(
                            id = UUID.randomUUID().toString(),
                            fileName = sanitizeAndroidFileTransferName(queryDisplayName(uri)),
                            deviceName = "LAN 设备",
                            sizeBytes = 0UL,
                            direction = FileTransferDirection.SENT,
                            status = FileTransferStatus.FAILED,
                            message = "无法读取文件",
                        )
                    )
                    continue
                }

                try {
                    for (target in targets) {
                        sendCachedFileToTarget(
                            cached = cached,
                            targetDeviceId = target,
                            targetName = peerNames[target] ?: target.take(12),
                        )
                    }
                } finally {
                    cached.file.delete()
                    cached.file.parentFile?.delete()
                }
            }
        }
    }

    private data class CachedTransferFile(
        val file: File,
        val displayName: String,
        val mimeType: String?,
        val sizeBytes: ULong,
    )

    private fun copyUriToTransferCache(uri: Uri): CachedTransferFile? {
        val displayName = sanitizeAndroidFileTransferName(queryDisplayName(uri))
        val dir = File(File(cacheDir, "clipbridge_file_sends"), UUID.randomUUID().toString())
        if (!dir.isDirectory && !dir.mkdirs()) return null
        val outFile = File(dir, displayName)
        val input = contentResolver.openInputStream(uri) ?: return null
        input.use { source ->
            outFile.outputStream().use { target ->
                source.copyTo(target, bufferSize = 256 * 1024)
            }
        }
        return CachedTransferFile(
            file = outFile,
            displayName = displayName,
            mimeType = contentResolver.getType(uri),
            sizeBytes = outFile.length().coerceAtLeast(0L).toULong(),
        )
    }

    private fun queryDisplayName(uri: Uri): String {
        val projection = arrayOf(OpenableColumns.DISPLAY_NAME)
        val fromProvider = runCatching {
            contentResolver.query(uri, projection, null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    if (idx >= 0) cursor.getString(idx) else null
                } else {
                    null
                }
            }
        }.getOrNull()
        return fromProvider
            ?: uri.lastPathSegment?.substringAfterLast('/')?.substringAfterLast('\\')
            ?: "file"
    }

    private fun sendCachedFileToTarget(
        cached: CachedTransferFile,
        targetDeviceId: String,
        targetName: String,
    ) {
        val localId = UUID.randomUUID().toString()
        appendFileTransferHistory(
            FileTransferHistoryEntry(
                id = localId,
                fileName = cached.displayName,
                deviceName = targetName,
                sizeBytes = cached.sizeBytes,
                direction = FileTransferDirection.SENT,
                status = FileTransferStatus.SENDING,
            )
        )

        try {
            val sent = client?.sendFileToPeer(
                targetDeviceId = targetDeviceId,
                sourcePath = cached.file.absolutePath,
                mimeType = cached.mimeType,
            ) ?: error("客户端未启动")
            upsertFileTransferHistory(
                FileTransferHistoryEntry(
                    id = localId,
                    fileName = sent.fileName,
                    deviceName = targetName,
                    sizeBytes = sent.bytesSent,
                    direction = FileTransferDirection.SENT,
                    status = FileTransferStatus.SENT,
                )
            )
        } catch (t: Throwable) {
            Log.w(TAG, "sendFileToPeer failed: ${t.message}")
            upsertFileTransferHistory(
                FileTransferHistoryEntry(
                    id = localId,
                    fileName = cached.displayName,
                    deviceName = targetName,
                    sizeBytes = cached.sizeBytes,
                    direction = FileTransferDirection.SENT,
                    status = FileTransferStatus.FAILED,
                    message = t.message ?: "发送失败",
                )
            )
        }
    }

    private fun handleRemoteClip(payload: ClipPayload) {
        // Deliberately no activateLanTemporarily here: the clip already
        // reached us (relay WS, or an already-open LAN path), so there is
        // nothing LAN could speed up — image fetches fall back to the relay
        // when no LAN peer is connected. Waking LAN on every remote receive
        // let a chatty peer keep multicast + pings running through the whole
        // screen-off period.
        when (payload.kind) {
            ClipKind.TEXT -> {
                Log.i(TAG, "remote text clip (${payload.content.length} chars)")
                // Newer text supersedes any image fetch deferred while the
                // screen was off — flushing it later would clobber this clip.
                pendingRemoteImage = null
                // Mark as expected echo *before* writing so the resulting
                // OnPrimaryClipChangedListener / Shizuku tick recognises it.
                expectedEcho = payload.content
                expectedEchoAt = System.currentTimeMillis()
                clipboard?.setPrimaryClip(
                    ClipData.newPlainText("ClipBridge", payload.content)
                )
                triggerShizukuClipboardRead("remote text")
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
        if (reconnectIdleMode) {
            // Screen is off: nobody can paste right now and the clipboard
            // only holds the latest item, so defer the (up to 32MB) blob
            // download until wake. Keeping just the newest meta means N
            // images copied while locked cost one download instead of N;
            // superseded ones never hit the network (they also skip the
            // image history — acceptable for a locked device).
            pendingRemoteImage = payload
            Log.i(TAG, "screen off, deferring remote image fetch (${meta.sizeBytes}B)")
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
                else {
                    Log.i(TAG, "wrote remote image to clipboard")
                    triggerShizukuClipboardRead("remote image")
                }
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
        // See `expectedEcho` doc above the field.
        private const val ECHO_WINDOW_MS = 10_000L
        // Short enough that an intentional re-copy of the same text still goes.
        private const val SOURCE_DEDUPE_MS = 3_000L
        private const val SELECTION_FRESH_MS = 30_000L
        private const val LAN_ACTIVE_WINDOW_MS = 60_000L
        /** Debounce rapid SCREEN_ON / USER_PRESENT so lock thrash cannot storm reconnects. */
        private const val LEAVE_STANDBY_DEBOUNCE_MS = 400L
        private const val LAN_COUNT_ACTIVE_INTERVAL_MS = 2_000L
        private const val LAN_COUNT_IDLE_INTERVAL_MS = 30_000L
        /** Foreground reconnect watchdog: first retry gap, and its ceiling. */
        private const val FOREGROUND_RECONNECT_MIN_MS = 5_000L
        private const val FOREGROUND_RECONNECT_MAX_MS = 30_000L

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

        private val _lanFilePeers = MutableStateFlow<List<LanFilePeer>>(emptyList())
        val lanFilePeers: StateFlow<List<LanFilePeer>> = _lanFilePeers.asStateFlow()

        private val _fileReceiveDir = MutableStateFlow("")
        val fileReceiveDir: StateFlow<String> = _fileReceiveDir.asStateFlow()

        // Image traffic history surfaced to the UI's image transfer card.
        // Newest first, capped at HISTORY_LIMIT. Sent and received both
        // appear here so the user can save / re-share their own outbound
        // images too (no separate "sent" tab on Android — keeps the
        // mobile UI compact).
        private const val HISTORY_LIMIT = 12
        private val _imageHistory = MutableStateFlow<List<ImageHistoryEntry>>(emptyList())
        val imageHistory: StateFlow<List<ImageHistoryEntry>> = _imageHistory.asStateFlow()

        private val _fileTransferHistory =
            MutableStateFlow<List<FileTransferHistoryEntry>>(emptyList())
        val fileTransferHistory: StateFlow<List<FileTransferHistoryEntry>> =
            _fileTransferHistory.asStateFlow()

        internal fun appendImageHistory(entry: ImageHistoryEntry) {
            val combined = (listOf(entry) + _imageHistory.value)
                .distinctBy { it.id }
                .take(HISTORY_LIMIT)
            _imageHistory.value = combined
        }

        internal fun clearImageHistory() {
            _imageHistory.value = emptyList()
        }

        internal fun appendFileTransferHistory(entry: FileTransferHistoryEntry) {
            val combined = (listOf(entry) + _fileTransferHistory.value)
                .distinctBy { it.id }
                .take(HISTORY_LIMIT)
            _fileTransferHistory.value = combined
        }

        internal fun upsertFileTransferHistory(entry: FileTransferHistoryEntry) {
            val current = _fileTransferHistory.value
            val replaced = current.map { if (it.id == entry.id) entry else it }
            _fileTransferHistory.value = if (current.any { it.id == entry.id }) {
                replaced
            } else {
                (listOf(entry) + current).take(HISTORY_LIMIT)
            }
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

private fun NetworkCapabilities.isLanCapable(): Boolean =
    hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ||
        hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
