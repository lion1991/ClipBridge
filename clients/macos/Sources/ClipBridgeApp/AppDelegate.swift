import AppKit
import ClipbridgeCore
import SwiftUI

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
        menu.addItem(NSMenuItem(title: "状态:未配对", action: nil, keyEquivalent: ""))
        menu.addItem(NSMenuItem.separator())
        menu.addItem(makeItem("打开配对窗口…", #selector(showPairing), key: "p"))
        menu.addItem(makeItem("重置配对", #selector(resetPairing), key: ""))
        menu.addItem(NSMenuItem.separator())
        menu.addItem(makeItem("退出 ClipBridge", #selector(quit), key: "q"))
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
                contentRect: NSRect(x: 0, y: 0, width: 460, height: 600),
                styleMask: [.titled, .closable, .resizable],
                backing: .buffered,
                defer: false
            )
            win.title = "ClipBridge"
            win.isReleasedWhenClosed = false
            win.center()
            pairingWindow = win
        }
        guard let win = pairingWindow else { return }

        let view = PairingScreen(
            existing: PairingStore.load(),
            onSave: { [weak self] config in
                PairingStore.save(config)
                self?.startCoordinator(with: config)
            },
            onReset: { [weak self] in
                self?.coordinator?.stop()
                self?.coordinator = nil
                PairingStore.clear()
                self?.applyStatus(.notPaired)
            }
        )
        win.contentViewController = NSHostingController(rootView: view)
        NSApp.activate(ignoringOtherApps: true)
        win.makeKeyAndOrderFront(nil)
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
        statusItem.menu?.item(at: 0)?.title = "状态:\(label(for: status))"
        guard let button = statusItem.button else { return }
        let image = NSImage(
            systemSymbolName: "doc.on.clipboard",
            accessibilityDescription: "ClipBridge"
        )
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
        case .notPaired: return "未配对"
        case .connecting: return "连接中…"
        case .connected: return "已连接"
        case .disconnected: return "已断开"
        case .error(let message): return "错误:\(message)"
        }
    }

    private func makeItem(_ title: String, _ selector: Selector, key: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: selector, keyEquivalent: key)
        item.target = self
        return item
    }
}
