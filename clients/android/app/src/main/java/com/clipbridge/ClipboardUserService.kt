package com.clipbridge

import android.content.ClipData
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import android.os.Parcelable
import android.os.Process
import android.system.Os
import android.util.Log
import androidx.annotation.Keep
import java.lang.reflect.Proxy
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicLong
import rikka.shizuku.SystemServiceHelper
import kotlin.system.exitProcess

/**
 * Screen-bound or short-lived Shizuku UserService used only in root mode.
 *
 * ClipboardService associates com.android.shell with uid 2000, not root. The
 * process starts as root, completes Shizuku's service handshake, then drops to
 * shell permanently for a short-lived ordinary read. The Samsung monitor
 * instead switches only its effective uid: shell while registering the event
 * listener, and Android's system uid while handling a null-payload event. The
 * system identity owns READ_CLIPBOARD_IN_BACKGROUND, avoiding One UI's
 * transient focus loss during the copy toast. If Samsung has already cleared
 * the framework clipboard, a bounded event worker reads the row that
 * SemClipboardService just persisted in HoneyBoard. The real/saved root uid
 * lets the monitor restore its identity after every call. The monitor exists
 * only while the screen is interactive.
 */
class ClipboardUserService : IClipboardUserService.Stub {
    @Volatile private var textListener: IClipboardTextListener? = null
    @Volatile private var monitorActive = false
    private val identityLock = Any()
    private val samsungEventExecutor = Executors.newSingleThreadExecutor { runnable ->
        Thread(runnable, "clipbridge-samsung-clipboard").apply {
            isDaemon = true
        }
    }
    private val nextTextTransferId = AtomicLong()
    private var lastHoneyboardClipKey: HoneyboardClipKey? = null
    private var samsungClipboardService: Any? = null
    private var samsungListenerProxy: Any? = null
    private var samsungListenerBinder: Binder? = null

    constructor()

    @Keep
    constructor(@Suppress("UNUSED_PARAMETER") context: Context) : this()

    override fun readPrimaryClipText(listener: IClipboardTextListener): Boolean {
        ensureShellIdentity()
        val token = Binder.clearCallingIdentity()
        return try {
            val binder = SystemServiceHelper.getSystemService("clipboard") ?: return false
            val clipboard = clipboardInterface(binder) ?: return false
            val text = extractFrameworkText(
                invokeGetPrimaryClip(
                    clipboard = clipboard,
                    packageName = SHELL_PACKAGE,
                    userId = 0,
                ),
            )
            if (text.isNullOrEmpty()) false else sendClipboardText(listener, text)
        } catch (t: Throwable) {
            Log.w(TAG, "shell clipboard read failed", t)
            false
        } finally {
            Binder.restoreCallingIdentity(token)
        }
    }

    override fun startClipboardMonitor(listener: IClipboardTextListener): Boolean {
        check(Process.myUid() == ROOT_UID) {
            "clipboard monitor must retain root uid"
        }
        textListener = listener
        val token = Binder.clearCallingIdentity()
        return try {
            primeHoneyboardBaseline()
            val registered = withEffectiveUid(SHELL_UID) {
                registerSamsungClipboardListener()
            }
            monitorActive = registered
            registered
        } catch (t: Throwable) {
            Log.w(TAG, "Samsung clipboard listener unavailable", t)
            monitorActive = false
            textListener = null
            false
        } finally {
            Binder.restoreCallingIdentity(token)
        }
    }

    override fun stopClipboardMonitor() {
        monitorActive = false
        val token = Binder.clearCallingIdentity()
        try {
            if (Process.myUid() == ROOT_UID) {
                withEffectiveUid(SHELL_UID) {
                    unregisterSamsungClipboardListener()
                }
            } else {
                unregisterSamsungClipboardListener()
            }
        } catch (t: Throwable) {
            Log.w(TAG, "failed to remove Samsung clipboard listener", t)
        } finally {
            textListener = null
            Binder.restoreCallingIdentity(token)
        }
    }

