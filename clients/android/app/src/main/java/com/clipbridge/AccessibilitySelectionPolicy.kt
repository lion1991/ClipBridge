package com.clipbridge

import android.view.accessibility.AccessibilityEvent

internal fun shouldRememberAccessibilitySelection(eventType: Int): Boolean =
    eventType == AccessibilityEvent.TYPE_VIEW_TEXT_SELECTION_CHANGED ||
        eventType == AccessibilityEvent.TYPE_VIEW_LONG_CLICKED
