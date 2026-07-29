package com.clipbridge

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class LanTransportPolicyTest {
    @Test
    fun mobileOnlyNetworkDoesNotEnableLan() {
        assertFalse(
            shouldEnableLanTransport(
                isTransportAwake = true,
                hasLanNetwork = false,
            ),
        )
    }

    @Test
    fun lanNetworkRequiresInteractiveScreen() {
        assertTrue(
            shouldEnableLanTransport(
                isTransportAwake = true,
                hasLanNetwork = true,
            ),
        )
        assertFalse(
            shouldEnableLanTransport(
                isTransportAwake = false,
                hasLanNetwork = true,
            ),
        )
    }
}
