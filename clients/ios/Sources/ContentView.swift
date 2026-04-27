import SwiftUI
import UIKit

struct ContentView: View {
    @EnvironmentObject var coordinator: BridgeCoordinator
    @State private var showPairing = false

    var body: some View {
        NavigationView {
            ScrollView {
                VStack(spacing: 16) {
                    StatusPill(status: coordinator.status)
                    PairingCard(
                        hasPairing: coordinator.hasPairing,
                        onTap: { showPairing = true }
                    )
                    if coordinator.hasPairing {
                        ClipHistoryCard(
                            title: "最近收到",
                            hint: "下拉刷新",
                            emptyMessage: "暂无 — 等待其他设备复制的内容",
                            clips: coordinator.recentClips
                        )
                        ClipHistoryCard(
                            title: "最近发送",
                            hint: nil,
                            emptyMessage: "暂无 — 在本机复制后会出现",
                            clips: coordinator.sentClips
                        )
                    }
                    Spacer(minLength: 12)
                    Text("默认中继 · " + DEFAULT_RELAY_URL.replacingOccurrences(of: "wss://", with: ""))
                        .font(.caption2)
                        .foregroundColor(.secondary)
                }
                .padding(20)
            }
            // Pull-to-refresh kicks `Client.fetchRecent()` so the relay
            // pushes its 5-min cache again. Updates land via the listener
            // → handleIncoming → recentClips. Auto-cancels when the gesture
            // ends; we don't await anything so the spinner just blinks.
            .refreshable {
                coordinator.refreshRecent()
            }
            .navigationTitle("ClipBridge")
            .navigationBarTitleDisplayMode(.inline)
            .sheet(isPresented: $showPairing) {
                PairingScreen(
                    isPresented: $showPairing,
                    coordinator: coordinator
                )
            }
        }
        .navigationViewStyle(.stack)
    }
}

struct StatusPill: View {
    let status: BridgeStatus

    var body: some View {
        let (icon, label, color): (String, String, Color) = {
            switch status {
            case .notPaired:
                return ("link.badge.plus", "未配对", .secondary)
            case .connecting:
                return ("arrow.triangle.2.circlepath", "连接中…", .orange)
            case .connected:
                return ("checkmark.circle.fill", "已连接 · 同步中", .green)
            case .disconnected:
                return ("icloud.slash", "已断开,正在重连", .secondary)
            case .error(let msg):
                return ("exclamationmark.triangle.fill", "错误: \(msg)", .red)
            }
        }()

        HStack(spacing: 10) {
            Image(systemName: icon)
                .imageScale(.medium)
            Text(label)
                .font(.subheadline.weight(.medium))
                .lineLimit(2)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 14)
                .fill(color.opacity(0.12))
        )
        .foregroundStyle(color)
    }
}

struct PairingCard: View {
    let hasPairing: Bool
    let onTap: () -> Void

    var body: some View {
        Button(action: onTap) {
            HStack(spacing: 16) {
                Image(systemName: hasPairing ? "qrcode.viewfinder" : "qrcode")
                    .font(.system(size: 28))
                    .foregroundColor(.accentColor)
                    .frame(width: 48, height: 48)
                    .background(
                        Circle().fill(Color.accentColor.opacity(0.12))
                    )

                VStack(alignment: .leading, spacing: 2) {
                    Text(hasPairing ? "重新配对" : "扫码配对")
                        .font(.headline)
                    Text(hasPairing
                         ? "当前已配对,点击进行重新扫码或重置"
                         : "在另一台设备生成二维码并扫描")
                        .font(.caption)
                        .foregroundColor(.secondary)
                        .multilineTextAlignment(.leading)
                }
                Spacer()
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold))
                    .foregroundColor(.secondary)
            }
            .padding(16)
            .background(
                RoundedRectangle(cornerRadius: 16)
                    .fill(Color(uiColor: .secondarySystemBackground))
            )
        }
        .buttonStyle(.plain)
    }
}

/// Latest 3 clips in one direction (received or sent), newest first. Tap
/// a row to copy that clip back into the local pasteboard — handy when
/// something newer has overwritten it, or to re-send a previously-sent
/// item to other devices.
struct ClipHistoryCard: View {
    let title: String
    /// Optional right-aligned hint shown next to the title. We use it on
    /// the received card to advertise pull-to-refresh; the sent card
    /// passes nil.
    let hint: String?
    let emptyMessage: String
    let clips: [ClipPayload]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(title)
                    .font(.subheadline.weight(.semibold))
                Spacer()
                if let hint = hint {
                    Text(hint)
                        .font(.caption2)
                        .foregroundColor(.secondary)
                }
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 10)

            if clips.isEmpty {
                Text(emptyMessage)
                    .font(.caption)
                    .foregroundColor(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.bottom, 14)
            } else {
                ForEach(Array(clips.enumerated()), id: \.element) { index, clip in
                    RecentClipRow(clip: clip)
                    if index < clips.count - 1 {
                        Divider().padding(.leading, 16)
                    }
                }
                .padding(.bottom, 6)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 16)
                .fill(Color(uiColor: .secondarySystemBackground))
        )
    }
}

struct RecentClipRow: View {
    let clip: ClipPayload

    var body: some View {
        Button(action: copy) {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(clip.deviceName)
                        .font(.caption.weight(.medium))
                        .foregroundColor(.secondary)
                    Text("·")
                        .font(.caption)
                        .foregroundColor(.secondary)
                    Text(relativeTime(clip.ts))
                        .font(.caption)
                        .foregroundColor(.secondary)
                    Spacer(minLength: 0)
                    Image(systemName: "doc.on.doc")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
                Text(clip.content)
                    .font(.callout)
                    .foregroundColor(.primary)
                    .lineLimit(3)
                    .multilineTextAlignment(.leading)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func copy() {
        UIPasteboard.general.string = clip.content
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    private func relativeTime(_ tsMillis: UInt64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(tsMillis) / 1000)
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter.localizedString(for: date, relativeTo: Date())
    }
}

#Preview {
    ContentView()
        .environmentObject(BridgeCoordinator.shared)
}
