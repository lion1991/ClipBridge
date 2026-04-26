#!/usr/bin/env bash
# Build a distributable ClipBridge.app bundle and wrap it in a DMG.
#
#   ./scripts/build-macos-app.sh                  # native arch only (fast)
#   ./scripts/build-macos-app.sh --universal      # arm64 + x86_64 via lipo
#   ./scripts/build-macos-app.sh --no-dmg         # skip the DMG step
#
# Output:
#   build/macos/ClipBridge.app
#   build/macos/ClipBridge.dmg          (drag-to-Applications installer)
#
# Install for end user:
#   double-click ClipBridge.dmg → drag ClipBridge.app onto Applications
#
# First launch from /Applications shows Gatekeeper warning (we only ad-hoc
# sign). Right-click → Open once and macOS remembers the choice.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

NATIVE_ARCH=$(uname -m)
ARCHS=("$NATIVE_ARCH")
MAKE_DMG=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch)
      ARCHS=("$2")
      shift 2
      ;;
    --universal)
      # SwiftPM's two `--arch` flags trip an `ld` assertion on linking ObjC
      # metadata, so we build each arch separately and lipo them together.
      ARCHS=(arm64 x86_64)
      shift
      ;;
    --no-dmg)
      MAKE_DMG=0
      shift
      ;;
    *)
      echo "unknown flag: $1" >&2
      exit 1
      ;;
  esac
done

OUT_DIR="$ROOT/build/macos"
APP="$OUT_DIR/ClipBridge.app"
BUNDLE_ID="com.clipbridge.mac"
VERSION="0.1.0"
MIN_OS="13.0"

cd "$ROOT"

echo "==> 1/4 rebuilding xcframework"
"$ROOT/scripts/build-macos-xcframework.sh" >/dev/null

echo "==> 2/4 swift build (release, archs: ${ARCHS[*]})"
cd "$ROOT/clients/macos"

PER_ARCH_BINS=()
for a in "${ARCHS[@]}"; do
  echo "    -> $a"
  swift build -c release --arch "$a" >/dev/null
  PER_ARCH_BINS+=("$ROOT/clients/macos/.build/${a}-apple-macosx/release/ClipBridgeApp")
done

EXEC="$ROOT/build/macos/ClipBridgeApp"
mkdir -p "$ROOT/build/macos"
if [[ ${#PER_ARCH_BINS[@]} -gt 1 ]]; then
  lipo -create "${PER_ARCH_BINS[@]}" -output "$EXEC"
else
  cp "${PER_ARCH_BINS[0]}" "$EXEC"
fi
[[ -f "$EXEC" ]] || { echo "executable not built" >&2; exit 1; }

echo "==> 3/4 assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$EXEC" "$APP/Contents/MacOS/ClipBridge"

cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>           <string>$BUNDLE_ID</string>
    <key>CFBundleName</key>                  <string>ClipBridge</string>
    <key>CFBundleDisplayName</key>           <string>ClipBridge</string>
    <key>CFBundleExecutable</key>            <string>ClipBridge</string>
    <key>CFBundleVersion</key>               <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>    <string>$VERSION</string>
    <key>CFBundlePackageType</key>           <string>APPL</string>
    <key>CFBundleSignature</key>             <string>????</string>
    <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
    <!-- LSUIElement = menu-bar-only app, no Dock icon, no main window. -->
    <key>LSUIElement</key>                   <true/>
    <key>LSMinimumSystemVersion</key>        <string>$MIN_OS</string>
    <key>NSHighResolutionCapable</key>       <true/>
    <key>NSHumanReadableCopyright</key>      <string>© 2026 ClipBridge</string>
    <key>NSSupportsAutomaticTermination</key> <false/>
    <key>NSSupportsSuddenTermination</key>   <true/>
</dict>
</plist>
PLIST

echo "==> 4/4 ad-hoc codesign"
codesign --force --deep --sign - "$APP" >/dev/null

DMG=""
if [[ $MAKE_DMG -eq 1 ]]; then
  echo "==> 5/5 building DMG"
  DMG="$OUT_DIR/ClipBridge.dmg"
  STAGE="$OUT_DIR/.dmg-stage"
  rm -rf "$STAGE" "$DMG"
  mkdir -p "$STAGE"
  cp -R "$APP" "$STAGE/"
  # Drag-to-Applications shortcut so the user can install with a flick.
  ln -s /Applications "$STAGE/Applications"

  # UDZO = zlib-compressed read-only disk image. About 1/3 the size of the
  # uncompressed bundle for a Rust+Swift binary like ours.
  hdiutil create \
    -volname "ClipBridge" \
    -srcfolder "$STAGE" \
    -fs HFS+ \
    -format UDZO \
    -ov \
    "$DMG" >/dev/null

  rm -rf "$STAGE"
fi

# Drop the staging executable used for assembly (the .app keeps its own copy).
rm -f "$EXEC"

SIZE=$(du -sh "$APP" | cut -f1)
ARCHS_STR=$(lipo -archs "$APP/Contents/MacOS/ClipBridge")

echo
echo "✓ Bundle:    $APP ($SIZE, $ARCHS_STR)"
[[ -n "$DMG" ]] && echo "✓ DMG:       $DMG ($(du -sh "$DMG" | cut -f1))"
echo "✓ BundleID:  $BUNDLE_ID"
echo
if [[ -n "$DMG" ]]; then
  echo "Open:        open '$DMG'"
  echo "             then drag ClipBridge to the Applications shortcut."
fi
echo "Try direct:  open '$APP'"
echo "Autostart:   System Settings → General → Login Items → + → ClipBridge.app"
echo "Note:        First launch may need Right-click → Open (Gatekeeper)"