    private fun registerSamsungClipboardListener(): Boolean {
        if (samsungListenerProxy != null) return true
        val rawBinder = SystemServiceHelper.getSystemService(SAMSUNG_CLIPBOARD_SERVICE)
            ?: return false
        val serviceStub = Class.forName("$SAMSUNG_SERVICE_INTERFACE\$Stub")
        val service = serviceStub
            .getMethod("asInterface", IBinder::class.java)
            .invoke(null, rawBinder)
            ?: return false
        val listenerInterface = Class.forName(SAMSUNG_LISTENER_INTERFACE)
        val callbackBinder = SamsungClipboardEventBinder()
        val listenerProxy = Proxy.newProxyInstance(
            listenerInterface.classLoader,
            arrayOf(listenerInterface),
        ) { proxy, method, args ->
            when (method.name) {
                "asBinder" -> callbackBinder
                "toString" -> "ClipBridgeSamsungClipboardListener"
                "hashCode" -> System.identityHashCode(proxy)
                "equals" -> proxy === args?.firstOrNull()
                else -> null
            }
        }
        service.javaClass.getMethod(
            "addClipboardEventListener",
            listenerInterface,
            String::class.java,
        ).invoke(service, listenerProxy, SHELL_PACKAGE)
        samsungClipboardService = service
        samsungListenerProxy = listenerProxy
        samsungListenerBinder = callbackBinder
        Log.i(TAG, "Samsung clipboard event listener registered")
        return true
    }

    private fun unregisterSamsungClipboardListener() {
        val service = samsungClipboardService
        val listener = samsungListenerProxy
        if (service != null && listener != null) {
            val listenerInterface = Class.forName(SAMSUNG_LISTENER_INTERFACE)
            service.javaClass.getMethod(
                "removeClipboardEventListener",
                listenerInterface,
            ).invoke(service, listener)
        }
        samsungClipboardService = null
        samsungListenerProxy = null
        samsungListenerBinder = null
    }

    private inner class SamsungClipboardEventBinder : Binder() {
        override fun onTransact(
            code: Int,
            data: Parcel,
            reply: Parcel?,
            flags: Int,
        ): Boolean {
            if (code == IBinder.INTERFACE_TRANSACTION) {
                reply?.writeString(SAMSUNG_LISTENER_INTERFACE)
                return true
            }
            if (code == samsungClipboardEventTransactionCode()) {
                data.enforceInterface(SAMSUNG_LISTENER_INTERFACE)
                val eventType = data.readInt()
                val clip = if (data.readInt() != 0) {
                    samsungClipCreator().createFromParcel(data)
                } else {
                    null
                }
                val eventReceivedAt = System.currentTimeMillis()
                // Android 16 / One UI can deliberately send null here. Try the
                // system identity immediately; if Samsung already cleared the
                // framework clipboard, read the HoneyBoard row asynchronously
                // so this Binder callback never blocks system_server.
                val text = extractSamsungText(clip)
                    ?: readPrimaryClipAsSystem()
                if (!text.isNullOrEmpty()) {
                    samsungEventExecutor.execute {
                        dispatchClipboardText(text)
                    }
                } else if (clip == null) {
                    samsungEventExecutor.execute {
                        val persistedText = readRecentHoneyboardText(eventReceivedAt)
                        if (!persistedText.isNullOrEmpty()) {
                            dispatchClipboardText(persistedText)
                        } else if (monitorActive) {
                            logUnreadableSamsungEvent(eventType, null)
                        }
                    }
                } else {
                    logUnreadableSamsungEvent(eventType, clip)
                }
                reply?.writeNoException()
                return true
            }
            if (code == samsungClipboardFilterTransactionCode()) {
                data.enforceInterface(SAMSUNG_LISTENER_INTERFACE)
                data.readInt()
                reply?.writeNoException()
                return true
            }
            return super.onTransact(code, data, reply, flags)
        }
    }

    private fun dispatchClipboardText(text: String) {
        if (!monitorActive) return
        Log.i(TAG, "Samsung clipboard event text (${text.length} chars)")
        val listener = textListener ?: return
        sendClipboardText(listener, text) {
            monitorActive && textListener === listener
        }
    }

    private fun sendClipboardText(
        listener: IClipboardTextListener,
        text: String,
        shouldContinue: () -> Boolean = { true },
    ): Boolean {
        val chunkCount = (text.length - 1) / AIDL_TEXT_CHUNK_CHARS + 1
        if (chunkCount > AIDL_TEXT_MAX_CHUNKS) {
            Log.w(TAG, "clipboard text exceeds AIDL chunk limit (${text.length} chars)")
            return false
        }
        val transferId = nextTextTransferId.incrementAndGet()
        return runCatching {
            repeat(chunkCount) { chunkIndex ->
                if (!shouldContinue()) return@runCatching false
                val start = chunkIndex * AIDL_TEXT_CHUNK_CHARS
                val end = minOf(text.length, start + AIDL_TEXT_CHUNK_CHARS)
                listener.onClipboardTextChunk(
                    transferId,
                    chunkIndex,
                    chunkCount,
                    text.substring(start, end),
                )
            }
            true
        }
            .onFailure { Log.w(TAG, "clipboard text callback failed", it) }
            .getOrDefault(false)
    }

