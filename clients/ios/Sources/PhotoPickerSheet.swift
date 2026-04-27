import PhotosUI
import SwiftUI
import UIKit

/// SwiftUI wrapper around `PHPickerViewController` (iOS 14+). Picks one or
/// more images from the user's library without prompting for full Photos
/// access — the picker runs in a separate process and only hands us back
/// the bytes for items the user explicitly tapped.
///
/// `selectionLimit = 0` would be unlimited, but we cap at 9 to match what
/// most chat apps do — sending a hundred photos in one go is more likely
/// a misclick than intent, and would saturate the relay's blob budget.
struct PhotoPickerSheet: UIViewControllerRepresentable {
    @Binding var isPresented: Bool
    let onPick: (Data) -> Void

    func makeUIViewController(context: Context) -> PHPickerViewController {
        var config = PHPickerConfiguration(photoLibrary: .shared())
        config.filter = .images
        config.selectionLimit = 9
        // .compatible asks PhotoKit to transcode HEIC → JPEG when handing
        // bytes back, so we don't have to ship a HEIC decoder to remote
        // peers. Our pipeline still re-encodes to PNG, but starting from
        // JPEG makes that path more reliable across iOS versions.
        config.preferredAssetRepresentationMode = .compatible

        let vc = PHPickerViewController(configuration: config)
        vc.delegate = context.coordinator
        return vc
    }

    func updateUIViewController(_ uiViewController: PHPickerViewController, context: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator: NSObject, PHPickerViewControllerDelegate {
        let parent: PhotoPickerSheet
        init(_ parent: PhotoPickerSheet) { self.parent = parent }

        func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
            parent.isPresented = false
            for result in results {
                let provider = result.itemProvider
                // image identifier covers HEIC/JPEG/PNG/etc.; the picker
                // returns one Data per item via `loadDataRepresentation`.
                let typeId = "public.image"
                guard provider.hasItemConformingToTypeIdentifier(typeId) else { continue }
                provider.loadDataRepresentation(forTypeIdentifier: typeId) { data, _ in
                    guard let data else { return }
                    DispatchQueue.main.async {
                        self.parent.onPick(data)
                    }
                }
            }
        }
    }
}

/// Save raw image bytes to the user's photo library. iOS prompts for
/// `NSPhotoLibraryAddUsageDescription` permission on first call; subsequent
/// saves are silent. Failures (denied, decode error, write error) surface
/// via `onResult` so the caller can show a toast.
enum PhotoSaver {
    static func saveToCameraRoll(_ bytes: Data, onResult: @escaping (Bool) -> Void) {
        guard let image = UIImage(data: bytes) else {
            onResult(false)
            return
        }
        // The C-style selector callback dance below is the only way to get
        // a completion signal from `UIImageWriteToSavedPhotosAlbum`; we
        // wrap it in a tiny NSObject so the selector targets a real Obj-C
        // method, then bridge the bool back to Swift.
        let proxy = SaveProxy(onResult: onResult)
        // Retain the proxy until the callback fires — otherwise it's
        // released and the selector lands on a dangling pointer.
        SaveProxy.pending.append(proxy)
        UIImageWriteToSavedPhotosAlbum(
            image,
            proxy,
            #selector(SaveProxy.image(_:didFinishSavingWithError:contextInfo:)),
            nil,
        )
    }
}

/// Internal — see `PhotoSaver.saveToCameraRoll`. Lifetime extended via the
/// `pending` static array; the callback removes its own entry.
private final class SaveProxy: NSObject {
    static var pending: [SaveProxy] = []
    let onResult: (Bool) -> Void
    init(onResult: @escaping (Bool) -> Void) {
        self.onResult = onResult
    }
    @objc func image(_ image: UIImage, didFinishSavingWithError error: Error?, contextInfo: UnsafeRawPointer) {
        DispatchQueue.main.async {
            self.onResult(error == nil)
            SaveProxy.pending.removeAll { $0 === self }
        }
    }
}
