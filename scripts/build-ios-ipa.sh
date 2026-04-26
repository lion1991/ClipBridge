#!/usr/bin/env bash
# Build a TrollStore-installable IPA for ClipBridge.
#
# Output:
#   build/ios/ClipBridge.tipa
#
# Install:
#   AirDrop / iCloud / 一个文件分享方式把 IPA 弄到 iOS 设备 → 在
#   TrollStore App 里点"安装",TrollStore 会用任意 entitlements 签名
#   并安装到桌面。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

OUT="$ROOT/build/ios"
mkdir -p "$OUT"

# 1. Build clipbridge-core into an xcframework with device + sim slices,
#    plus the Swift glue. The Xcode project (clients/ios/project.yml) symlinks
#    the framework path so this needs to land under build/ios/.
echo "==> 1/4 building Rust xcframework"
XCF_OUT="$OUT"
XCF_HEADERS="$XCF_OUT/headers"
mkdir -p "$XCF_HEADERS"
PROFILE="release"

(
  cd "$ROOT"
  echo "    -> aarch64-apple-ios (device)"
  cargo build --$PROFILE -p clipbridge-core --target aarch64-apple-ios >/dev/null
  echo "    -> aarch64-apple-ios-sim (Apple Silicon simulator)"
  cargo build --$PROFILE -p clipbridge-core --target aarch64-apple-ios-sim >/dev/null

  # UniFFI bindgen reads a host dylib for metadata; cross-compiled .a files
  # can't be introspected, so we lean on the macOS dev build that's already
  # sitting in target/debug.
  echo "    -> swift bindings"
  cargo build -p clipbridge-core >/dev/null
  cargo run -p uniffi-bindgen -- generate \
    --library "target/debug/libclipbridge_core.dylib" \
    --language swift \
    --out-dir "$XCF_OUT" >/dev/null

  mv "$XCF_OUT/clipbridge_coreFFI.h" "$XCF_HEADERS/"
  mv "$XCF_OUT/clipbridge_coreFFI.modulemap" "$XCF_HEADERS/module.modulemap"

  rm -rf "$XCF_OUT/ClipbridgeCore.xcframework"
  xcodebuild -create-xcframework \
    -library "target/aarch64-apple-ios/$PROFILE/libclipbridge_core.a" \
    -headers "$XCF_HEADERS" \
    -library "target/aarch64-apple-ios-sim/$PROFILE/libclipbridge_core.a" \
    -headers "$XCF_HEADERS" \
    -output "$XCF_OUT/ClipbridgeCore.xcframework" >/dev/null

  # Keep the Xcode project's local Swift glue in sync.
  SWIFTPM_GLUE="$ROOT/clients/ios/Sources/ClipbridgeCore/clipbridge_core.swift"
  mkdir -p "$(dirname "$SWIFTPM_GLUE")"
  cp "$XCF_OUT/clipbridge_core.swift" "$SWIFTPM_GLUE"
)

cd "$ROOT/clients/ios"

# 2. Regenerate the Xcode project from project.yml in case anything
#    structural changed since last run (sources added, entitlements, etc.).
echo "==> 2/4 regenerating Xcode project"
xcodegen generate >/dev/null

# 3. Release build for device, no codesigning (TrollStore signs on device).
echo "==> 3/4 xcodebuild (release, iOS device)"
DERIVED="$ROOT/build/ios/.derived"
rm -rf "$DERIVED"
xcodebuild \
  -project ClipBridge.xcodeproj \
  -scheme ClipBridge \
  -sdk iphoneos \
  -configuration Release \
  -destination 'generic/platform=iOS' \
  -derivedDataPath "$DERIVED" \
  CODE_SIGNING_ALLOWED=NO \
  build >/dev/null

APP="$DERIVED/Build/Products/Release-iphoneos/ClipBridge.app"
[[ -d "$APP" ]] || { echo "expected $APP, build output not found" >&2; exit 1; }

# 3a. Embed entitlements with ldid.
#
# Why: CODE_SIGNING_ALLOWED=NO skips Xcode's entitlement embedding step, so
# the binary ships with no entitlements at all. TrollStore then signs with
# its default minimal entitlements (just get-task-allow + identifier), and
# our private TrollStore-only keys are LOST. Running ldid afterward bakes
# the full entitlements plist into the LC_CODE_SIGNATURE; TrollStore's own
# re-sign preserves them.
echo "==> 3a/4 embedding entitlements via ldid"
if ! command -v ldid >/dev/null 2>&1; then
  echo "ldid not found. Install: brew install ldid" >&2
  exit 1
fi
ldid "-S$ROOT/clients/ios/ClipBridge.entitlements" "$APP/ClipBridge"

# 4. Zip into the standard IPA layout (Payload/<App>.app/...).
echo "==> 4/4 packaging IPA"
STAGE="$OUT/.ipa-stage"
# .tipa = TrollStore IPA. Same zip-of-Payload layout as a regular IPA, but the
# extension lets iOS' "Open in…" sheet route taps to TrollStore directly
# instead of falling back to App Store / unsupported handlers.
TIPA="$OUT/ClipBridge.tipa"
rm -rf "$STAGE" "$TIPA"
mkdir -p "$STAGE/Payload"
cp -R "$APP" "$STAGE/Payload/"

( cd "$STAGE" && /usr/bin/zip -qr "$TIPA" Payload )

rm -rf "$STAGE" "$DERIVED"

SIZE=$(du -sh "$TIPA" | cut -f1)
echo
echo "✓ TIPA:      $TIPA ($SIZE)"
echo
echo "Install with TrollStore:"
echo "  1. AirDrop / 隔空投送 / 文件 App / iCloud Drive 把 TIPA 传到 iOS 设备"
echo "  2. iOS 上点 TIPA → 选择"在 TrollStore 中打开""
echo "  3. TrollStore 弹出来,点"安装""
echo "  4. 桌面出现 ClipBridge,点开扫码完成配对"
