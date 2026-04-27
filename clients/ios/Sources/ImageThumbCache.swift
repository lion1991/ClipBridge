import UIKit

/// Tiny in-memory cache for image previews shown in the recent-clips cards.
///
/// Keyed by clip timestamp (UInt64 → NSNumber) so both the send and receive
/// paths can stash a UIImage right when they have the bytes in hand: the
/// send path caches before handing off to Rust, the receive path caches
/// right after `fetchImage` returns. The UI layer just looks up by ts.
///
/// We don't persist across app launches — main-app history itself is in
/// memory only (see BridgeCoordinator.recentClips), so a thumbnail cache
/// that survived would just dangle. Bounded by NSCache's defaults plus an
/// explicit cost limit so a flurry of large images can't push the app to
/// jetsam.
final class ImageThumbCache {
    static let shared = ImageThumbCache()

    private let cache: NSCache<NSNumber, UIImage> = {
        let c = NSCache<NSNumber, UIImage>()
        c.countLimit = 64
        // Cost = approx pixel bytes. 32MB is enough for a handful of
        // full-resolution screenshots; SwiftUI downscales for display.
        c.totalCostLimit = 32 * 1024 * 1024
        return c
    }()

    func image(forTs ts: UInt64) -> UIImage? {
        cache.object(forKey: NSNumber(value: ts))
    }

    func store(_ image: UIImage, forTs ts: UInt64) {
        let cost = Int(image.size.width * image.size.height * 4)
        cache.setObject(image, forKey: NSNumber(value: ts), cost: cost)
    }
}
