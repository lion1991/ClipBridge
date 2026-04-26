package com.clipbridge

import android.app.Application
import android.os.Build
import org.lsposed.hiddenapibypass.HiddenApiBypass

/**
 * Boots reflection access for Android 9+ before any code that needs to call
 * into hidden system APIs (we reflect into `android.content.IClipboard.Stub`
 * to read the clipboard with shell privileges via Shizuku).
 */
class ClipBridgeApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            // "L" matches every JNI class signature (they all start with 'L'),
            // i.e. exempt the entire hidden API list. We're sideloading and
            // not subject to Play policy.
            HiddenApiBypass.addHiddenApiExemptions("L")
        }
    }
}
