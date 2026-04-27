package com.clipbridge

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.util.Log
import androidx.core.content.FileProvider
import java.io.ByteArrayOutputStream
import java.io.File
import java.security.MessageDigest

/**
 * Image-side counterpart of the existing text path in
 * `ClipBridgeAccessibilityService`. Three jobs:
 *
 *  - **Read** (Shizuku required): pull raw image bytes from the system
 *    clipboard via the IClipboard binder.
 *  - **Write**: dump bytes to the app's cache directory and surface them
 *    as a content:// URI through FileProvider so `ClipboardManager` can
 *    hand them to apps that paste images (Telegram, Signal, gallery, …).
 *  - **Pixel hash**: same SHA-256 of decoded RGBA pixels we use on Mac/iOS,
 *    so dedup survives Android's habit of re-encoding pasteboard bytes
 *    when ferrying them across processes.
 *
 * No persistent state — all helpers are static. The accessibility service
 * orchestrates the lifecycle.
 */
object ImagePipeline {
    private const val TAG = "ClipBridge.Image"
    private const val CACHE_SUBDIR = "clipbridge_images"
    /// Cap mirrors the relay's default `CLIPBRIDGE_BLOB_MAX_BYTES`. Larger
    /// images are dropped client-side rather than fail at the relay.
    const val MAX_IMAGE_BYTES = 32 * 1024 * 1024

    /** Bytes + metadata we need to publish a clip to the relay. */
    data class Outbound(
        val bytes: ByteArray,
        val mime: String,
        val width: UInt,
        val height: UInt,
    )

    // -------------------- Read --------------------

    /**
     * Decode bytes to a Bitmap purely for dimension extraction (no full
     * raster pulled into memory thanks to inJustDecodeBounds).
     */
    private fun bitmapBounds(bytes: ByteArray): Pair<Int, Int>? {
        val opts = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, opts)
        if (opts.outWidth <= 0 || opts.outHeight <= 0) return null
        return opts.outWidth to opts.outHeight
    }

    /**
     * Convert a freshly-read URI from the system clipboard into an
     * `Outbound` ready to send. Re-encodes to PNG when the source isn't
     * already PNG, so receivers on Win/Android don't need a HEIC decoder.
     * Returns null on any failure (URI inaccessible, decode failed).
     */
    fun outboundFromUri(ctx: Context, uri: Uri): Outbound? {
        return runCatching {
            val mime = ctx.contentResolver.getType(uri) ?: "image/*"
            val raw: ByteArray = ctx.contentResolver.openInputStream(uri)
                ?.use { it.readBytes() }
                ?: return@runCatching null

            // Normalize to PNG unless already PNG — keeps the wire format
            // predictable across platforms. JPEG re-encode would lose
            // quality, but for clipboard images PNG is the safe bet.
            val (bytes, finalMime) = if (mime == "image/png") {
                raw to "image/png"
            } else {
                val bmp = BitmapFactory.decodeByteArray(raw, 0, raw.size)
                    ?: return@runCatching null
                val out = ByteArrayOutputStream(raw.size)
                bmp.compress(Bitmap.CompressFormat.PNG, 100, out)
                bmp.recycle()
                out.toByteArray() to "image/png"
            }
            val (w, h) = bitmapBounds(bytes) ?: return@runCatching null
            Outbound(bytes, finalMime, w.toUInt(), h.toUInt())
        }.onFailure { Log.w(TAG, "outboundFromUri failed", it) }.getOrNull()
    }

    // -------------------- Write --------------------

    /**
     * Drop bytes into the app's cache and return a content:// URI suitable
     * for `ClipData.newUri`. Files are namespaced by sha256 so repeats
     * collapse onto one inode.
     */
    fun cacheToContentUri(ctx: Context, bytes: ByteArray, ext: String): Uri? {
        return runCatching {
            val dir = File(ctx.cacheDir, CACHE_SUBDIR).apply { mkdirs() }
            val sha = sha256Hex(bytes).take(16)
            val file = File(dir, "img_$sha.$ext")
            if (!file.exists() || file.length() != bytes.size.toLong()) {
                file.writeBytes(bytes)
            }
            FileProvider.getUriForFile(
                ctx,
                "${ctx.packageName}.fileprovider",
                file,
            )
        }.onFailure { Log.w(TAG, "cacheToContentUri failed", it) }.getOrNull()
    }

    /**
     * Write bytes to the system clipboard as an image. The URI comes from
     * our own FileProvider (declared with grantUriPermissions=true), so
     * the system's clipboard service automatically grants temporary read
     * permission to whoever subsequently pastes — no per-package grant
     * needed from us.
     */
    fun writeImageToClipboard(
        clipboard: ClipboardManager,
        ctx: Context,
        bytes: ByteArray,
        mime: String,
    ): Boolean {
        val ext = when (mime) {
            "image/png" -> "png"
            "image/jpeg" -> "jpg"
            "image/heic" -> "heic"
            else -> "img"
        }
        val uri = cacheToContentUri(ctx, bytes, ext) ?: return false
        // Build ClipData with the explicit mime — `ClipData.newUri` queries
        // the resolver for type, but our FileProvider's reported mime can
        // be "application/octet-stream" if Android's mime guesser doesn't
        // recognise the extension. Forcing image/png here keeps paste
        // targets routing to their image branch.
        val clip = ClipData(
            ClipDescription("ClipBridge image", arrayOf(mime)),
            ClipData.Item(uri),
        )
        clipboard.setPrimaryClip(clip)
        return true
    }

    // -------------------- Hash --------------------

    /**
     * SHA-256 of the decoded RGBA pixels. Invariant to encoder differences
     * (PNG / JPEG / system re-encodes) — without this, our dedup misses
     * round-trips through Android's clipboard manager (which sometimes
     * hands back re-encoded bytes) and we'd republish the same image as
     * a "new" clip. Costs ~10ms for a 1MP screenshot.
     */
    fun pixelHashHex(bytes: ByteArray): String? {
        val bmp = runCatching {
            BitmapFactory.decodeByteArray(bytes, 0, bytes.size)
        }.getOrNull() ?: return null
        return try {
            val w = bmp.width
            val h = bmp.height
            if (w <= 0 || h <= 0) return null
            val pixels = IntArray(w * h)
            bmp.getPixels(pixels, 0, w, 0, 0, w, h)
            val md = MessageDigest.getInstance("SHA-256")
            // Hash little-endian int representations directly. Stable
            // across Android versions because IntArray layout is fixed.
            val buf = ByteArray(pixels.size * 4)
            var i = 0
            for (px in pixels) {
                buf[i++] = (px and 0xff).toByte()
                buf[i++] = ((px ushr 8) and 0xff).toByte()
                buf[i++] = ((px ushr 16) and 0xff).toByte()
                buf[i++] = ((px ushr 24) and 0xff).toByte()
            }
            md.update(buf)
            md.digest().toHex()
        } finally {
            bmp.recycle()
        }
    }

    fun sha256Hex(bytes: ByteArray): String {
        val md = MessageDigest.getInstance("SHA-256")
        return md.digest(bytes).toHex()
    }

    fun sha256Hex(s: String): String = sha256Hex(s.toByteArray(Charsets.UTF_8))

    private fun ByteArray.toHex(): String {
        val sb = StringBuilder(size * 2)
        for (b in this) {
            val v = b.toInt() and 0xff
            sb.append(HEX[v ushr 4])
            sb.append(HEX[v and 0x0f])
        }
        return sb.toString()
    }

    private val HEX = charArrayOf(
        '0', '1', '2', '3', '4', '5', '6', '7',
        '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    )
}
