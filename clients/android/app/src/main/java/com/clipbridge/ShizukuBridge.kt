package com.clipbridge

import android.Manifest
import android.content.ClipData
import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.net.Uri
import android.os.IBinder
import android.os.Process
import android.provider.Settings
import android.util.Log
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeoutOrNull
import rikka.shizuku.Shizuku
import rikka.shizuku.ShizukuBinderWrapper
import rikka.shizuku.SystemServiceHelper

/**
 * Wraps Shizuku's privileged binder so we can read the system clipboard
 * even when ClipBridge is in the background. Without Shizuku the Android 10+
 * `ClipboardService` rejects our reads ("application is not in focus nor is
 * it a system service").
 *
 * Direct clipboard reads require Shizuku's server to run as shell uid (2000),
 * because `ClipboardService` validates `pkg = "com.android.shell"` against
 * the Binder caller. Root-mode Shizuku therefore uses a UserService that
 * drops to shell before ordinary reads. A Samsung event monitor is bound only
 * while the screen is interactive and can briefly use Android's system
 * identity when One UI sends a null clipboard-event payload.
 */
object ShizukuBridge {
    private const val TAG = "ShizukuBridge"
    private const val APPLICATION_ID = "com.clipbridge"
    private const val SHELL_PKG = "com.android.shell"
    private const val SHIZUKU_ROOT_UID = 0
    private const val SHIZUKU_SHELL_UID = 2000
    private const val USER_SERVICE_VERSION = 6
    private const val USER_SERVICE_TIMEOUT_MS = 5_000L
    private const val PER_USER_RANGE = 100000

    enum class State { UNAVAILABLE, NOT_AUTHORIZED, READY }

    /** Listener fires whenever Shizuku binder or permission state changes. */
    fun interface StateListener {
        fun onChange(state: State)
    }

    private val listeners = mutableSetOf<StateListener>()

    private val binderReceived = Shizuku.OnBinderReceivedListener { notifyState() }
    private val binderDead = Shizuku.OnBinderDeadListener { notifyState() }
    private val permissionResult = Shizuku.OnRequestPermissionResultListener { _, _ -> notifyState() }
    private var registered = false
    private val monitorLock = Any()
    private var monitorArgs: Shizuku.UserServiceArgs? = null
    private var monitorConnection: ServiceConnection? = null
    private var monitorService: IClipboardUserService? = null
    private var monitorTextHandler: ((String) -> Unit)? = null
    private val monitorTextAssembler = ClipboardTextAssembler()

    private val monitorTextListener = object : IClipboardTextListener.Stub() {
        override fun onClipboardTextChunk(
            transferId: Long,
            chunkIndex: Int,
            chunkCount: Int,
            textChunk: String,
        ) {
            val delivery = synchronized(monitorLock) {
                val text = monitorTextAssembler.append(
                    transferId = transferId,
                    chunkIndex = chunkIndex,
                    chunkCount = chunkCount,
                    textChunk = textChunk,
                )
                val handler = monitorTextHandler
                if (text != null && handler != null) text to handler else null
            }
            delivery?.second?.invoke(delivery.first)
        }
    }

    fun register() {
        if (registered) return
        registered = true
        Shizuku.addBinderReceivedListenerSticky(binderReceived)
        Shizuku.addBinderDeadListener(binderDead)
        Shizuku.addRequestPermissionResultListener(permissionResult)
    }

    fun unregister() {
        if (!registered) return
        registered = false
        Shizuku.removeBinderReceivedListener(binderReceived)
        Shizuku.removeBinderDeadListener(binderDead)
        Shizuku.removeRequestPermissionResultListener(permissionResult)
    }

    fun addStateListener(l: StateListener) {
        listeners.add(l)
        l.onChange(state())
    }

    fun removeStateListener(l: StateListener) {
        listeners.remove(l)
    }

    private fun notifyState() {
        val s = state()
        listeners.forEach { it.onChange(s) }
    }

    fun state(): State = try {
        if (!Shizuku.pingBinder()) State.UNAVAILABLE
        else if (Shizuku.isPreV11()) State.UNAVAILABLE  // ancient Shizuku, not supported
        else if (Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED) State.READY
        else State.NOT_AUTHORIZED
    } catch (_: Throwable) {
        State.UNAVAILABLE
    }

