import Foundation

enum FileTransferDirection: Equatable {
    case received
    case sent
}

enum FileTransferStatus: Equatable {
    case sending
    case completed
    case failed(String)

    var label: String {
        switch self {
        case .sending: return "发送中"
        case .completed: return "完成"
        case .failed: return "失败"
        }
    }

    var detail: String? {
        if case .failed(let message) = self { return message }
        return nil
    }
}

struct FileTransferHistoryEntry: Identifiable, Equatable {
    let id: UUID
    let direction: FileTransferDirection
    let fileName: String
    let fileURL: URL?
    let deviceName: String
    var sizeBytes: UInt64
    let date: Date
    var status: FileTransferStatus

    init(
        id: UUID = UUID(),
        direction: FileTransferDirection,
        fileName: String,
        fileURL: URL?,
        deviceName: String,
        sizeBytes: UInt64,
        date: Date = Date(),
        status: FileTransferStatus
    ) {
        self.id = id
        self.direction = direction
        self.fileName = fileName
        self.fileURL = fileURL
        self.deviceName = deviceName
        self.sizeBytes = sizeBytes
        self.date = date
        self.status = status
    }

    init(received record: ReceivedFileRecord) {
        self.init(
            id: UUID(uuidString: record.transferId) ?? UUID(),
            direction: .received,
            fileName: record.fileName,
            fileURL: URL(fileURLWithPath: record.path),
            deviceName: "LAN 设备",
            sizeBytes: record.sizeBytes,
            status: .completed
        )
    }

    var sizeLabel: String {
        fileTransferSizeLabel(sizeBytes)
    }
}

func fileTransferTargetSummary<S: Sequence>(
    peers: [LanPeerRecord],
    selectedIds: S
) -> String where S.Element == String {
    guard !peers.isEmpty else { return "暂无可用 LAN 设备" }
    let validIds = Set(peers.map(\.deviceId))
    let selectedCount = Set(selectedIds).intersection(validIds).count
    if selectedCount == 0 { return "未选择设备" }
    if selectedCount == peers.count { return "已选择全部 \(peers.count) 台" }
    return "已选择 \(selectedCount)/\(peers.count) 台"
}

func fileTransferSizeLabel(_ bytes: UInt64) -> String {
    if bytes == 0 { return "0 KB" }
    let kb = max(1, Int((bytes + 1023) / 1024))
    if kb >= 1024 {
        return String(format: "%.1f MB", Double(bytes) / 1_048_576.0)
    }
    return "\(kb) KB"
}

func iOSFileReceiveDirectory() -> URL {
    let documents = FileManager.default.urls(
        for: .documentDirectory,
        in: .userDomainMask
    ).first ?? URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
    return documents.appendingPathComponent("ClipBridge", isDirectory: true)
}

func isRegularFileURL(_ url: URL) -> Bool {
    guard url.isFileURL,
          let values = try? url.resourceValues(forKeys: [.isRegularFileKey])
    else { return false }
    return values.isRegularFile == true
}

func fileByteSize(_ url: URL) -> UInt64 {
    guard let values = try? url.resourceValues(forKeys: [.fileSizeKey]),
          let size = values.fileSize,
          size > 0
    else { return 0 }
    return UInt64(size)
}
