import SwiftUI
import UIKit

struct ContentView: View {
    @EnvironmentObject var coordinator: BridgeCoordinator
    @State private var showPairing = false
    @State private var showPhotoPicker = false
    @State private var showFilePicker = false
    @State private var selectedFileTargetIds: Set<String> = []
    @State private var saveToast: String?

    var body: some View {
        TabView {
            syncTab
                .tabItem { Label("同步", systemImage: "doc.on.clipboard") }
            transferTab
                .tabItem { Label("传输", systemImage: "tray.and.arrow.up") }
        }
    }

    // MARK: - Sync tab (status, pairing, text history)

    private var syncTab: some View {
        NavigationView {
            ScrollView {
                VStack(spacing: 16) {
                    StatusPill(status: coordinator.status, lanPeerNames: coordinator.lanPeerNames)
                    PairingCard(
                        hasPairing: coordinator.hasPairing,
                        onTap: { showPairing = true }
                    )
                    if coordinator.hasPairing {
                        // Filter at the card boundary rather than passing
                        // a `kind` flag in. Kept the cards generic so the
                        // image tab can reuse the exact same component.
                        ClipHistoryCard(
                            title: "最近收到",
                            hint: "下拉刷新",
                            emptyMessage: "暂无 — 等待其他设备复制的文字",
                            clips: coordinator.recentClips.filter { $0.kind == .text }
                        )
                        ClipHistoryCard(
                            title: "最近发送",
                            hint: nil,
                            emptyMessage: "暂无 — 在本机复制后会出现",
                            clips: coordinator.sentClips.filter { $0.kind == .text }
                        )
                    }
                    Spacer(minLength: 12)
                    Text("默认中继 · " + DEFAULT_RELAY_URL.replacingOccurrences(of: "wss://", with: ""))
                        .font(.caption2)
                        .foregroundColor(.secondary)
                }
                .padding(20)
            }
            .refreshable { coordinator.refreshRecent() }
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

    // MARK: - Transfer tab (LAN files + image transfer)

    private var transferTab: some View {
        NavigationView {
            ScrollView {
                VStack(spacing: 16) {
                    if coordinator.hasPairing {
                        FileTransferCard(
                            peers: coordinator.lanFilePeers,
                            selectedIds: $selectedFileTargetIds,
                            receiveDirectory: coordinator.fileReceiveDirectory,
                            onPickFiles: { showFilePicker = true }
                        )
                        FileTransferHistoryCard(entries: coordinator.fileTransferHistory)
                        ImageTransferCard(onSendImage: { showPhotoPicker = true })
                        ClipHistoryCard(
                            title: "最近收到",
                            hint: "下拉刷新",
                            emptyMessage: "暂无 — 等其他设备发图过来",
                            clips: coordinator.recentClips.filter { $0.kind == .image }
                        )
                        ClipHistoryCard(
                            title: "最近发送",
                            hint: nil,
                            emptyMessage: "暂无 — 选图发送或本机复制图片后会出现",
                            clips: coordinator.sentClips.filter { $0.kind == .image }
                        )
                    } else {
                        // Pre-pairing nudge — the sync tab is where the
                        // user pairs, so just point them there.
                        VStack(spacing: 8) {
                            Image(systemName: "qrcode")
                                .font(.system(size: 36))
                                .foregroundColor(.secondary)
                            Text("先到「同步」标签完成配对").font(.callout)
                                .foregroundColor(.secondary)
                        }
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding(.top, 40)
                    }
                    Spacer(minLength: 12)
                }
                .padding(20)
            }
            .refreshable { coordinator.refreshRecent() }
            .navigationTitle("传输")
            .navigationBarTitleDisplayMode(.inline)
            .sheet(isPresented: $showPhotoPicker) {
                PhotoPickerSheet(isPresented: $showPhotoPicker) { bytes in
                    coordinator.sendImageBytes(bytes)
                }
            }
            .sheet(isPresented: $showFilePicker) {
                FilePickerSheet(isPresented: $showFilePicker) { urls in
                    coordinator.sendFiles(urls: urls, to: selectedFileTargetIds)
                }
            }
            .onChange(of: coordinator.lanFilePeers) { peers in
                let validIds = Set(peers.map(\.deviceId))
                selectedFileTargetIds = selectedFileTargetIds.intersection(validIds)
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

struct FileTransferCard: View {
    let peers: [LanPeerRecord]
    @Binding var selectedIds: Set<String>
    let receiveDirectory: URL
    let onPickFiles: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("发送文件")
                        .font(.subheadline.weight(.semibold))
                    Text(fileTransferTargetSummary(peers: peers, selectedIds: selectedIds))
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
                Spacer()
                Button(allSelected ? "清空" : "全选") {
                    if allSelected {
                        selectedIds.removeAll()
                    } else {
                        selectedIds = Set(peers.map(\.deviceId))
                    }
                }
                .font(.caption.weight(.medium))
                .disabled(peers.isEmpty)
            }

            if peers.isEmpty {
                HStack(spacing: 10) {
                    Image(systemName: "wifi.slash")
                        .foregroundColor(.secondary)
                    Text("等同组设备出现在局域网后可发送文件")
                        .font(.caption)
                        .foregroundColor(.secondary)
                    Spacer()
                }
                .padding(12)
                .background(rowBackground)
            } else {
                VStack(spacing: 8) {
                    ForEach(peers, id: \.deviceId) { peer in
                        Button {
                            toggle(peer.deviceId)
                        } label: {
                            HStack(spacing: 10) {
                                Image(systemName: selectedIds.contains(peer.deviceId)
                                      ? "checkmark.circle.fill"
                                      : "circle")
                                    .foregroundColor(selectedIds.contains(peer.deviceId) ? .accentColor : .secondary)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(peer.displayName)
                                        .font(.callout.weight(.medium))
                                        .foregroundColor(.primary)
                                        .lineLimit(1)
                                    Text("\(peer.candidateCount) 个地址")
                                        .font(.caption2)
                                        .foregroundColor(.secondary)
                                }
                                Spacer()
                            }
                            .padding(12)
                            .background(rowBackground)
                        }
                        .buttonStyle(.plain)
                    }
                }
            }

            Button(action: onPickFiles) {
                Label("选择文件发送", systemImage: "doc.badge.plus")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .disabled(selectedIds.isEmpty)

            HStack(spacing: 6) {
                Image(systemName: "folder")
                Text("接收目录 · \(receiveDirectory.lastPathComponent)")
                    .lineLimit(1)
            }
            .font(.caption2)
            .foregroundColor(.secondary)
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 16)
                .fill(Color(uiColor: .secondarySystemBackground))
        )
    }

    private var allSelected: Bool {
        !peers.isEmpty && Set(peers.map(\.deviceId)).isSubset(of: selectedIds)
    }

    private var rowBackground: some View {
        RoundedRectangle(cornerRadius: 12)
            .fill(Color(uiColor: .tertiarySystemBackground))
    }

    private func toggle(_ id: String) {
        if selectedIds.contains(id) {
            selectedIds.remove(id)
        } else {
            selectedIds.insert(id)
        }
    }
}

struct FileTransferHistoryCard: View {
    let entries: [FileTransferHistoryEntry]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("文件记录")
                    .font(.subheadline.weight(.semibold))
                Spacer()
                Text("\(entries.count)")
                    .font(.caption2)
                    .foregroundColor(.secondary)
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 10)

            if entries.isEmpty {
                Text("暂无文件传输记录")
                    .font(.caption)
                    .foregroundColor(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.bottom, 14)
            } else {
                ForEach(Array(entries.enumerated()), id: \.element.id) { index, entry in
                    FileTransferRow(entry: entry)
                    if index < entries.count - 1 {
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

private struct FileTransferRow: View {
    let entry: FileTransferHistoryEntry

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: iconName)
                .font(.title3)
                .foregroundColor(iconColor)
                .frame(width: 34, height: 34)
                .background(Circle().fill(iconColor.opacity(0.12)))

            VStack(alignment: .leading, spacing: 5) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(entry.fileName)
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: 6)
                    Text(entry.status.label)
                        .font(.caption2.weight(.semibold))
                        .foregroundColor(statusColor)
                        .padding(.horizontal, 7)
                        .padding(.vertical, 3)
                        .background(Capsule().fill(statusColor.opacity(0.12)))
                }
                Text("\(entry.deviceName) · \(entry.sizeLabel) · \(relative(entry.date))")
                    .font(.caption)
                    .foregroundColor(.secondary)
                    .lineLimit(1)
                if let detail = entry.status.detail {
                    Text(detail)
                        .font(.caption2)
                        .foregroundColor(.red)
                        .lineLimit(2)
                }
            }

            if let url = entry.fileURL, FileManager.default.fileExists(atPath: url.path) {
                Button {
                    presentFileShareSheet(url)
                } label: {
                    Image(systemName: "square.and.arrow.up")
                        .padding(6)
                }
                .buttonStyle(.plain)
                .foregroundColor(.secondary)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    private var iconName: String {
        entry.direction == .received ? "arrow.down.doc" : "arrow.up.doc"
    }

    private var iconColor: Color {
        entry.direction == .received ? .green : .accentColor
    }

    private var statusColor: Color {
        switch entry.status {
        case .sending: return .accentColor
        case .completed: return .green
        case .failed: return .red
        }
    }

    private func relative(_ date: Date) -> String {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter.localizedString(for: date, relativeTo: Date())
    }
}

struct StatusPill: View {
    let status: BridgeStatus
    let lanPeerNames: [String]

    var body: some View {
        // Append a transport hint only when actually connected — until
        // then the user wants to know *why* we're not connected, not
        // which lane would have been used.
        let connectedLabel: String = {
            if lanPeerNames.isEmpty {
                return "已连接 · 同步中 · 仅中继"
            } else {
                return "已连接 · 同步中 · 局域网 \(lanPeerNames.count) (\(lanPeerNames.joined(separator: ", ")))"
            }
        }()
        let (icon, label, color): (String, String, Color) = {
            switch status {
            case .notPaired:
                return ("link.badge.plus", "未配对", .secondary)
            case .connecting:
                return ("arrow.triangle.2.circlepath", "连接中…", .orange)
            case .connected:
                return ("checkmark.circle.fill", connectedLabel, .green)
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

private func presentFileShareSheet(_ url: URL) {
    guard let top = topViewController() else { return }
    let activity = UIActivityViewController(
        activityItems: [url],
        applicationActivities: nil
    )
    if let popover = activity.popoverPresentationController {
        popover.sourceView = top.view
        popover.sourceRect = CGRect(
            x: top.view.bounds.midX,
            y: top.view.bounds.midY,
            width: 1,
            height: 1
        )
        popover.permittedArrowDirections = []
    }
    top.present(activity, animated: true)
}

#Preview {
    ContentView()
        .environmentObject(BridgeCoordinator.shared)
}
