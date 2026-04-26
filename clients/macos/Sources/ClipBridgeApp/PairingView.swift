import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins

/// Programmatic AppKit view (avoids SwiftUI to keep the SwiftPM build minimal).
/// Two modes:
///   - "Generate new": user enters relay URL, we mint a fresh GroupConfig and show
///     it both as JSON text and as a QR for the Android app to scan.
///   - "Paste existing": user pastes JSON received from the other device.
final class PairingView: NSView {
    private let relayField = NSTextField()
    private let configTextView = NSTextView()
    private let qrImageView = NSImageView()
    private let onSave: (PairingConfig) -> Void

    init(existing: PairingConfig?, onSave: @escaping (PairingConfig) -> Void) {
        self.onSave = onSave
        super.init(frame: NSRect(x: 0, y: 0, width: 760, height: 460))

        let outer = NSStackView()
        outer.orientation = .vertical
        outer.alignment = .leading
        outer.spacing = 12
        outer.translatesAutoresizingMaskIntoConstraints = false
        outer.edgeInsets = NSEdgeInsets(top: 16, left: 20, bottom: 16, right: 20)
        addSubview(outer)
        NSLayoutConstraint.activate([
            outer.leadingAnchor.constraint(equalTo: leadingAnchor),
            outer.trailingAnchor.constraint(equalTo: trailingAnchor),
            outer.topAnchor.constraint(equalTo: topAnchor),
            outer.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])

        let title = NSTextField(labelWithString: "Pair this Mac with another device")
        title.font = .boldSystemFont(ofSize: 14)
        outer.addArrangedSubview(title)

        let relayLabel = NSTextField(labelWithString: "Relay URL (wss://host or ws://host:port)")
        outer.addArrangedSubview(relayLabel)
        relayField.stringValue = existing?.relayUrl ?? "wss://clip.wrlog.cn"
        relayField.translatesAutoresizingMaskIntoConstraints = false
        outer.addArrangedSubview(relayField)
        relayField.widthAnchor.constraint(equalToConstant: 700).isActive = true

        let buttonRow = NSStackView()
        buttonRow.orientation = .horizontal
        buttonRow.spacing = 8

        let genButton = NSButton(title: "Generate new pairing", target: self, action: #selector(generate))
        genButton.bezelStyle = .rounded
        buttonRow.addArrangedSubview(genButton)

        let pasteHint = NSTextField(labelWithString: "or paste JSON config from another device below")
        pasteHint.font = .systemFont(ofSize: 11)
        pasteHint.textColor = .secondaryLabelColor
        buttonRow.addArrangedSubview(pasteHint)

        outer.addArrangedSubview(buttonRow)

        // Side-by-side: JSON on the left, QR on the right.
        let middleRow = NSStackView()
        middleRow.orientation = .horizontal
        middleRow.alignment = .top
        middleRow.distribution = .fill
        middleRow.spacing = 16

        let scroll = NSScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.borderType = .bezelBorder
        scroll.hasVerticalScroller = true
        scroll.documentView = configTextView
        configTextView.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        configTextView.isAutomaticQuoteSubstitutionEnabled = false
        configTextView.isAutomaticDashSubstitutionEnabled = false
        configTextView.minSize = NSSize(width: 0, height: 0)
        configTextView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        configTextView.isVerticallyResizable = true
        configTextView.isHorizontallyResizable = false
        configTextView.autoresizingMask = [.width]
        configTextView.delegate = self
        if let existing = existing {
            configTextView.string = (try? PairingView.encodeJSON(existing)) ?? ""
        }
        middleRow.addArrangedSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.widthAnchor.constraint(equalToConstant: 420),
            scroll.heightAnchor.constraint(equalToConstant: 240),
        ])

        let qrColumn = NSStackView()
        qrColumn.orientation = .vertical
        qrColumn.alignment = .centerX
        qrColumn.spacing = 6
        let qrLabel = NSTextField(labelWithString: "Scan from Android")
        qrLabel.font = .systemFont(ofSize: 11)
        qrLabel.textColor = .secondaryLabelColor
        qrColumn.addArrangedSubview(qrLabel)
        qrImageView.imageScaling = .scaleProportionallyUpOrDown
        qrImageView.translatesAutoresizingMaskIntoConstraints = false
        qrImageView.wantsLayer = true
        qrImageView.layer?.backgroundColor = NSColor.white.cgColor
        qrColumn.addArrangedSubview(qrImageView)
        NSLayoutConstraint.activate([
            qrImageView.widthAnchor.constraint(equalToConstant: 240),
            qrImageView.heightAnchor.constraint(equalToConstant: 240),
        ])
        middleRow.addArrangedSubview(qrColumn)

        outer.addArrangedSubview(middleRow)

        let actionRow = NSStackView()
        actionRow.orientation = .horizontal
        actionRow.spacing = 8

        let copyButton = NSButton(title: "Copy JSON", target: self, action: #selector(copyJSON))
        copyButton.bezelStyle = .rounded
        actionRow.addArrangedSubview(copyButton)

        let saveButton = NSButton(title: "Save & start syncing", target: self, action: #selector(save))
        saveButton.bezelStyle = .rounded
        saveButton.keyEquivalent = "\r"
        actionRow.addArrangedSubview(saveButton)

        outer.addArrangedSubview(actionRow)

        refreshQR()
    }

    required init?(coder: NSCoder) { fatalError() }

    @objc private func generate() {
        let relay = relayField.stringValue.trimmingCharacters(in: .whitespaces)
        let cfg = PairingConfig.makeNew(relayUrl: relay.isEmpty ? "wss://clip.wrlog.cn" : relay)
        configTextView.string = (try? Self.encodeJSON(cfg)) ?? ""
        refreshQR()
    }

    @objc private func copyJSON() {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(configTextView.string, forType: .string)
    }

    @objc private func save() {
        let raw = configTextView.string.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty,
              let data = raw.data(using: .utf8),
              let cfg = try? JSONDecoder().decode(PairingConfig.self, from: data),
              cfg.keyData != nil
        else {
            let alert = NSAlert()
            alert.messageText = "Invalid pairing JSON"
            alert.informativeText = "Expected fields: relay_url, group_id, key (base64url, 32 bytes)."
            alert.runModal()
            return
        }
        onSave(cfg)
    }

    private static func encodeJSON(_ cfg: PairingConfig) throws -> String {
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data = try enc.encode(cfg)
        return String(data: data, encoding: .utf8) ?? ""
    }

    fileprivate func refreshQR() {
        let raw = configTextView.string.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty,
              let data = raw.data(using: .utf8),
              // Only render a QR if the JSON actually parses — otherwise the QR
              // would encode garbage and confuse the scanner.
              let _ = try? JSONDecoder().decode(PairingConfig.self, from: data)
        else {
            qrImageView.image = nil
            return
        }
        qrImageView.image = Self.makeQR(from: raw, pixelSize: 480)
    }

    private static func makeQR(from string: String, pixelSize: CGFloat) -> NSImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(string.utf8)
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

extension PairingView: NSTextViewDelegate {
    func textDidChange(_ notification: Notification) {
        refreshQR()
    }
}
