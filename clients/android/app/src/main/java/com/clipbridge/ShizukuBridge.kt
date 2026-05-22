package com.clipbridge

import android.Manifest
import android.content.ClipData
import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager
import android.net.Uri
import android.os.IBinder
import android.os.Process
import android.provider.Settings
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
    fun readPrimaryClip(): Clip? {
        if (state() != State.READY) return null
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
    fun readPrimaryClipText(): String? =
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
