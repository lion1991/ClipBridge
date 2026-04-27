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

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            sendSection
            Divider()
            autoSaveRow
            Divider()
            historyTabs
        }
        .padding(16)
        .frame(minWidth: 480, minHeight: 560)
    }

    // MARK: - Send

    private var sendSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("发送图片").font(.headline)
            ZStack {
                RoundedRectangle(cornerRadius: 12)
                    .strokeBorder(
                        dropHover ? Color.accentColor : Color.secondary.opacity(0.4),
                        style: StrokeStyle(lineWidth: 1.5, dash: [6])
                    )
                    .background(
                        RoundedRectangle(cornerRadius: 12)
                            .fill(dropHover
                                  ? Color.accentColor.opacity(0.08)
                                  : Color(nsColor: .controlBackgroundColor))
                    )
                VStack(spacing: 8) {
                    Image(systemName: "tray.and.arrow.up")
                        .font(.system(size: 30))
                        .foregroundStyle(.secondary)
                    Text(dropHover ? "松手发送" : "拖入图片到这里")
                        .font(.subheadline)
                    Text("或")
                        .font(.caption)
                        .foregroundColor(.secondary)
                    Button("选择文件…") { pickFiles() }
                }
                .padding(20)
            }
            .frame(height: 130)
            // Accept image-typed URLs only; non-image drops are ignored
            // silently rather than firing an error toast for every random
            // file the user might brush over the window.
            .onDrop(of: [.fileURL], isTargeted: $dropHover) { providers in
                handleDrop(providers: providers)
            }
            Text("单张上限 32MB · 自动转 PNG 发送").font(.caption2).foregroundColor(.secondary)
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

    private var autoSaveRow: some View {
        HStack(spacing: 8) {
            Text("自动保存到:").font(.subheadline)
            Text(autoSaveFolderPath.isEmpty ? "未设置" : abbreviated(autoSaveFolderPath))
                .font(.subheadline)
                .foregroundColor(autoSaveFolderPath.isEmpty ? .secondary : .primary)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer()
            Button("选择…") { pickAutoSaveFolder() }
            if !autoSaveFolderPath.isEmpty {
                Button("清除") {
                    AppSettings.imageAutoSaveFolder = nil
                    autoSaveFolderPath = ""
                }
            }
        }
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

    // MARK: - History tabs

    @State private var selectedTab: HistoryTab = .received

    private var historyTabs: some View {
        VStack(alignment: .leading, spacing: 8) {
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
                Text(selectedTab == .received
                     ? "暂无 — 等待其他设备发图"
                     : "暂无 — 拖入图片或选择文件后会出现")
                    .font(.callout)
                    .foregroundColor(.secondary)
                    .frame(maxWidth: .infinity, alignment: .center)
                    .padding(.vertical, 30)
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(entries) { entry in
                            ImageHistoryRow(entry: entry, kind: selectedTab)
                            Divider()
                        }
                    }
                }
            }
        }
    }
}

private enum HistoryTab: Hashable {
    case received
    case sent
}

private struct ImageHistoryRow: View {
    let entry: ImageHistoryEntry
    let kind: HistoryTab

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            thumbnail
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Text(entry.deviceName)
                        .font(.subheadline.weight(.medium))
                    Text("·")
                        .foregroundColor(.secondary)
                    Text(relative(entry.date))
                        .font(.caption)
                        .foregroundColor(.secondary)
                    Spacer()
                }
                Text("\(entry.dimsLabel) · \(entry.sizeLabel)")
                    .font(.caption)
                    .foregroundColor(.secondary)
                HStack(spacing: 6) {
                    Button("保存到…") { saveAs() }
                    Button("再粘贴") { rePaste() }
                    Button("Finder 显示") { revealLastSaved() }
                        .disabled(lastSavedURL == nil)
                }
                .controlSize(.small)
            }
        }
        .padding(.horizontal, 4)
        .padding(.vertical, 10)
    }

    @State private var lastSavedURL: URL? = nil

    private var thumbnail: some View {
        Group {
            if let img = NSImage(data: entry.bytes) {
                Image(nsImage: img)
                    .resizable()
                    .aspectRatio(contentMode: .fill)
                    .frame(width: 64, height: 64)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                RoundedRectangle(cornerRadius: 8)
                    .fill(Color.secondary.opacity(0.15))
                    .frame(width: 64, height: 64)
                    .overlay(Image(systemName: "photo").foregroundColor(.secondary))
            }
        }
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
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setData(entry.bytes, forType: .png)
        if let rep = NSBitmapImageRep(data: entry.bytes),
           let tiff = rep.representation(using: .tiff, properties: [:])
        {
            pb.setData(tiff, forType: .tiff)
        }
    }

    private func revealLastSaved() {
        guard let url = lastSavedURL else { return }
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }

    private func relative(_ date: Date) -> String {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .short
        return f.localizedString(for: date, relativeTo: Date())
    }
}
