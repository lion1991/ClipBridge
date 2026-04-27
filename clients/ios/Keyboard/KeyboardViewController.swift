import UIKit
// `clipbridge_core.swift` (Rust glue) and `PairingConfig.swift` (PairingStore,
// PairingConfig, DEFAULT_RELAY_URL) are compiled into this same target via
// project.yml's `sources:` list, so all those types are in scope without an
// explicit import.

/// Custom keyboard surface for ClipBridge.
///
/// Lifecycle: iOS instantiates this VC each time the user invokes our
/// keyboard in any app. While the view is on screen the runloop runs, so
/// our `KeyboardSync` (Rust client + 1Hz pasteboard polling) has runtime.
/// When the view is dismissed we tear everything down — no background hack
/// needed; we run only when the user is actually typing.
final class KeyboardViewController: UIInputViewController {

    private let sync = KeyboardSync()
    private let statusLabel = UILabel()
    private let previewLabel = UILabel()
    private let pasteButton = UIButton(type: .system)
    private let nextKeyboardButton = UIButton(type: .system)
    private let copySelectionButton = UIButton(type: .system)

    override func viewDidLoad() {
        super.viewDidLoad()
        buildUI()
        sync.delegate = self
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        // `start` is idempotent — if iOS reuses the same VC across keyboard
        // sessions (it can), we won't double-connect.
        sync.start()
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        sync.stop()
    }

    // MARK: - UI

    private func buildUI() {
        view.backgroundColor = UIColor.secondarySystemBackground

        statusLabel.font = .systemFont(ofSize: 12, weight: .medium)
        statusLabel.textColor = .secondaryLabel
        statusLabel.text = "未启动"

        previewLabel.font = .systemFont(ofSize: 14)
        previewLabel.textColor = .label
        previewLabel.numberOfLines = 2
        previewLabel.text = "等待最新剪切板…"

        var pasteCfg = UIButton.Configuration.filled()
        pasteCfg.title = "粘贴最新"
        pasteCfg.cornerStyle = .large
        pasteButton.configuration = pasteCfg
        pasteButton.addTarget(self, action: #selector(handlePaste), for: .touchUpInside)
        pasteButton.isEnabled = false

        var copyCfg = UIButton.Configuration.tinted()
        copyCfg.title = "复制选中"
        copyCfg.cornerStyle = .large
        copySelectionButton.configuration = copyCfg
        copySelectionButton.addTarget(self, action: #selector(handleCopySelection), for: .touchUpInside)

        nextKeyboardButton.setTitle("🌐", for: .normal)
        nextKeyboardButton.titleLabel?.font = .systemFont(ofSize: 22)
        // `handleInputModeList(from:with:)` is the SDK-provided behavior:
        // tap = advance to next, long-press = show keyboard list. Wiring it
        // to .allTouchEvents is the canonical idiom.
        nextKeyboardButton.addTarget(
            self,
            action: #selector(handleInputModeList(from:with:)),
            for: .allTouchEvents
        )

        let topRow = UIStackView(arrangedSubviews: [statusLabel])
        topRow.axis = .horizontal
        topRow.alignment = .center
        topRow.distribution = .fill

        let actionRow = UIStackView(arrangedSubviews: [copySelectionButton, pasteButton])
        actionRow.axis = .horizontal
        actionRow.spacing = 10
        actionRow.distribution = .fillEqually

        let bottomRow = UIStackView(arrangedSubviews: [nextKeyboardButton, UIView()])
        bottomRow.axis = .horizontal
        bottomRow.alignment = .center

        let stack = UIStackView(arrangedSubviews: [topRow, previewLabel, actionRow, bottomRow])
        stack.axis = .vertical
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)

        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: view.topAnchor, constant: 10),
            stack.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 14),
            stack.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -14),
            stack.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -10),
            pasteButton.heightAnchor.constraint(equalToConstant: 44),
            copySelectionButton.heightAnchor.constraint(equalToConstant: 44),
            nextKeyboardButton.widthAnchor.constraint(equalToConstant: 44),
            nextKeyboardButton.heightAnchor.constraint(equalToConstant: 36),
        ])
    }

    @objc private func handlePaste() {
        guard let text = sync.latestRemoteClip else { return }
        // textDocumentProxy.insertText goes straight into the focused field
        // — no UIPasteboard round-trip needed and no "粘贴" prompt.
        textDocumentProxy.insertText(text)
    }

    @objc private func handleCopySelection() {
        // textDocumentProxy.selectedText is iOS 16+ but we only run on
        // iOS 15+ targets that may or may not honor it. Fall back to the
        // current pasteboard contents; the watchdog in `sync` will pick up
        // any change the user just made via the system Edit menu.
        if #available(iOS 16.0, *), let s = textDocumentProxy.selectedText, !s.isEmpty {
            UIPasteboard.general.string = s
        }
        sync.flushPasteboard()
    }
}

// MARK: - KeyboardSyncDelegate

extension KeyboardViewController: KeyboardSyncDelegate {
    func keyboardSync(_ s: KeyboardSync, didUpdateStatus text: String) {
        statusLabel.text = text
    }

    func keyboardSync(_ s: KeyboardSync, didReceiveClipPreview text: String) {
        previewLabel.text = "最新: \(text)"
        pasteButton.isEnabled = true
    }
}
