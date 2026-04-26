import AppKit
import ClipbridgeCore

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusItem: NSStatusItem!
    private var coordinator: BridgeCoordinator?
    private var pairingWindow: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = statusItem.button {
            button.title = ""
            button.image = NSImage(
                systemSymbolName: "doc.on.clipboard",
                accessibilityDescription: "ClipBridge"
            )
            button.imagePosition = .imageOnly
        }

        let menu = NSMenu()
        menu.addItem(NSMenuItem(title: "Status: not paired", action: nil, keyEquivalent: ""))
        menu.addItem(NSMenuItem.separator())
        // NSMenuItem dispatches via the responder chain, but a status-bar menu
        // doesn't connect to AppDelegate by default — without an explicit
        // target, clicks silently no-op.
        menu.addItem(makeItem("Show pairing config…", #selector(showPairing), key: "p"))
        menu.addItem(makeItem("Reset pairing", #selector(resetPairing), key: ""))
        menu.addItem(NSMenuItem.separator())
        menu.addItem(makeItem("Quit ClipBridge", #selector(quit), key: "q"))
        statusItem.menu = menu

        if let config = PairingStore.load() {
            startCoordinator(with: config)
        } else {
            applyStatus(.notPaired)
            showPairing()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        coordinator?.stop()
    }

    @objc private func showPairing() {
        if pairingWindow == nil {
            let win = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 760, height: 460),
                styleMask: [.titled, .closable, .resizable],
                backing: .buffered,
                defer: false
            )
            win.title = "ClipBridge Pairing"
            // NSWindow defaults to releasing itself on close, which would leave
            // pairingWindow as a dangling reference the next time the menu item
            // fires. Hide instead of release so we can reopen.
            win.isReleasedWhenClosed = false
            win.center()
            pairingWindow = win
        }
        guard let win = pairingWindow else { return }

        let view = PairingView(
            existing: PairingStore.load(),
            onSave: { [weak self] config in
                PairingStore.save(config)
                self?.startCoordinator(with: config)
                self?.pairingWindow?.orderOut(nil)
            }
        )
        win.contentView = view
        NSApp.activate(ignoringOtherApps: true)
        win.makeKeyAndOrderFront(nil)
    }

    private func makeItem(_ title: String, _ selector: Selector, key: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: selector, keyEquivalent: key)
        item.target = self
        return item
    }

    @objc private func resetPairing() {
        coordinator?.stop()
        coordinator = nil
        PairingStore.clear()
        applyStatus(.notPaired)
        showPairing()
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    private func startCoordinator(with config: PairingConfig) {
        coordinator?.stop()
        let coord = BridgeCoordinator(config: config) { [weak self] status in
            self?.applyStatus(status)
        }
        coord.start()
        coordinator = coord
        applyStatus(.connecting)
    }

    private func applyStatus(_ status: BridgeStatus) {
        statusItem.menu?.item(at: 0)?.title = "Status: \(label(for: status))"
        guard let button = statusItem.button else { return }
        let image = NSImage(
            systemSymbolName: "doc.on.clipboard",
            accessibilityDescription: "ClipBridge"
        )
        // Template images get tinted black/white by the system based on the
        // current appearance. We only treat .connected that way; for other
        // states we want an explicit color, so isTemplate is turned off.
        switch status {
        case .connected:
            image?.isTemplate = true
            button.contentTintColor = nil
        case .connecting:
            image?.isTemplate = false
            button.contentTintColor = .systemOrange
        case .notPaired, .disconnected:
            image?.isTemplate = false
            button.contentTintColor = .secondaryLabelColor
        case .error:
            image?.isTemplate = false
            button.contentTintColor = .systemRed
        }
        button.image = image
    }

    private func label(for status: BridgeStatus) -> String {
        switch status {
        case .notPaired: return "not paired"
        case .connecting: return "connecting…"
        case .connected: return "connected"
        case .disconnected: return "disconnected"
        case .error(let message): return "error: \(message)"
        }
    }
}