    private fun logUnreadableSamsungEvent(eventType: Int, clip: Any?) {
        Log.w(
            TAG,
            "Samsung clipboard event had no readable text " +
                "(event=$eventType, payload=${clip?.javaClass?.simpleName ?: "null"})",
        )
    }

    private fun readPrimaryClipAsSystem(): String? {
        val token = Binder.clearCallingIdentity()
        return try {
            withEffectiveUid(SYSTEM_UID) {
                val binder = SystemServiceHelper.getSystemService("clipboard")
                    ?: return@withEffectiveUid null
                val clipboard = clipboardInterface(binder)
                    ?: return@withEffectiveUid null
                extractFrameworkText(
                    invokeGetPrimaryClip(
                        clipboard = clipboard,
                        packageName = SYSTEM_PACKAGE,
                        userId = 0,
                    ),
                )
            }
        } catch (t: Throwable) {
            Log.w(TAG, "system clipboard read failed", t)
            null
        } finally {
            Binder.restoreCallingIdentity(token)
        }
    }

    private fun primeHoneyboardBaseline() {
        lastHoneyboardClipKey = runCatching {
            readLatestHoneyboardClip()?.key
        }.onFailure {
            Log.w(TAG, "failed to read HoneyBoard clipboard baseline", it)
        }.getOrNull()
    }

    private fun readRecentHoneyboardText(eventReceivedAt: Long): String? {
        var failure: Throwable? = null
        repeat(HONEYBOARD_QUERY_ATTEMPTS) { attempt ->
            if (!monitorActive) return null
            val clip = try {
                readLatestHoneyboardClip()
            } catch (t: Throwable) {
                failure = t
                null
            }
            if (
                clip != null &&
                clip.key != lastHoneyboardClipKey &&
                clip.key.timestamp >= eventReceivedAt - HONEYBOARD_EVENT_MAX_SKEW_MS
            ) {
                lastHoneyboardClipKey = clip.key
                return clip.text.takeIf(String::isNotEmpty)
            }
            if (attempt + 1 < HONEYBOARD_QUERY_ATTEMPTS) {
                Thread.sleep(HONEYBOARD_QUERY_RETRY_MS)
            }
        }
        failure?.let { Log.w(TAG, "HoneyBoard clipboard read failed", it) }
        return null
    }

    private fun readLatestHoneyboardClip(): HoneyboardClip? {
        val databaseUid = Os.stat(HONEYBOARD_DATABASE_PATH).st_uid
        return withEffectiveUid(databaseUid) {
            SQLiteDatabase.openDatabase(
                HONEYBOARD_DATABASE_PATH,
                null,
                SQLiteDatabase.OPEN_READONLY or SQLiteDatabase.NO_LOCALIZED_COLLATORS,
            ).use { database ->
                val key = database.rawQuery(
                    "SELECT id, time_stamp FROM clip_table " +
                        "ORDER BY time_stamp DESC, id DESC LIMIT 1",
                    null,
                ).use { cursor ->
                    if (!cursor.moveToFirst()) return@use null
                    HoneyboardClipKey(
                        id = cursor.getLong(0),
                        timestamp = cursor.getLong(1),
                    )
                } ?: return@use null
                val text = database.compileStatement(
                    "SELECT text FROM clip_table WHERE id = ? LIMIT 1",
                ).use { statement ->
                    statement.bindLong(1, key.id)
                    statement.simpleQueryForString()
                }
                HoneyboardClip(key = key, text = text)
            }
        }
    }

    private fun samsungClipboardEventTransactionCode(): Int =
        samsungListenerStubField("TRANSACTION_onClipboardEvent")

    private fun samsungClipboardFilterTransactionCode(): Int =
        samsungListenerStubField("TRANSACTION_onUpdateFilter")

    private fun samsungListenerStubField(name: String): Int {
        val stub = Class.forName("$SAMSUNG_LISTENER_INTERFACE\$Stub")
        return stub.getDeclaredField(name).run {
            isAccessible = true
            getInt(null)
        }
    }

    @Suppress("UNCHECKED_CAST")
    private fun samsungClipCreator(): Parcelable.Creator<Any> {
        val clipClass = Class.forName(SAMSUNG_CLIP_DATA_CLASS)
        return clipClass.getField("CREATOR").get(null) as Parcelable.Creator<Any>
    }

