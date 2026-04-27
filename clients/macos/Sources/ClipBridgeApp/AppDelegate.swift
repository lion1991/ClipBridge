import AppKit
import ClipbridgeCore
import ServiceManagement
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusItem: NSStatusItem!
    private var coordinator: BridgeCoordinator?
    private var pairingWindow: NSWindow?
    private var imageWindow: NSWindow?
    private var autostartItem: NSMenuItem!
    /// Refreshes the "传输:..." menu item every 2s. The peer count comes
    /// from the Rust `Client` and changes asynchronously when peers come
    /// or go on the LAN, so a tight UI binding isn't worth the wiring.
    private var transportTimer: Timer?

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
        // Transport status: shows "局域网:N 设备" when peers are connected
        // via mDNS, "仅中继" otherwise. Refreshed by `transportTimer`.
        menu.addItem(NSMenuItem(title: "传输:仅中继", action: nil, keyEquivalent: ""))
        menu.addItem(NSMenuItem.separator())
        menu.addItem(makeItem("图片传输…", #selector(showImageTransfer), key: "i"))
        menu.addItem(makeItem("打开配对窗口…", #selector(showPairing), key: "p"))
        menu.addItem(makeItem("重置配对", #selector(resetPairing), key: ""))
        menu.addItem(NSMenuItem.separator())
        autostartItem = makeItem("开机自启", #selector(toggleAutostart), key: "")
        menu.addItem(autostartItem)
        menu.addItem(NSMenuItem.separator())
        menu.addItem(makeItem("退出 ClipBridge", #selector(quit), key: "q"))
        statusItem.menu = menu
        refreshAutostartItem()

        if let config = PairingStore.load() {
            startCoordinator(with: config)
        } else {
            applyStatus(.notPaired)
            showPairing()
        }

        // Start the transport poll. Common(.commonModes) keeps it firing
        // while the menu is open — otherwise NSMenu's tracking runloop
        // suspends our default-mode timer and the badge freezes mid-view.
        let timer = Timer(timeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.refreshTransportItem()
        }
        RunLoop.main.add(timer, forMode: .common)
        transportTimer = timer
        refreshTransportItem()
    }

    func applicationWillTerminate(_ notification: Notification) {
        transportTimer?.invalidate()
        transportTimer = nil
        coordinator?.stop()
    }

    private func refreshTransportItem() {
        let title: String
        if coordinator == nil {
            title = "传输:未配对"
        } else {
            let n = coordinator?.lanPeerCount ?? 0
            title = n == 0 ? "传输:仅中继" : "传输:局域网 \(n) 设备"
        }
        statusItem.menu?.item(at: 1)?.title = title
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
        coordinator?.clearImageHistory()
        coordinator?.stop()
        coordinator = nil
        PairingStore.clear()
        applyStatus(.notPaired)
        showPairing()
    }

    @objc private func showImageTransfer() {
        guard let coord = coordinator else {
            // No active client = pairing is missing or hasn't connected yet.
            // Nudge the user toward pairing first instead of opening an
            // empty image window with no way to send.
            let alert = NSAlert()
            alert.messageText = "未配对"
            alert.informativeText = "请先在「打开配对窗口…」完成扫码配对, 才能使用图片传输。"
            alert.alertStyle = .informational
            alert.addButton(withTitle: "打开配对窗口…")
            alert.addButton(withTitle: "取消")
            if alert.runModal() == .alertFirstButtonReturn {
                showPairing()
            }
            return
        }
        if imageWindow == nil {
            let win = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 520, height: 640),
                styleMask: [.titled, .closable, .resizable, .miniaturizable],
                backing: .buffered,
                defer: false
            )
            win.title = "ClipBridge · 图片传输"
            win.isReleasedWhenClosed = false
            win.center()
            imageWindow = win
        }
        guard let win = imageWindow else { return }
        win.contentViewController = NSHostingController(
            rootView: ImageTransferView(coordinator: coord)
        )
        NSApp.activate(ignoringOtherApps: true)
        win.makeKeyAndOrderFront(nil)
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    /// Toggle Login Item registration via SMAppService (macOS 13+). The
    /// system surfaces a confirmation in System Settings → General → Login
    /// Items the first time we register; subsequent toggles are silent.
    @objc private func toggleAutostart() {
        let service = SMAppService.mainApp
        do {
            if service.status == .enabled {
                try service.unregister()
            } else {
                try service.register()
            }
        } catch {
            let alert = NSAlert()
            alert.messageText = "无法修改开机自启设置"
            alert.informativeText = "\(error.localizedDescription)\n\n请在 系统设置 → 通用 → 登录项 中手动开关。"
            alert.alertStyle = .warning
            alert.addButton(withTitle: "好")
            alert.runModal()
        }
        refreshAutostartItem()
    }

    private func refreshAutostartItem() {
        // `.requiresApproval` shows up when the user has previously denied
        // login items for this bundle id; the checkmark is misleading there,
        // so we surface it as a separate label.
        switch SMAppService.mainApp.status {
        case .enabled:
            autostartItem.state = .on
            autostartItem.title = "开机自启"
        case .requiresApproval:
            autostartItem.state = .mixed
            autostartItem.title = "开机自启 · 待系统批准"
        case .notRegistered, .notFound:
            fallthrough
        @unknown default:
            autostartItem.state = .off
            autostartItem.title = "开机自启"
        }
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
