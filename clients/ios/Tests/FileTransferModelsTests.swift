import XCTest
@testable import ClipBridge

final class FileTransferModelsTests: XCTestCase {
    func testTargetSummaryReflectsSelectionCount() {
        let peers = [
            LanPeerRecord(deviceId: "mac", displayName: "MacBook", candidateCount: 2),
            LanPeerRecord(deviceId: "win", displayName: "Windows", candidateCount: 1),
        ]

        XCTAssertEqual(
            fileTransferTargetSummary(peers: peers, selectedIds: ["mac"]),
            "已选择 1/2 台"
        )
        XCTAssertEqual(
            fileTransferTargetSummary(peers: peers, selectedIds: ["mac", "win"]),
            "已选择全部 2 台"
        )
    }

    func testTargetSummaryHandlesEmptyAndStaleSelection() {
        let peers = [
            LanPeerRecord(deviceId: "mac", displayName: "MacBook", candidateCount: 2),
        ]

        XCTAssertEqual(fileTransferTargetSummary(peers: [], selectedIds: []), "暂无可用 LAN 设备")
        XCTAssertEqual(fileTransferTargetSummary(peers: peers, selectedIds: []), "未选择设备")
        XCTAssertEqual(fileTransferTargetSummary(peers: peers, selectedIds: ["gone"]), "未选择设备")
    }

    func testFileSizeLabelsUseReadableUnits() {
        XCTAssertEqual(fileTransferSizeLabel(0), "0 KB")
        XCTAssertEqual(fileTransferSizeLabel(512), "1 KB")
        XCTAssertEqual(fileTransferSizeLabel(2_097_152), "2.0 MB")
    }

    func testReceivedRecordBuildsHistoryEntry() {
        let record = ReceivedFileRecord(
            transferId: "7D3C5A3B-1A43-48E6-B6A3-87CECFB2BC67",
            fileName: "report.pdf",
            path: "/tmp/ClipBridge/report.pdf",
            sizeBytes: 4096,
            sha256Hex: "abc"
        )

        let entry = FileTransferHistoryEntry(received: record)

        XCTAssertEqual(entry.id.uuidString, "7D3C5A3B-1A43-48E6-B6A3-87CECFB2BC67")
        XCTAssertEqual(entry.direction, .received)
        XCTAssertEqual(entry.fileName, "report.pdf")
        XCTAssertEqual(entry.fileURL?.path, "/tmp/ClipBridge/report.pdf")
        XCTAssertEqual(entry.sizeLabel, "4 KB")
        XCTAssertEqual(entry.status, .completed)
    }
}
