package com.clipbridge

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ClipboardTextAssemblerTest {
    @Test
    fun reassemblesTextLargerThanBinderTransactionLimit() {
        val text = buildString {
            repeat(300_000) {
                append("长文本")
                append(it)
                append('\n')
            }
        }
        val chunks = text.chunked(AIDL_TEXT_CHUNK_CHARS)
        val assembler = ClipboardTextAssembler()
        var completed: String? = null

        chunks.forEachIndexed { index, chunk ->
            completed = assembler.append(
                transferId = 7,
                chunkIndex = index,
                chunkCount = chunks.size,
                textChunk = chunk,
            )
            if (index < chunks.lastIndex) assertNull(completed)
        }

        assertEquals(text, completed)
    }

    @Test
    fun discardsOutOfOrderTransferAndAcceptsNextFirstChunk() {
        val assembler = ClipboardTextAssembler()

        assertNull(assembler.append(1, 0, 2, "old-"))
        assertNull(assembler.append(1, 0, 2, "replacement-"))
        assertEquals("replacement-text", assembler.append(1, 1, 2, "text"))
    }

    @Test
    fun rejectsOversizedBinderChunk() {
        val assembler = ClipboardTextAssembler()

        assertNull(
            assembler.append(
                transferId = 1,
                chunkIndex = 0,
                chunkCount = 1,
                textChunk = "x".repeat(AIDL_TEXT_CHUNK_CHARS + 1),
            ),
        )
    }
}
