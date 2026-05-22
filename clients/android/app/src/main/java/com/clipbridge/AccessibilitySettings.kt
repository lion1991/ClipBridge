package com.clipbridge

import java.util.Locale

internal fun isAccessibilityServiceEnabledInSetting(
    current: String?,
    serviceName: String,
): Boolean {
    val expected = serviceName.trim()
    if (expected.isEmpty()) return false
    return current.orEmpty()
        .split(':')
        .any { it.trim().equals(expected, ignoreCase = true) }
}

internal fun enabledAccessibilityServicesWith(
    current: String?,
    serviceName: String,
): String {
    val entries = LinkedHashMap<String, String>()
    current.orEmpty()
        .split(':')
        .map { it.trim() }
        .filter { it.isNotEmpty() }
        .forEach { entries.putIfAbsent(it.lowercase(Locale.ROOT), it) }

    val normalizedServiceName = serviceName.trim()
    if (normalizedServiceName.isNotEmpty()) {
        entries.putIfAbsent(normalizedServiceName.lowercase(Locale.ROOT), normalizedServiceName)
    }

    return entries.values.joinToString(":")
}