    fun requestPermission(requestCode: Int) {
        if (!Shizuku.pingBinder()) return
        if (Shizuku.shouldShowRequestPermissionRationale()) {
            Log.w(TAG, "user previously denied; showing system dialog again")
        }
        Shizuku.requestPermission(requestCode)
    }

    /** Return type of `readPrimaryClip` — exactly one of text or imageUri. */
    sealed class Clip {
        data class Text(val value: String) : Clip()
        data class ImageUri(val uri: Uri) : Clip()
    }

    /**
     * Read the current primary clipboard. Distinguishes text from image URIs
     * so the caller can route to the right pipeline. Returns null when
     * Shizuku isn't authorized, the clipboard is empty, or the reflected
     * call fails.
     */
    suspend fun readPrimaryClip(): Clip? {
        if (state() != State.READY) return null
        val shizukuUid = runCatching { Shizuku.getUid() }.getOrNull()
        if (shizukuUid == SHIZUKU_ROOT_UID) {
            return readPrimaryClipThroughShellUserService()
        }
        if (shizukuUid != SHIZUKU_SHELL_UID) {
            Log.w(TAG, "unsupported Shizuku server uid=$shizukuUid")
            return null
        }
        return runCatching {
            val rawBinder = SystemServiceHelper.getSystemService("clipboard")
                ?: return@runCatching null
            val proxy: IBinder = ShizukuBinderWrapper(rawBinder)
            val clipboard: Any = asInterface("android.content.IClipboard\$Stub", proxy)
                ?: return@runCatching null
            val clip = invokeGetPrimaryClip(clipboard) ?: return@runCatching null
            extractClip(clip)
        }.onFailure { Log.w(TAG, "readPrimaryClip failed", it) }.getOrNull()
    }

