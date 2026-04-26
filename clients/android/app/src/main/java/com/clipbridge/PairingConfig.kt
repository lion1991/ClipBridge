package com.clipbridge

import android.content.Context
import android.util.Base64
import kotlinx.serialization.Serializable
import kotlinx.serialization.SerialName
import kotlinx.serialization.json.Json
import java.security.SecureRandom
import java.util.UUID

/// Default relay endpoint baked into the app. Hidden from the pairing UI so
/// the user doesn't have to know about server URLs at all. Kept inside the
/// JSON / QR payload for forward compatibility with future "use a different
/// relay" advanced settings.
const val DEFAULT_RELAY_URL = "wss://clip.wrlog.cn"

/// Wire-compatible with the macOS / desktop pairing config: relay URL, group ID,
/// and the 32-byte ChaCha20-Poly1305 key encoded as base64url (no padding).
@Serializable
data class PairingConfig(
    @SerialName("relay_url") val relayUrl: String,
    @SerialName("group_id") val groupId: String,
    @SerialName("key") val keyBase64Url: String,
) {
    fun keyBytes(): ByteArray? = runCatching {
        Base64.decode(keyBase64Url, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
    }.getOrNull()

    companion object {
        fun makeNew(relayUrl: String = DEFAULT_RELAY_URL): PairingConfig {
            val key = ByteArray(32).also { SecureRandom().nextBytes(it) }
            val encoded = Base64.encodeToString(
                key,
                Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP,
            )
            return PairingConfig(
                relayUrl = relayUrl,
                groupId = UUID.randomUUID().toString(),
                keyBase64Url = encoded,
            )
        }
    }
}

object PairingStore {
    const val PREFS = "clipbridge_prefs"
    const val KEY_PAIRING = "pairing_v1"
    private val json = Json { ignoreUnknownKeys = true }

    fun load(context: Context): PairingConfig? {
        val raw = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(KEY_PAIRING, null) ?: return null
        return runCatching { json.decodeFromString(PairingConfig.serializer(), raw) }.getOrNull()
    }

    fun save(context: Context, config: PairingConfig) {
        val raw = json.encodeToString(PairingConfig.serializer(), config)
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString(KEY_PAIRING, raw)
            .apply()
    }

    fun clear(context: Context) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .remove(KEY_PAIRING)
            .apply()
    }

    fun deviceId(context: Context): String {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs.getString("device_id", null)?.let { return it }
        val id = UUID.randomUUID().toString()
        prefs.edit().putString("device_id", id).apply()
        return id
    }
}
