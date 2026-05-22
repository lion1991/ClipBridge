package com.clipbridge

import android.view.accessibility.AccessibilityEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
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

    @Test
    fun fileTransferTargetSummaryReportsSelectionCount() {
        val peers = listOf(
            LanFilePeer("mac", "MacBook Pro", 2),
            LanFilePeer("win", "Windows Workstation", 1),
            LanFilePeer("android", "SM-S9380", 3),
        )

        assertEquals("未选择设备", fileTransferTargetSummary(peers, emptySet()))
        assertEquals("已选择 2/3 台", fileTransferTargetSummary(peers, setOf("mac", "android")))
        assertEquals("已选择全部 3 台", fileTransferTargetSummary(peers, setOf("mac", "win", "android")))
    }

    @Test
    fun androidFileTransferCacheNameKeepsBasenameAndRemovesUnsafeCharacters() {
        assertEquals("report.pdf", sanitizeAndroidFileTransferName("../report.pdf"))
        assertEquals("file", sanitizeAndroidFileTransferName("   "))
        assertEquals("a_b_c.txt", sanitizeAndroidFileTransferName("a/b\u0000c.txt"))
    }

    @Test
    fun enabledAccessibilityServicesWithAddsClipBridgeWithoutDroppingExistingServices() {
        val current = "com.example/.OtherService:com.vendor/.AssistService"
        val clipBridge = "com.clipbridge/.ClipBridgeAccessibilityService"

        assertEquals(
            "com.example/.OtherService:com.vendor/.AssistService:com.clipbridge/.ClipBridgeAccessibilityService",
            enabledAccessibilityServicesWith(current, clipBridge),
        )
    }

    @Test
    fun enabledAccessibilityServicesWithIsCaseInsensitiveAndIdempotent() {
        val current = "com.example/.OtherService:COM.CLIPBRIDGE/.ClipBridgeAccessibilityService"
        val clipBridge = "com.clipbridge/.ClipBridgeAccessibilityService"

        assertEquals(
            "com.example/.OtherService:COM.CLIPBRIDGE/.ClipBridgeAccessibilityService",
            enabledAccessibilityServicesWith(current, clipBridge),
        )
        assertEquals(
            "",
            enabledAccessibilityServicesWith("", ""),
        )
    }

    @Test
    fun fileTransferSizeLabelUsesCompactUnits() {
        assertEquals("1 KB", fileTransferSizeLabel(1UL))
        assertEquals("900 KB", fileTransferSizeLabel(900UL * 1024UL))
        assertEquals("1.5 MB", fileTransferSizeLabel(1536UL * 1024UL))
    }

    @Test
    fun selectedViewEventsAreNotTreatedAsClipboardSelections() {
        assertFalse(
            shouldRememberAccessibilitySelection(AccessibilityEvent.TYPE_VIEW_SELECTED),
        )
        assertTrue(
            shouldRememberAccessibilitySelection(
                AccessibilityEvent.TYPE_VIEW_TEXT_SELECTION_CHANGED,
            ),
        )
        assertTrue(
            shouldRememberAccessibilitySelection(AccessibilityEvent.TYPE_VIEW_LONG_CLICKED),
        )
    }
}
