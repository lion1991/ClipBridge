import Foundation
import Network

/// Forces iOS to evaluate (and prompt for) the "Local Network" privacy
/// permission for this app.
///
/// Why this exists: our LAN transport in the Rust core (see `core/src/lan.rs`)
/// uses pure-Rust `mdns-sd`, which talks raw multicast UDP via BSD sockets.
/// Apple's privacy framework only triggers the local-network grant prompt
/// when the app uses high-level Bonjour APIs — `NWBrowser`, `NetService`,
/// `dns-sd`. Raw socket multicast is *silently dropped* until the user
/// has granted the permission for some other reason. Result: without this
/// primer the user is never asked, the system blocks our multicast, and
/// LAN discovery silently never works no matter how perfect Info.plist is.
///
/// Solution: spin up a no-op `NWBrowser` for our service type. Just creating
/// and starting it is enough to make iOS evaluate the entitlement, which
/// shows the system prompt on first launch. After the user grants it the
/// `mdns-sd` raw sockets in the Rust core start receiving multicast
/// normally. We don't need the browser's actual results — `mdns-sd` is the
/// authoritative discovery path.
///
/// We keep the browser running for the app lifetime: the cost is one
/// idle UDP listener, and that ensures the grant stays evaluated whenever
/// the user toggles network conditions.
final class LocalNetworkPrimer {
    private var browser: NWBrowser?

    func start() {
        guard browser == nil else { return }
        let descriptor = NWBrowser.Descriptor.bonjour(
            type: "_clipbridge._tcp",
            domain: nil
        )
        let params = NWParameters()
        // Re-use the port if anything else binds it — irrelevant for browse,
        // but harmless and matches the more permissive defaults the Rust
        // side uses.
        params.includePeerToPeer = true
        let b = NWBrowser(for: descriptor, using: params)
        b.stateUpdateHandler = { state in
            // Logged at debug level only — this is a permission probe, not
            // a feature path. The actual discovery + transport lives in
            // the Rust core.
            #if DEBUG
            print("[LocalNetworkPrimer] state: \(state)")
            #endif
        }
        b.browseResultsChangedHandler = { _, _ in
            // We deliberately ignore results — `mdns-sd` is the source of
            // truth for the actual peer connections.
        }
        b.start(queue: .main)
        browser = b
    }

    func stop() {
        browser?.cancel()
        browser = nil
    }
}
