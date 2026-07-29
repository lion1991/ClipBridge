package com.clipbridge

internal fun shouldEnableLanTransport(
    isTransportAwake: Boolean,
    hasLanNetwork: Boolean,
): Boolean = isTransportAwake && hasLanNetwork