    private fun extractSamsungText(clip: Any?): String? {
        clip ?: return null
        val direct = when (clip.javaClass.simpleName) {
            "SemHtmlClipData" -> invokeStringMethod(clip, "getPlainText")
            "SemTextClipData" -> invokeStringMethod(clip, "getText")
            else -> null
        }
        if (!direct.isNullOrEmpty()) return direct
        val frameworkClip = runCatching {
            clip.javaClass.getMethod("getClipData").invoke(clip) as? ClipData
        }.getOrNull() ?: return null
        if (frameworkClip.itemCount == 0) return null
        val item = frameworkClip.getItemAt(0)
        return item.text?.toString() ?: item.htmlText?.toString()
    }

    private fun invokeStringMethod(target: Any, name: String): String? =
        runCatching {
            target.javaClass.getMethod(name).invoke(target)?.toString()
        }.getOrNull()

    private fun extractFrameworkText(clip: ClipData?): String? {
        if (clip == null || clip.itemCount == 0) return null
        val item = clip.getItemAt(0)
        return item.text?.toString() ?: item.htmlText?.toString()
    }

    private fun ensureShellIdentity() {
        when (Process.myUid()) {
            SHELL_UID -> return
            ROOT_UID -> {
                Os.setgid(SHELL_UID)
                Os.setuid(SHELL_UID)
                check(Process.myUid() == SHELL_UID) {
                    "failed to drop clipboard service to shell uid"
                }
            }
            else -> error("clipboard UserService has unsupported uid=${Process.myUid()}")
        }
    }

    private inline fun <T> withEffectiveUid(uid: Int, block: () -> T): T {
        return synchronized(identityLock) {
            check(Process.myUid() == ROOT_UID) {
                "effective uid switching requires retained root uid"
            }
            Os.seteuid(uid)
            try {
                block()
            } finally {
                Os.seteuid(ROOT_UID)
            }
        }
    }

    private fun clipboardInterface(binder: IBinder): Any? {
        val stubClass = Class.forName("android.content.IClipboard\$Stub")
        val asInterface = stubClass.getMethod("asInterface", IBinder::class.java)
        return asInterface.invoke(null, binder)
    }

    /**
     * IClipboard.getPrimaryClip changed shape across Android releases.
     */
    private fun invokeGetPrimaryClip(
        clipboard: Any,
        packageName: String,
        userId: Int,
    ): ClipData? {
        val cls = clipboard.javaClass
        runCatching {
            val method = cls.getMethod(
                "getPrimaryClip",
                String::class.java,
                String::class.java,
                Int::class.javaPrimitiveType,
                Int::class.javaPrimitiveType,
            )
            return method.invoke(clipboard, packageName, null, userId, 0) as? ClipData
        }
        runCatching {
            val method = cls.getMethod(
                "getPrimaryClip",
                String::class.java,
                String::class.java,
                Int::class.javaPrimitiveType,
            )
            return method.invoke(clipboard, packageName, null, userId) as? ClipData
        }
        runCatching {
            val method = cls.getMethod(
                "getPrimaryClip",
                String::class.java,
                Int::class.javaPrimitiveType,
            )
            return method.invoke(clipboard, packageName, userId) as? ClipData
        }
        return null
    }

    override fun destroy() {
        runCatching { stopClipboardMonitor() }
        samsungEventExecutor.shutdownNow()
        exitProcess(0)
    }

    private companion object {
        data class HoneyboardClipKey(val id: Long, val timestamp: Long)
        data class HoneyboardClip(val key: HoneyboardClipKey, val text: String)

        const val TAG = "ClipboardUserService"
        const val ROOT_UID = 0
        const val SYSTEM_UID = 1000
        const val SHELL_UID = 2000
        const val SYSTEM_PACKAGE = "android"
        const val SHELL_PACKAGE = "com.android.shell"
        const val HONEYBOARD_DATABASE_PATH =
            "/data/user/0/com.samsung.android.honeyboard/databases/ClipItem.db"
        const val HONEYBOARD_QUERY_ATTEMPTS = 6
        const val HONEYBOARD_QUERY_RETRY_MS = 20L
        const val HONEYBOARD_EVENT_MAX_SKEW_MS = 5_000L
        const val SAMSUNG_CLIPBOARD_SERVICE = "semclipboard"
        const val SAMSUNG_SERVICE_INTERFACE = "android.sec.clipboard.IClipboardService"
        const val SAMSUNG_LISTENER_INTERFACE =
            "com.samsung.android.content.clipboard.IOnClipboardEventListener"
        const val SAMSUNG_CLIP_DATA_CLASS =
            "com.samsung.android.content.clipboard.data.SemClipData"
    }
}
