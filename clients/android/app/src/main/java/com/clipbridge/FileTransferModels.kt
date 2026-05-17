package com.clipbridge

import java.util.Locale

data class LanFilePeer(
    val deviceId: String,
    val displayName: String,
    val candidateCount: Int,
)

enum class FileTransferDirection { SENT, RECEIVED }

enum class FileTransferStatus(val label: String) {
    SENDING("发送中"),
    SENT("已发送"),
    RECEIVED("已接收"),
    FAILED("失败"),
}

data class FileTransferHistoryEntry(
    val id: String,
    val fileName: String,
    val deviceName: String,
    val sizeBytes: ULong,
    val direction: FileTransferDirection,
    val status: FileTransferStatus,
    val path: String? = null,
    val message: String? = null,
    val tsMillis: Long = System.currentTimeMillis(),
) {
    val sizeLabel: String
        get() = fileTransferSizeLabel(sizeBytes)
}

internal fun fileTransferTargetSummary(
    peers: List<LanFilePeer>,
    selectedDeviceIds: Set<String>,
): String {
    if (peers.isEmpty()) return "暂无可用设备"
    val peerIds = peers.map { it.deviceId }.toSet()
    val selectedCount = selectedDeviceIds.count { it in peerIds }
    return when {
        selectedCount == 0 -> "未选择设备"
        selectedCount == peerIds.size -> "已选择全部 ${peerIds.size} 台"
        else -> "已选择 $selectedCount/${peerIds.size} 台"
    }
}

internal fun sanitizeAndroidFileTransferName(raw: String): String {
    val withoutTraversal = raw
        .trim()
        .substringAfterLast("../")
        .substringAfterLast("..\\")
    val cleaned = withoutTraversal
        .map { ch ->
            when {
                ch.code < 32 -> '_'
                ch == '/' || ch == '\\' -> '_'
                else -> ch
            }
        }
        .joinToString("")
        .trim(' ', '.')
        .replace(Regex("_+"), "_")
    return cleaned.ifBlank { "file" }
}

internal fun fileTransferSizeLabel(bytes: ULong): String {
    val kb = ((bytes + 1023UL) / 1024UL).coerceAtLeast(1UL)
    return if (kb >= 1024UL) {
        String.format(Locale.US, "%.1f MB", kb.toDouble() / 1024.0)
    } else {
        "$kb KB"
    }
}
