import SwiftUI
import UIKit

@main
struct ClipBridgeApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) var appDelegate

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(appDelegate.coordinator)
        }
    }
}

/// AppDelegate owns the singleton `BridgeCoordinator` so the WebSocket
/// outlives any individual view's lifetime, and keeps it running across
/// scene transitions.
final class AppDelegate: NSObject, UIApplicationDelegate, ObservableObject {
    let coordinator = BridgeCoordinator.shared

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        coordinator.bootstrap()
        return true
    }

    func applicationDidBecomeActive(_ application: UIApplication) {
        coordinator.applicationDidBecomeActive()
    }

    func applicationDidEnterBackground(_ application: UIApplication) {
        coordinator.applicationDidEnterBackground()
    }
}