    private suspend fun readPrimaryClipThroughShellUserService(): Clip? =
        withTimeoutOrNull(USER_SERVICE_TIMEOUT_MS) {
            suspendCancellableCoroutine { continuation ->
                val completed = AtomicBoolean(false)
                val args = clipboardUserServiceArgs(
                    tag = "clipboard-reader",
                    processSuffix = "clipboard-read",
                )
                lateinit var connection: ServiceConnection

                fun unbind() {
                    runCatching {
                        Shizuku.unbindUserService(args, connection, true)
                    }.onFailure {
                        Log.w(TAG, "failed to remove clipboard UserService", it)
                    }
                }

                fun complete(result: Clip?) {
                    if (!completed.compareAndSet(false, true)) return
                    unbind()
                    if (continuation.isActive) continuation.resume(result)
                }

                connection = object : ServiceConnection {
                    override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                        val result = runCatching {
                            val service = IClipboardUserService.Stub.asInterface(binder)
                            val assembler = ClipboardTextAssembler()
                            val received = AtomicReference<Clip?>()
                            val listener = object : IClipboardTextListener.Stub() {
                                override fun onClipboardTextChunk(
                                    transferId: Long,
                                    chunkIndex: Int,
                                    chunkCount: Int,
                                    textChunk: String,
                                ) {
                                    assembler.append(
                                        transferId = transferId,
                                        chunkIndex = chunkIndex,
                                        chunkCount = chunkCount,
                                        textChunk = textChunk,
                                    )?.let { received.set(Clip.Text(it)) }
                                }
                            }
                            if (service.readPrimaryClipText(listener)) {
                                received.get()
                            } else {
                                null
                            }
                        }.onFailure {
                            Log.w(TAG, "root clipboard UserService read failed", it)
                        }.getOrNull()
                        complete(result)
                    }

                    override fun onServiceDisconnected(name: ComponentName) {
                        complete(null)
                    }
                }

                continuation.invokeOnCancellation {
                    if (completed.compareAndSet(false, true)) unbind()
                }

                runCatching {
                    Shizuku.bindUserService(args, connection)
                }.onFailure {
                    Log.w(TAG, "failed to bind clipboard UserService", it)
                    complete(null)
                }
            }
        }

    fun startClipboardMonitor(onText: (String) -> Unit) {
        if (state() != State.READY) return
        val shizukuUid = runCatching { Shizuku.getUid() }.getOrNull()
        if (shizukuUid != SHIZUKU_ROOT_UID) return
        val args = clipboardUserServiceArgs(
            tag = "clipboard-monitor",
            processSuffix = "clipboard-monitor",
        )
        lateinit var connection: ServiceConnection
        connection = object : ServiceConnection {
            override fun onServiceConnected(name: ComponentName, binder: IBinder) {
                val service = IClipboardUserService.Stub.asInterface(binder)
                val active = synchronized(monitorLock) {
                    if (monitorConnection === this) {
                        monitorService = service
                        monitorTextAssembler.clear()
                        true
                    } else {
                        false
                    }
                }
                if (!active) {
                    removeClipboardMonitor(args, this, service)
                    return
                }
                val started = runCatching {
                    service.startClipboardMonitor(monitorTextListener)
                }.onFailure {
                    Log.w(TAG, "failed to start Samsung clipboard monitor", it)
                }.getOrDefault(false)
                if (!started) {
                    synchronized(monitorLock) {
                        if (monitorConnection === this) {
                            monitorArgs = null
                            monitorConnection = null
                            monitorService = null
                            monitorTextAssembler.clear()
                        }
                    }
                    removeClipboardMonitor(args, this, service)
                    return
                }
            }

            override fun onServiceDisconnected(name: ComponentName) {
                synchronized(monitorLock) {
                    if (monitorConnection === this) {
                        monitorArgs = null
                        monitorConnection = null
                        monitorService = null
                        monitorTextAssembler.clear()
                    }
                }
            }
        }

        val shouldBind = synchronized(monitorLock) {
            monitorTextHandler = onText
            monitorTextAssembler.clear()
            if (monitorConnection != null) {
                false
            } else {
                monitorArgs = args
                monitorConnection = connection
                true
            }
        }
        if (!shouldBind) return

        runCatching {
            Shizuku.bindUserService(args, connection)
        }.onFailure {
            Log.w(TAG, "failed to bind Samsung clipboard monitor", it)
            synchronized(monitorLock) {
                if (monitorConnection === connection) {
                    monitorArgs = null
                    monitorConnection = null
                    monitorService = null
                    monitorTextAssembler.clear()
                }
            }
        }
    }

    fun isClipboardMonitorActive(): Boolean =
        synchronized(monitorLock) { monitorService != null }

    fun stopClipboardMonitor() {
        val state = synchronized(monitorLock) {
            val args = monitorArgs
            val connection = monitorConnection
            val service = monitorService
            monitorArgs = null
            monitorConnection = null
            monitorService = null
            monitorTextHandler = null
            monitorTextAssembler.clear()
            if (args != null && connection != null) {
                Triple(args, connection, service)
            } else {
                null
            }
        } ?: return
        removeClipboardMonitor(state.first, state.second, state.third)
    }

    private fun removeClipboardMonitor(
        args: Shizuku.UserServiceArgs,
        connection: ServiceConnection,
        service: IClipboardUserService?,
    ) {
        runCatching { service?.stopClipboardMonitor() }
            .onFailure { Log.w(TAG, "failed to stop Samsung clipboard monitor", it) }
        runCatching { Shizuku.unbindUserService(args, connection, true) }
            .onFailure { Log.w(TAG, "failed to remove Samsung clipboard monitor", it) }
    }

    private fun clipboardUserServiceArgs(
        tag: String,
        processSuffix: String,
    ): Shizuku.UserServiceArgs =
        Shizuku.UserServiceArgs(
            ComponentName(APPLICATION_ID, ClipboardUserService::class.java.name),
        )
            .daemon(false)
            .processNameSuffix(processSuffix)
            .debuggable(false)
            .tag(tag)
            .version(USER_SERVICE_VERSION)

    fun enableAccessibilityService(context: Context, serviceClass: Class<*>): Boolean {
        if (state() != State.READY) return false
        val appContext = context.applicationContext
        val serviceName = ComponentName(appContext, serviceClass).flattenToString()
        val resolver = appContext.contentResolver
        val current = Settings.Secure.getString(
            resolver,
            Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
        )
        if (isAccessibilityServiceEnabledInSetting(current, serviceName)) return true
        if (!ensureWriteSecureSettingsPermission(appContext)) return false

        val updated = enabledAccessibilityServicesWith(current, serviceName)
        if (updated.isEmpty()) return false

        return runCatching {
            val wroteServices = Settings.Secure.putString(
                resolver,
                Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
                updated,
            )
            val wroteEnabled = Settings.Secure.putString(
                resolver,
                Settings.Secure.ACCESSIBILITY_ENABLED,
                "1",
            )
            val after = Settings.Secure.getString(
                resolver,
                Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
            )
            wroteServices && wroteEnabled &&
                isAccessibilityServiceEnabledInSetting(after, serviceName)
        }.onFailure {
            Log.w(TAG, "enableAccessibilityService failed", it)
        }.getOrDefault(false)
    }

    /** Convenience for the text-only path that pre-existed the image work. */
    suspend fun readPrimaryClipText(): String? =
        (readPrimaryClip() as? Clip.Text)?.value

    private fun ensureWriteSecureSettingsPermission(context: Context): Boolean {
        if (context.checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) ==
            PackageManager.PERMISSION_GRANTED
        ) {
            return true
        }
        if (state() != State.READY) return false

        return runCatching {
            val rawBinder = SystemServiceHelper.getSystemService("package")
                ?: return@runCatching false
            val proxy: IBinder = ShizukuBinderWrapper(rawBinder)
            val packageManager = asInterface("android.content.pm.IPackageManager\$Stub", proxy)
                ?: return@runCatching false
            val grantRuntimePermission = packageManager.javaClass.getMethod(
                "grantRuntimePermission",
                String::class.java,
                String::class.java,
                Int::class.javaPrimitiveType,
            )
            grantRuntimePermission.invoke(
                packageManager,
                context.packageName,
                Manifest.permission.WRITE_SECURE_SETTINGS,
                Process.myUid() / PER_USER_RANGE,
            )
            context.checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) ==
                PackageManager.PERMISSION_GRANTED
        }.onFailure {
            Log.w(TAG, "grant WRITE_SECURE_SETTINGS via Shizuku failed", it)
        }.getOrDefault(false)
    }

    private fun asInterface(stubClassName: String, binder: IBinder): Any? {
        val stubClass = Class.forName(stubClassName)
        val asInterface = stubClass.getMethod("asInterface", IBinder::class.java)
        return asInterface.invoke(null, binder)
    }

    /**
     * `IClipboard.getPrimaryClip` signature changed multiple times. We try
     * the modern shape first and fall back to older ones.
     *
     *   API 34+: getPrimaryClip(String pkg, String attrTag, int userId, int deviceId)
     *   API 33  : getPrimaryClip(String pkg, String attrTag, int userId)
     *   API ≤32 : getPrimaryClip(String pkg, int userId)
     */
    private fun invokeGetPrimaryClip(clipboard: Any): ClipData? {
        val cls = clipboard.javaClass
        val userId = 0
        val deviceId = 0

        // API 34+
        runCatching {
            val m = cls.getMethod(
                "getPrimaryClip",
                String::class.java,
                String::class.java,
                Int::class.javaPrimitiveType,
                Int::class.javaPrimitiveType,
            )
            return m.invoke(clipboard, SHELL_PKG, null, userId, deviceId) as? ClipData
        }
        // API 33
        runCatching {
            val m = cls.getMethod(
                "getPrimaryClip",
                String::class.java,
                String::class.java,
                Int::class.javaPrimitiveType,
            )
            return m.invoke(clipboard, SHELL_PKG, null, userId) as? ClipData
        }
        // API ≤32
        runCatching {
            val m = cls.getMethod(
                "getPrimaryClip",
                String::class.java,
                Int::class.javaPrimitiveType,
            )
            return m.invoke(clipboard, SHELL_PKG, userId) as? ClipData
        }
        return null
    }

    /**
     * Distinguish text vs image-URI without a Context. ClipDescription's
     * mime types are the canonical signal — anything starting with
     * `image/` indicates the Item carries a URI we can openInputStream
     * on (with proper permission handed off to the caller).
     */
    private fun extractClip(clip: ClipData): Clip? {
        if (clip.itemCount == 0) return null
        val item = clip.getItemAt(0) ?: return null
        val desc = clip.description
        // Image: any image-typed mime → return the URI.
        for (i in 0 until desc.mimeTypeCount) {
            if (desc.getMimeType(i).startsWith("image/")) {
                val uri = item.uri ?: continue
                return Clip.ImageUri(uri)
            }
        }
        // Text path. Deliberately NOT falling back to item.uri.toString()
        // anymore — that used to leak `content://media/...` URIs as plain
        // text to other devices when an app put an image-only ClipData
        // and we somehow missed the image branch. URIs are useless on
        // remote devices anyway; if we can't read it as image, drop it.
        val text = item.text?.toString() ?: item.htmlText?.toString()
        return text?.let { Clip.Text(it) }
    }
}
