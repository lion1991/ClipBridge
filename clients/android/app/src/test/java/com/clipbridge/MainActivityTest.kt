package com.clipbridge

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class MainActivityTest {
    @Test
    fun connectionTransportSuffixSummarizesLanPeerCountWithoutEmbeddingNames() {
        val names = listOf("Pixel 8 Pro", "Windows Workstation", "MacBook Pro")

        val suffix = connectionTransportSuffix(
            paired = true,
            asEnabled = true,
            state = UiConnState.Connected,
            lanPeerNames = names,
        )

        assertEquals(" · 局域网 3 台", suffix)
        assertFalse(suffix.contains("Pixel 8 Pro"))
        assertFalse(suffix.contains("Windows Workstation"))
        assertFalse(suffix.contains("MacBook Pro"))
    }
}
