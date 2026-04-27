import UIKit

/// In-memory cache for the recent-clips cards.
///
/// Keyed by clip timestamp (UInt64) so both the send and receive paths can
/// stash a UIImage right when they have the bytes in hand. Two parts per
/// entry:
///
///  - **Thumbnail**: pre-rendered ≤96pt UIImage for the row preview.
///  - **Full bytes**: the original PNG `Data` for tap-to-paste fidelity.
///
/// Storage is a manual LRU dictionary, *not* `NSCache`. NSCache evicts
/// opaquely under memory pressure regardless of cost limits, which made
/// tap-to-paste look broken when bytes vanished between receive and tap —
/// the row's `setData` call became a silent no-op. With a fixed 16-entry
/// LRU we know exactly how much we hold (a few MB) and that nothing
/// disappears under us.
final class ImageThumbCache {
    static let shared = ImageThumbCache()

    /// Pre-rendered thumbnail max side. 96pt covers the 56pt row preview at
    /// up to ~1.7x oversampling without retina blur.
    private static let thumbnailMaxSide: CGFloat = 96
    private static let capacity = 16

    private struct Entry {
        let thumbnail: UIImage
        let bytes: Data
    }

    private let lock = NSLock()
    private var entries: [UInt64: Entry] = [:]
    /// Insertion order; oldest first. Updated on every store so the LRU
    /// drops the longest-untouched entry once we hit `capacity`.
    private var order: [UInt64] = []

    func thumbnail(forTs ts: UInt64) -> UIImage? {
        lock.lock(); defer { lock.unlock() }
        return entries[ts]?.thumbnail
    }

    func fullData(forTs ts: UInt64) -> Data? {
        lock.lock(); defer { lock.unlock() }
        return entries[ts]?.bytes
    }

    /// Store both reps. `image` is downscaled once on the caller's thread
    /// so SwiftUI never blocks on a multi-MB decode.
    func store(image: UIImage, bytes: Data, forTs ts: UInt64) {
        let thumb = downscale(image, maxSide: Self.thumbnailMaxSide)
        lock.lock(); defer { lock.unlock() }
        if entries[ts] == nil {
            order.append(ts)
        } else {
            // Refresh recency: move ts to the end of the order list.
            order.removeAll { $0 == ts }
            order.append(ts)
        }
        entries[ts] = Entry(thumbnail: thumb, bytes: bytes)
        while order.count > Self.capacity {
            let oldest = order.removeFirst()
            entries.removeValue(forKey: oldest)
        }
    }

    private func downscale(_ image: UIImage, maxSide: CGFloat) -> UIImage {
        let w = image.size.width
        let h = image.size.height
        let scale = min(maxSide / max(w, 1), maxSide / max(h, 1), 1.0)
        if scale >= 1.0 { return image }
        let target = CGSize(width: w * scale, height: h * scale)
        let format = UIGraphicsImageRendererFormat.default()
        format.opaque = false
        format.scale = UIScreen.main.scale
        let renderer = UIGraphicsImageRenderer(size: target, format: format)
        return renderer.image { _ in
            image.draw(in: CGRect(origin: .zero, size: target))
        }
    }
}
