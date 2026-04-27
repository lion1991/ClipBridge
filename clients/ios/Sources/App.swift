import SwiftUI
import UIKit

@main
struct ClipBridgeApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) var appDelegate
    // SwiftUI's App + WindowGroup is scene-based, so the classic
    // UIApplicationDelegate methods (applicationDidBecomeActive /
    // applicationDidEnterBackground) are unreliable — UIKit may route
    // active/inactive transitions to a UIScene delegate instead, and our
    // delegate's methods then never fire. Watching `scenePhase` is the
    // SwiftUI-blessed way to observe foreground/background transitions
    // and works regardless of which lifecycle UIKit picks.
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(appDelegate.coordinator)
        }
        .onChange(of: scenePhase) { phase in
            switch phase {
            case .active:
                appDelegate.coordinator.applicationDidBecomeActive()
            case .background:
                appDelegate.coordinator.applicationDidEnterBackground()
            case .inactive:
                // Brief intermediate state during transitions / Control
                // Center pull-down. Don't tear down the client just for
                // these — wait for a real .background.
                break
            @unknown default:
                break
            }
        }
    }
}

final class AppDelegate: NSObject, UIApplicationDelegate, ObservableObject {
    let coordinator = BridgeCoordinator.shared

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        coordinator.bootstrap()
        return true
    }
}
