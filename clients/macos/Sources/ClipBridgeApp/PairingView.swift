import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins
import SwiftUI

/// SwiftUI pairing screen embedded in the AppKit window via NSHostingController.
/// Centerpiece is a large QR — the typical flow is "open this window, click
/// 生成, hand the QR to the other device, done". Paste-JSON path lives under
/// 高级 for the rare cases when scanning isn't available.
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

    private var hasPairing: Bool {
        !configJson.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

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

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                header

                qrCard

                primaryButton

                advancedSection

                Spacer(minLength: 6)

                Text("默认中继 · " + DEFAULT_RELAY_URL.replacingOccurrences(of: "wss://", with: ""))
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(24)
        }
        .frame(minWidth: 460, minHeight: 600)
    }

    // MARK: - Sections

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("配对设备")
                .font(.system(size: 22, weight: .semibold))
            Text(hasPairing
                 ? "在另一台设备上扫描下方二维码即可加入。"
                 : "点击「生成新配对」创建二维码,在另一台设备上扫码完成配对。")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var qrCard: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 18)
                .fill(Color.white)
                .overlay(
                    RoundedRectangle(cornerRadius: 18)
                        .stroke(Color.secondary.opacity(0.18), lineWidth: 1)
                )

            if let qr {
                Image(nsImage: qr)
                    .interpolation(.none)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .padding(20)
            } else {
                VStack(spacing: 10) {
                    Image(systemName: "qrcode")
                        .font(.system(size: 56))
                        .foregroundStyle(.secondary.opacity(0.5))
                    Text("尚未生成配对")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .frame(height: 280)
    }

    private var primaryButton: some View {
        Button {
            if hasPairing {
                showRegenConfirm = true
            } else {
                generate()
            }
        } label: {
            Label(hasPairing ? "生成新二维码" : "生成新配对", systemImage: "qrcode")
                .frame(maxWidth: .infinity)
                .padding(.vertical, 6)
        }
        .buttonStyle(.borderedProminent)
        .controlSize(.large)
        .confirmationDialog(
            "重新生成会让当前已配对的设备失效",
            isPresented: $showRegenConfirm,
            titleVisibility: .visible
        ) {
            Button("继续生成", role: .destructive) { generate() }
            Button("取消", role: .cancel) {}
        }
    }

    private var advancedSection: some View {
        DisclosureGroup(isExpanded: $showAdvanced) {
            VStack(alignment: .leading, spacing: 10) {
                Text("粘贴另一台设备的配对 JSON,或查看 / 复制当前配对内容。")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                TextEditor(text: $configJson)
                    .font(.system(.caption, design: .monospaced))
                    .frame(height: 140)
                    .padding(8)
                    .background(
                        RoundedRectangle(cornerRadius: 8)
                            .fill(Color(nsColor: .textBackgroundColor))
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 8)
                            .stroke(Color.secondary.opacity(0.25), lineWidth: 1)
                    )
                    .onChange(of: configJson) { newValue in
                        // Keep the QR preview in sync as the user types or pastes.
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
            .padding(.top, 8)
        } label: {
            Text("高级")
                .font(.callout.weight(.medium))
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
