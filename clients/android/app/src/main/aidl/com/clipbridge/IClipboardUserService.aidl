package com.clipbridge;

import com.clipbridge.IClipboardTextListener;

interface IClipboardUserService {
    void destroy() = 16777114;
    boolean readPrimaryClipText(IClipboardTextListener listener) = 1;
    boolean startClipboardMonitor(IClipboardTextListener listener) = 2;
    void stopClipboardMonitor() = 3;
}
