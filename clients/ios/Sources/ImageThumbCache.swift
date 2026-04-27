import UIKit

/// In-memory cache for the recent-clips cards.
///
/// Keyed by clip timestamp (UInt64 → NSNumber) so both the send and receive
/// paths can stash a UIImage right when they have the bytes in hand. Two
/// independent stores per entry:
///
///  - **Thumbnail**: pre-rendered small bitmap used by the row's preview
///    image. Tiny (≤ a few KB each) so the budget can hold a long history
///    without NSCache evicting under memory pressure.
///  - **Full data**: the original PNG bytes for tap-to-paste. Larger (a
///    full-screen iPhone screenshot is ~14MB decoded, ~150KB compressed),
///    so we stash the *bytes* not the decoded UIImage — much cheaper, and
///    the user pays the decode only on tap.
///
/// Splitting the two means a flurry of big screenshots can't evict each
/// other's thumbnails, which previously caused recent rows to render with
/// the gray placeholder seconds after they appeared.
final class ImageThumbCache {
    static let shared = ImageThumbCache()

    /// 96pt at @3x = 288×288 = ~330 KB per entry. 16MB budget = ~50 entries
    /// of headroom, far more than the 6 rows we ever show.
    private static let thumbnailMaxSide: CGFloat = 96

    private let thumbnails: NSCache<NSNumber, UIImage> = {
        let c = NSCache<NSNumber, UIImage>()
        c.countLimit = 64
        c.totalCostLimit = 16 * 1024 * 1024
        return c
    }()

    private let fullData: NSCache<NSNumber, NSData> = {
        let c = NSCache<NSNumber, NSData>()
        c.countLimit = 32
        // 128MB — enough for a handful of full-screen screenshots without
        // NSCache eagerly evicting them. iOS still discards under real
        // memory pressure regardless of this limit.
        c.totalCostLimit = 128 * 1024 * 1024
        return c
    }()

    func thumbnail(forTs ts: UInt64) -> UIImage? {
        thumbnails.object(forKey: NSNumber(value: ts))
    }

    func fullData(forTs ts: UInt64) -> Data? {
        fullData.object(forKey: NSNumber(value: ts)) as Data?
    }

    /// Store both representations. `image` is decoded once on the caller's
    /// thread (presumably already a hot UIImage from the read or fetch
    /// path) and downscaled to a thumbnail; `bytes` is held verbatim for
    /// re-paste fidelity.
    func store(image: UIImage, bytes: Data, forTs ts: UInt64) {
        let key = NSNumber(value: ts)
        let thumb = downscale(image, maxSide: Self.thumbnailMaxSide)
        let thumbCost = Int(thumb.size.width * thumb.size.height * 4 * thumb.scale * thumb.scale)
        thumbnails.setObject(thumb, forKey: key, cost: thumbCost)
        fullData.setObject(bytes as NSData, forKey: key, cost: bytes.count)
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
