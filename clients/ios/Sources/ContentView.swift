import SwiftUI

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
                    Spacer(minLength: 12)
                    Text("默认中继 · " + DEFAULT_RELAY_URL.replacingOccurrences(of: "wss://", with: ""))
                        .font(.caption2)
                        .foregroundColor(.secondary)
                }
                .padding(20)
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

#Preview {
    ContentView()
        .environmentObject(BridgeCoordinator.shared)
}
