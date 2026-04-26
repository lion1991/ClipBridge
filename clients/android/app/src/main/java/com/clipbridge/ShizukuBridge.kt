package com.clipbridge

import android.content.ClipData
import android.content.pm.PackageManager
import android.os.IBinder
import android.util.Log
import rikka.shizuku.Shizuku
import rikka.shizuku.ShizukuBinderWrapper
import rikka.shizuku.SystemServiceHelper

/**
 * Wraps Shizuku's privileged binder so we can read the system clipboard
 * even when ClipBridge is in the background. Without Shizuku the Android 10+
 * `ClipboardService` rejects our reads ("application is not in focus nor is
 * it a system service").
 *
 * Calls go through Shizuku → shell uid (2000) → IClipboard system service,
 * so we pass `pkg = "com.android.shell"` to satisfy the system's
 * package-vs-uid consistency check.
 */
object ShizukuBridge {
    private const val TAG = "ShizukuBridge"
    private const val SHELL_PKG = "com.android.shell"

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

    /**
     * Read the current primary clipboard's text. Returns null when Shizuku
     * isn't authorized, the clipboard is empty, or the reflected call fails.
     */
    fun readPrimaryClipText(): String? {
        if (state() != State.READY) return null
        return runCatching {
            val rawBinder = SystemServiceHelper.getSystemService("clipboard")
                ?: return@runCatching null
            val proxy: IBinder = ShizukuBinderWrapper(rawBinder)
            val clipboard: Any = asInterface(proxy) ?: return@runCatching null
            val clip = invokeGetPrimaryClip(clipboard) ?: return@runCatching null
            extractText(clip)
        }.onFailure { Log.w(TAG, "readPrimaryClipText failed", it) }.getOrNull()
    }

    private fun asInterface(binder: IBinder): Any? {
        val stubClass = Class.forName("android.content.IClipboard\$Stub")
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

    private fun extractText(clip: ClipData): String? {
        if (clip.itemCount == 0) return null
        val item = clip.getItemAt(0) ?: return null
        // No Context here — coerceToText needs one for HtmlConverter, so we
        // settle for plain text + URI fallback.
        return item.text?.toString()
            ?: item.htmlText
            ?: item.uri?.toString()
    }
}
