package com.clipbridge;

interface IClipboardTextListener {
    void onClipboardTextChunk(long transferId, int chunkIndex, int chunkCount, String textChunk);
}
