#!/usr/bin/env bash
# Build a TrollStore-installable IPA for ClipBridge.
#
# Output:
#   build/ios/ClipBridge.ipa
#
# Install:
#   AirDrop / iCloud / 一个文件分享方式把 IPA 弄到 iOS 设备 → 在
#   TrollStore App 里点"安装",TrollStore 会用任意 entitlements 签名
#   并安装到桌面。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/clients/ios"

OUT="$ROOT/build/ios"
mkdir -p "$OUT"

# 1. Make sure the xcframework is up to date.
echo "==> 1/4 building Rust xcframework"
"$ROOT/scripts/build-ios-xcframework.sh" >/dev/null

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

# 4. Zip into the standard IPA layout (Payload/<App>.app/...).
echo "==> 4/4 packaging IPA"
STAGE="$OUT/.ipa-stage"
IPA="$OUT/ClipBridge.ipa"
rm -rf "$STAGE" "$IPA"
mkdir -p "$STAGE/Payload"
cp -R "$APP" "$STAGE/Payload/"

( cd "$STAGE" && /usr/bin/zip -qr "$IPA" Payload )

rm -rf "$STAGE" "$DERIVED"

SIZE=$(du -sh "$IPA" | cut -f1)
echo
echo "✓ IPA:       $IPA ($SIZE)"
echo
echo "Install with TrollStore:"
echo "  1. AirDrop / 隔空投送 / 文件 App / iCloud Drive 把 IPA 传到 iOS 设备"
echo "  2. iOS 上点 IPA → 选择"在 TrollStore 中打开""
echo "  3. TrollStore 弹出来,点"安装""
echo "  4. 桌面出现 ClipBridge,点开扫码完成配对"
