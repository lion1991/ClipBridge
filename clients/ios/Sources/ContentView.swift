import SwiftUI
import UIKit

struct ContentView: View {
    @EnvironmentObject var coordinator: BridgeCoordinator
    @State private var showPairing = false
    @State private var showPhotoPicker = false
    @State private var saveToast: String?

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
                        ImageTransferCard(onSendImage: { showPhotoPicker = true })
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
            .sheet(isPresented: $showPhotoPicker) {
                PhotoPickerSheet(isPresented: $showPhotoPicker) { bytes in
                    coordinator.sendImageBytes(bytes)
                }
            }
            .overlay(alignment: .bottom) {
                if let toast = saveToast {
                    Text(toast)
                        .font(.callout)
                        .padding(.horizontal, 14).padding(.vertical, 8)
                        .background(.thinMaterial)
                        .clipShape(Capsule())
                        .padding(.bottom, 24)
                        .transition(.opacity.combined(with: .move(edge: .bottom)))
                }
            }
            .onPreferenceChange(SaveToastPreferenceKey.self) { msg in
                guard let msg else { return }
                withAnimation { saveToast = msg }
                DispatchQueue.main.asyncAfter(deadline: .now() + 2) {
                    withAnimation { saveToast = nil }
                }
            }
        }
        .navigationViewStyle(.stack)
    }
}

/// Preference key used by image rows to bubble "保存成功 / 失败" up to the
/// root view's overlay toast — avoids passing a binding through the row
/// constructor and keeps history rows free of save-state.
struct SaveToastPreferenceKey: PreferenceKey {
    static var defaultValue: String? = nil
    static func reduce(value: inout String?, nextValue: () -> String?) {
        let next = nextValue()
        if next != nil { value = next }
    }
}

/// Compact card with a single "从相册选图发送" button. Triggers the
/// PhotosUI picker without requiring full library access — the picker
/// runs in another process and only hands us the bytes for the items the
/// user explicitly tapped.
struct ImageTransferCard: View {
    let onSendImage: () -> Void

