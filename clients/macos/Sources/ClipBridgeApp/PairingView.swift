import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins
import SwiftUI

/// Pairing window — macOS-native form with grouped sections (System Settings
/// style). The first section centers the QR for the other device to scan;
/// "高级选项" hides the JSON / paste / reset path that's only used when scanning
/// isn't possible.
struct PairingScreen: View {
    let existing: PairingConfig?
    let onSave: (PairingConfig) -> Void
    let onReset: () -> Void

    @State private var configJson: String = ""
    @State private var qr: NSImage?
    @State private var error: String?
    @State private var showAdvanced: Bool = false
    @State private var showRegenConfirm: Bool = false
    @State private var showResetConfirm: Bool = false

    init(
        existing: PairingConfig?,
        onSave: @escaping (PairingConfig) -> Void,
        onReset: @escaping () -> Void
    ) {
        self.existing = existing
        self.onSave = onSave
        self.onReset = onReset
        if let existing {
            let json = (try? Self.encodeJSON(existing)) ?? ""
            _configJson = State(initialValue: json)
            _qr = State(initialValue: Self.makeQR(text: json))
        }
    }

    private var hasPairing: Bool { qr != nil }

    var body: some View {
        Form {
            qrSection
            advancedSection
            relayFooter
        }
        .formStyle(.grouped)
        .frame(minWidth: 440, idealWidth: 460, minHeight: 560)
    }

    // MARK: - Sections

    private var qrSection: some View {
        Section("配对二维码") {
            VStack(spacing: 12) {
                qrImage
                Text(hasPairing
                     ? "在另一台设备上扫描即可加入"
                     : "点击下方按钮生成新二维码")
                    .font(.callout)
                    .foregroundStyle(.secondary)

                Button {
                    if hasPairing {
                        showRegenConfirm = true
                    } else {
                        generate()
                    }
                } label: {
                    Text(hasPairing ? "生成新二维码" : "生成新配对")
                        .frame(minWidth: 100)
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.regular)
                .confirmationDialog(
                    "重新生成会让当前已配对的设备失效",
                    isPresented: $showRegenConfirm,
                    titleVisibility: .visible
                ) {
                    Button("继续生成", role: .destructive) { generate() }
                    Button("取消", role: .cancel) {}
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 4)
        }
    }

    private var qrImage: some View {
        Group {
            if let qr {
                Image(nsImage: qr)
                    .resizable()
                    .interpolation(.none)
                    .frame(width: 220, height: 220)
                    .background(Color.white)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .overlay(
                        RoundedRectangle(cornerRadius: 8)
                            .stroke(.separator, lineWidth: 1)
                    )
            } else {
                ZStack {
                    RoundedRectangle(cornerRadius: 8)
                        .fill(Color(nsColor: .controlBackgroundColor))
                        .overlay(
                            RoundedRectangle(cornerRadius: 8)
                                .stroke(.separator, lineWidth: 1)
                        )
                    VStack(spacing: 6) {
                        Image(systemName: "qrcode")
                            .font(.system(size: 40))
                            .foregroundStyle(.tertiary)
                        Text("尚未生成")
                            .font(.caption)
                            .foregroundStyle(.tertiary)
                    }
                }
                .frame(width: 220, height: 220)
            }
        }
    }

    private var advancedSection: some View {
        Section {
            DisclosureGroup(isExpanded: $showAdvanced) {
                VStack(alignment: .leading, spacing: 10) {
                    Text("粘贴另一台设备的配对 JSON,或查看、复制当前配对内容。")
                        .font(.caption)
                        .foregroundStyle(.secondary)

                    TextEditor(text: $configJson)
                        .font(.system(.caption, design: .monospaced))
                        .frame(height: 110)
                        .padding(6)
                        .background(
                            RoundedRectangle(cornerRadius: 6)
                                .fill(Color(nsColor: .textBackgroundColor))
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 6)
                                .stroke(.separator, lineWidth: 1)
                        )
                        .onChange(of: configJson) { newValue in
                            qr = Self.makeQR(text: newValue)
                        }

                    if let error {
                        Text(error)
                            .font(.caption)
                            .foregroundStyle(.red)
                    }

                    HStack(spacing: 8) {
                        Button("复制 JSON", action: copyJSON)
                        Button("保存配对", action: save)
                            .buttonStyle(.borderedProminent)
                            .keyboardShortcut(.return, modifiers: [.command])
                        Spacer()
                        Button(role: .destructive) {
                            showResetConfirm = true
                        } label: {
                            Text("重置配对")
                        }
                        .confirmationDialog(
                            "重置后所有已配对设备都需要重新配对",
                            isPresented: $showResetConfirm,
                            titleVisibility: .visible
                        ) {
                            Button("确认重置", role: .destructive) {
                                configJson = ""
                                qr = nil
                                error = nil
                                onReset()
                            }
                            Button("取消", role: .cancel) {}
                        }
                    }
                }
                .padding(.top, 4)
            } label: {
                Text("高级选项")
                    .font(.callout.weight(.medium))
            }
        }
    }

    private var relayFooter: some View {
        Section {
            LabeledContent("默认中继") {
                Text(DEFAULT_RELAY_URL.replacingOccurrences(of: "wss://", with: ""))
                    .font(.system(.callout, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        }
    }

    // MARK: - Actions

    private func generate() {
        let cfg = PairingConfig.makeNew()
        let json = (try? Self.encodeJSON(cfg)) ?? ""
        configJson = json
        qr = Self.makeQR(text: json)
        error = nil
        onSave(cfg)
    }

    private func save() {
        let raw = configJson.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty,
              let data = raw.data(using: .utf8),
              let cfg = try? JSONDecoder().decode(PairingConfig.self, from: data),
              cfg.keyData != nil
        else {
            error = "配对信息无效:需要 relay_url、group_id、key(32 字节 base64url)"
            return
        }
        error = nil
        qr = Self.makeQR(text: raw)
        onSave(cfg)
    }

    private func copyJSON() {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(configJson, forType: .string)
    }

    // MARK: - Helpers

    private static func encodeJSON(_ cfg: PairingConfig) throws -> String {
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data = try enc.encode(cfg)
        return String(data: data, encoding: .utf8) ?? ""
    }

    private static func makeQR(text: String, pixelSize: CGFloat = 600) -> NSImage? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(trimmed.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage else { return nil }
        let scale = pixelSize / output.extent.width
        let scaled = output.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        let rep = NSCIImageRep(ciImage: scaled)
        let img = NSImage(size: rep.size)
        img.addRepresentation(rep)
        return img
    }
}
