package com.clipbridge

internal const val AIDL_TEXT_CHUNK_CHARS = 64 * 1024
internal const val AIDL_TEXT_MAX_CHUNKS = 512

/**
 * Reassembles the bounded Binder transactions emitted by ClipboardUserService.
 * A new first chunk always replaces an incomplete older transfer.
 */
internal class ClipboardTextAssembler {
    private var transferId: Long? = null
    private var expectedIndex = 0
    private var chunkCount = 0
    private var text = StringBuilder()

    fun append(
        transferId: Long,
        chunkIndex: Int,
        chunkCount: Int,
        textChunk: String,
    ): String? {
        if (
            chunkCount !in 1..AIDL_TEXT_MAX_CHUNKS ||
            chunkIndex !in 0 until chunkCount ||
            textChunk.length > AIDL_TEXT_CHUNK_CHARS
        ) {
            clear()
            return null
        }

        if (chunkIndex == 0) {
            clear()
            this.transferId = transferId
            this.chunkCount = chunkCount
        } else if (
            this.transferId != transferId ||
            this.chunkCount != chunkCount ||
            expectedIndex != chunkIndex
        ) {
            clear()
            return null
        }

        text.append(textChunk)
        expectedIndex += 1
        if (expectedIndex != chunkCount) return null

        return text.toString().also { clear() }
    }

    fun clear() {
        transferId = null
        expectedIndex = 0
        chunkCount = 0
        text = StringBuilder()
    }
}