    var body: some View {
        Button(action: onSendImage) {
            HStack(spacing: 16) {
                Image(systemName: "photo.on.rectangle.angled")
                    .font(.system(size: 24))
                    .foregroundColor(.accentColor)
                    .frame(width: 48, height: 48)
                    .background(Circle().fill(Color.accentColor.opacity(0.12)))
                VStack(alignment: .leading, spacing: 2) {
                    Text("从相册发送图片")
                        .font(.headline)
                    Text("不走剪切板, 直接选图加密上传")
                        .font(.caption)
                        .foregroundColor(.secondary)
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
    @EnvironmentObject var coordinator: BridgeCoordinator

    var body: some View {
        Button(action: copy) {
            VStack(alignment: .leading, spacing: 6) {
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
                clipBody
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    @ViewBuilder
    private var clipBody: some View {
        switch clip.kind {
        case .text:
            Text(clip.content)
                .font(.callout)
                .foregroundColor(.primary)
                .lineLimit(3)
                .multilineTextAlignment(.leading)
        case .image:
            ImageClipBody(clip: clip)
        }
    }

    private func copy() {
        // Route through the coordinator: it handles cache miss (refetch
        // from relay's blob cache), writes both public.png + public.image
        // for IM-app compatibility, and updates dedup state in one place.
        coordinator.pasteFromHistory(clip)
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    private func relativeTime(_ tsMillis: UInt64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(tsMillis) / 1000)
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter.localizedString(for: date, relativeTo: Date())
    }
}

/// Image-kind body: small thumbnail + a one-line caption (`640×480 · 124 KB`).
/// The thumbnail comes from `ImageThumbCache`; if not yet cached (race with
/// the blob fetch on receive, or evicted under memory pressure) we render a
/// placeholder with the dimensions still visible so the row never collapses.
private struct ImageClipBody: View {
    let clip: ClipPayload
    @State private var saveToastMessage: String? = nil

    var body: some View {
        HStack(spacing: 12) {
            thumbnail
            VStack(alignment: .leading, spacing: 2) {
                Text(caption)
                    .font(.callout)
                    .foregroundColor(.primary)
                Text("图片")
                    .font(.caption2)
                    .foregroundColor(.secondary)
            }
            Spacer(minLength: 0)
            // Tap stops here so the parent row's "tap = re-paste" doesn't
            // also fire. Menu wraps two destinations: camera roll (silent,
            // permission-prompted on first save) and the share sheet
            // (lets the user pick "Save to Files", AirDrop, etc.).
            Menu {
                Button {
                    saveToCameraRoll()
                } label: {
                    Label("保存到相册", systemImage: "square.and.arrow.down")
                }
                Button {
                    presentShareSheet()
                } label: {
                    Label("分享 / 保存到文件…", systemImage: "square.and.arrow.up")
                }
            } label: {
                Image(systemName: "ellipsis.circle")
                    .font(.title3)
                    .foregroundColor(.secondary)
                    .padding(8)
                    .contentShape(Rectangle())
            }
            // Stop the row's `Button(action:)` from intercepting taps that
            // land on the menu. Without this, tapping the menu also fires
            // re-paste underneath.
            .onTapGesture { /* swallowed */ }
        }
        .preference(key: SaveToastPreferenceKey.self, value: saveToastMessage)
    }

    private func bytes() -> Data? {
        ImageThumbCache.shared.fullData(forTs: clip.ts)
    }

    private func saveToCameraRoll() {
        guard let data = bytes() else {
            postToast("图片字节已过期, 无法保存")
            return
        }
        PhotoSaver.saveToCameraRoll(data) { ok in
            postToast(ok ? "已保存到相册" : "保存失败 (检查相册权限)")
        }
    }

    private func presentShareSheet() {
        guard let data = bytes(), let img = UIImage(data: data) else {
            postToast("图片字节已过期")
            return
        }
        // Wrap the image so AirDrop / Files / WeChat all get a proper
        // image attachment rather than raw NSData; iOS renders a preview
        // in the share sheet too.
        let activity = UIActivityViewController(
            activityItems: [img],
            applicationActivities: nil,
        )
        activity.completionWithItemsHandler = { _, completed, _, _ in
            if completed { postToast("已分享") }
        }
        topViewController()?.present(activity, animated: true)
    }

    private func postToast(_ msg: String) {
        // Push then immediately clear so consecutive identical messages
        // still trigger the overlay (PreferenceKey only fires on change).
        saveToastMessage = msg
        DispatchQueue.main.async { saveToastMessage = nil }
    }

    @ViewBuilder
    private var thumbnail: some View {
        if let img = ImageThumbCache.shared.thumbnail(forTs: clip.ts) {
            Image(uiImage: img)
                .resizable()
                .aspectRatio(contentMode: .fill)
                .frame(width: 56, height: 56)
                .clipShape(RoundedRectangle(cornerRadius: 8))
        } else {
            RoundedRectangle(cornerRadius: 8)
                .fill(Color.secondary.opacity(0.15))
                .frame(width: 56, height: 56)
                .overlay(
                    Image(systemName: "photo")
                        .foregroundColor(.secondary)
                )
        }
    }

    private var caption: String {
        guard let meta = clip.image else { return "图片" }
        let kb = max(1, Int(meta.sizeBytes) / 1024)
        let size = kb >= 1024
            ? String(format: "%.1f MB", Double(kb) / 1024.0)
            : "\(kb) KB"
        return "\(meta.width)×\(meta.height) · \(size)"
    }
}

/// Walk to the topmost presented controller of the foreground key window
/// so `present(_:)` lands on the correct host. SwiftUI doesn't expose the
/// hosting controller directly; this is the canonical UIKit fallback.
private func topViewController() -> UIViewController? {
    let scenes = UIApplication.shared.connectedScenes
    let keyWindow = scenes
        .compactMap { $0 as? UIWindowScene }
        .flatMap { $0.windows }
        .first(where: { $0.isKeyWindow })
    var top = keyWindow?.rootViewController
    while let presented = top?.presentedViewController {
        top = presented
    }
    return top
}

#Preview {
    ContentView()
        .environmentObject(BridgeCoordinator.shared)
}
