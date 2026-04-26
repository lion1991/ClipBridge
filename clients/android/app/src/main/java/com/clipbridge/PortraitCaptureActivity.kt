package com.clipbridge

import com.journeyapps.barcodescanner.CaptureActivity

/**
 * ZXing's bundled `CaptureActivity` is declared landscape-only in its own
 * manifest, which forces our app to flip orientation when launching the
 * scanner. We extend it and override the orientation in our manifest so
 * the camera stays portrait.
 */
class PortraitCaptureActivity : CaptureActivity()
