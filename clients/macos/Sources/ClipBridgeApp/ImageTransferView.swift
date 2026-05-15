import AppKit
import ClipbridgeCore
import SwiftUI
import UniformTypeIdentifiers

/// Dedicated view for explicit image transfer: user drops files / picks via
/// open panel to send, and sees a history of received images that can be
/// saved anywhere or re-pasted. Distinct from the pasteboard sync (which
/// runs in the background regardless of this window being open).
struct ImageTransferView: View {
    @ObservedObject var coordinator: BridgeCoordinator
    @State private var autoSaveFolderPath: String =
        AppSettings.imageAutoSaveFolder?.path ?? ""
    @State private var dropHover = false
    @State private var selectedTab: HistoryTab = .received

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            sendSection
            autoSaveCard
            historySection
        }
        .padding(20)
        .frame(minWidth: 480, minHeight: 560)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    // MARK: - Shared chrome

    private func sectionHeader(_ icon: String, _ title: String) -> some View {
        HStack(spacing: 6) {
            Image(systemName: icon)
                .font(.subheadline)
                .foregroundStyle(.secondary)
            Text(title).font(.headline)
        }
    }

    /// Subtle card surface reused by the auto-save row and history rows so
    /// the grouped content reads as distinct blocks rather than a flat list.
    private static func card(cornerRadius: CGFloat = 10) -> some View {
        RoundedRectangle(cornerRadius: cornerRadius)
            .fill(Color(nsColor: .controlBackgroundColor))
            .overlay(
                RoundedRectangle(cornerRadius: cornerRadius)
                    .strokeBorder(Color.secondary.opacity(0.12))
            )
    }

    // MARK: - Send

    private var sendSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            sectionHeader("paperplane", "发送图片")
            dropZone
            Label("单张上限 32MB · 自动转 PNG 发送", systemImage: "info.circle")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
    }

    private var dropZone: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 14)
                .fill(dropHover
                      ? Color.accentColor.opacity(0.10)
                      : Color(nsColor: .controlBackgroundColor))
            RoundedRectangle(cornerRadius: 14)
                .strokeBorder(
                    dropHover ? Color.accentColor : Color.secondary.opacity(0.35),
                    style: StrokeStyle(lineWidth: 1.5, dash: [7])
                )
            VStack(spacing: 10) {
                ZStack {
                    Circle()
                        .fill(dropHover
                              ? Color.accentColor.opacity(0.18)
                              : Color.secondary.opacity(0.10))
                        .frame(width: 56, height: 56)
                    Image(systemName: dropHover
                          ? "tray.and.arrow.down.fill"
                          : "tray.and.arrow.up")
                        .font(.system(size: 24, weight: .medium))
                        .foregroundStyle(dropHover ? Color.accentColor : .secondary)
                }
                Text(dropHover ? "松手发送" : "拖入图片到这里")
                    .font(.callout.weight(.medium))
                Button("选择文件…") { pickFiles() }
                    .buttonStyle(.borderedProminent)
            }
            .padding(24)
        }
        .frame(height: 160)
        .animation(.easeInOut(duration: 0.15), value: dropHover)
        // Accept image-typed URLs only; non-image drops are ignored
        // silently rather than firing an error toast for every random
        // file the user might brush over the window.
        .onDrop(of: [.fileURL], isTargeted: $dropHover) { providers in
            handleDrop(providers: providers)
        }
    }

    private func pickFiles() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = true
        panel.allowedContentTypes = [.image]
        if panel.runModal() == .OK {
            for url in panel.urls { coordinator.sendImageFromFile(at: url) }
        }
    }

    private func handleDrop(providers: [NSItemProvider]) -> Bool {
        var accepted = false
        for provider in providers {
            // loadItem rather than loadFileRepresentation: we want the file's
            // existing on-disk URL, not a copied tmp, so the source path is
            // used in the suggested filename.
            provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, _ in
                guard let data = item as? Data,
                      let url = URL(dataRepresentation: data, relativeTo: nil) else { return }
                guard let type = UTType(filenameExtension: url.pathExtension),
                      type.conforms(to: .image) else { return }
                DispatchQueue.main.async {
                    coordinator.sendImageFromFile(at: url)
                }
            }
            accepted = true
        }
        return accepted
    }

    // MARK: - Auto-save folder

    private var autoSaveCard: some View {
        HStack(spacing: 12) {
            Image(systemName: "folder")
                .font(.system(size: 16))
                .foregroundStyle(.secondary)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text("自动保存收到的图片")
                    .font(.subheadline.weight(.medium))
                Text(autoSaveFolderPath.isEmpty ? "未设置" : abbreviated(autoSaveFolderPath))
                    .font(.caption)
                    .foregroundStyle(autoSaveFolderPath.isEmpty ? AnyShapeStyle(.secondary) : AnyShapeStyle(.primary))
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: 8)
            if !autoSaveFolderPath.isEmpty {
                Button("清除") {
                    AppSettings.imageAutoSaveFolder = nil
                    autoSaveFolderPath = ""
                }
                .controlSize(.small)
            }
            Button("选择…") { pickAutoSaveFolder() }
                .controlSize(.small)
        }
        .padding(12)
        .background(Self.card())
    }

    private func pickAutoSaveFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.message = "选择自动保存收到图片的目录"
        if panel.runModal() == .OK, let url = panel.url {
            AppSettings.imageAutoSaveFolder = url
            autoSaveFolderPath = url.path
        }
    }

    private func abbreviated(_ path: String) -> String {
        path.replacingOccurrences(of: NSHomeDirectory(), with: "~")
    }

    // MARK: - History

    private var historySection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Picker("", selection: $selectedTab) {
                Text("最近收到 (\(coordinator.receivedImages.count))").tag(HistoryTab.received)
                Text("最近发送 (\(coordinator.sentImages.count))").tag(HistoryTab.sent)
            }
            .pickerStyle(.segmented)
            .labelsHidden()

            let entries = selectedTab == .received
                ? coordinator.receivedImages
                : coordinator.sentImages

            if entries.isEmpty {
                emptyState
            } else {
                ScrollView {
                    LazyVStack(spacing: 8) {
                        ForEach(entries) { entry in
                            ImageHistoryRow(
                                entry: entry,
                                kind: selectedTab,
                                coordinator: coordinator
                            )
                        }
                    }
                    .padding(.vertical, 2)
                }
            }
        }
        // Let the history own all remaining vertical space so the window
        // never shows a centered island of content with dead margins.
        .frame(maxHeight: .infinity, alignment: .top)
    }

    private var emptyState: some View {
        VStack(spacing: 12) {
            Image(systemName: selectedTab == .received ? "tray" : "paperplane")
                .font(.system(size: 38))
                .foregroundStyle(.tertiary)
            Text(selectedTab == .received ? "暂无收到的图片" : "暂无发送的图片")
                .font(.callout.weight(.medium))
                .foregroundStyle(.secondary)
            Text(selectedTab == .received
                 ? "其他设备发图后会出现在这里"
                 : "拖入图片或选择文件后会出现在这里")
                .font(.caption)
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private enum HistoryTab: Hashable {
    case received
    case sent
}

private struct ImageHistoryRow: View {
    let entry: ImageHistoryEntry
    let kind: HistoryTab
    let coordinator: BridgeCoordinator

    @State private var lastSavedURL: URL? = nil
    @State private var hover = false

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            thumbnail
            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 6) {
                    Image(systemName: kind == .received
                          ? "arrow.down.circle.fill"
                          : "arrow.up.circle.fill")
                        .font(.caption)
                        .foregroundStyle(kind == .received ? Color.green : Color.accentColor)
                    Text(entry.deviceName)
                        .font(.subheadline.weight(.semibold))
                        .lineLimit(1)
                    Spacer(minLength: 8)
                    Text(relative(entry.date))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Text("\(entry.dimsLabel) · \(entry.sizeLabel)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack(spacing: 6) {
                    Button { saveAs() } label: {
                        Label("保存到…", systemImage: "square.and.arrow.down")
                    }
                    Button { rePaste() } label: {
                        Label("再粘贴", systemImage: "doc.on.clipboard")
                    }
                    Button { revealInFinder() } label: {
                        Label("Finder", systemImage: "folder")
                    }
                    .disabled(revealableURL == nil)
                }
                .controlSize(.small)
                .buttonStyle(.bordered)
                .padding(.top, 2)
            }
        }
        .padding(10)
        .background(
            RoundedRectangle(cornerRadius: 10)
                .fill(hover
                      ? Color.secondary.opacity(0.10)
                      : Color(nsColor: .controlBackgroundColor))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10)
                .strokeBorder(Color.secondary.opacity(0.12))
        )
        .onHover { hover = $0 }
    }

    private var thumbnail: some View {
        Group {
            if let img = NSImage(data: entry.bytes) {
                Image(nsImage: img)
                    .resizable()
                    .aspectRatio(contentMode: .fill)
            } else {
                Color.secondary.opacity(0.15)
                    .overlay(Image(systemName: "photo").foregroundStyle(.secondary))
            }
        }
        .frame(width: 64, height: 64)
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .strokeBorder(Color.secondary.opacity(0.15))
        )
    }

    private func saveAs() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = entry.suggestedFilename
        // Default location: the auto-save folder if set, else the user's
        // Pictures dir. Either way, no surprise about where things land.
        panel.directoryURL = AppSettings.imageAutoSaveFolder
            ?? FileManager.default.urls(for: .picturesDirectory, in: .userDomainMask).first
        if panel.runModal() == .OK, let url = panel.url {
            do {
                try entry.bytes.write(to: url)
                lastSavedURL = url
            } catch {
                NSAlert(error: error).runModal()
            }
        }
    }

    private func rePaste() {
        // Routed through the coordinator: the heavy TIFF re-encode runs off
        // the main thread (no UI freeze on large images) and the pasteboard
        // write is dedup-guarded so the poll loop won't re-send it.
        coordinator.rePasteImageToClipboard(entry.bytes)
    }

    /// A file on disk this entry can be revealed at: the location the user
    /// manually saved to this session, or — for received images that were
    /// auto-saved — the deterministic `folder/suggestedFilename` path the
    /// coordinator writes to (see `autoSaveIfConfigured`). `nil` only when
    /// nothing has actually been written, which is when we grey out the
    /// button instead of revealing a path that doesn't exist.
    private var revealableURL: URL? {
        if let url = lastSavedURL,
           FileManager.default.fileExists(atPath: url.path) {
            return url
        }
        if let folder = AppSettings.imageAutoSaveFolder {
            let candidate = folder.appendingPathComponent(entry.suggestedFilename)
            if FileManager.default.fileExists(atPath: candidate.path) {
                return candidate
            }
        }
        return nil
    }

    private func revealInFinder() {
        guard let url = revealableURL else { return }
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }

    private func relative(_ date: Date) -> String {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .short
        return f.localizedString(for: date, relativeTo: Date())
    }
}
