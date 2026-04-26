#!/usr/bin/env bash
# Build an iOS-compatible xcframework for ClipbridgeCore.
#
# Output:
#   build/ios/ClipbridgeCore.xcframework — slices for iOS device + simulator
#   build/ios/clipbridge_core.swift      — generated Swift glue
#
# Used by clients/ios/project.yml as a binary dependency.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OUT="$ROOT/build/ios"
HEADERS="$OUT/headers"
rm -rf "$OUT"
mkdir -p "$OUT" "$HEADERS"

PROFILE="release"

echo "==> compiling for aarch64-apple-ios (device)"
cargo build --$PROFILE -p clipbridge-core --target aarch64-apple-ios

echo "==> compiling for aarch64-apple-ios-sim (Apple Silicon simulator)"
cargo build --$PROFILE -p clipbridge-core --target aarch64-apple-ios-sim

echo "==> generating swift bindings + header"
# UniFFI bindgen reads a host dylib for metadata; cross-compiled .a files
# can't be introspected, so we lean on the macOS dev build that's already
# sitting in target/debug.
cargo build -p clipbridge-core
cargo run -p uniffi-bindgen -- generate \
  --library "target/debug/libclipbridge_core.dylib" \
  --language swift \
  --out-dir "$OUT"

mv "$OUT/clipbridge_coreFFI.h" "$HEADERS/"
mv "$OUT/clipbridge_coreFFI.modulemap" "$HEADERS/module.modulemap"

echo "==> creating xcframework"
rm -rf "$OUT/ClipbridgeCore.xcframework"
xcodebuild -create-xcframework \
  -library "target/aarch64-apple-ios/$PROFILE/libclipbridge_core.a" \
  -headers "$HEADERS" \
  -library "target/aarch64-apple-ios-sim/$PROFILE/libclipbridge_core.a" \
  -headers "$HEADERS" \
  -output "$OUT/ClipbridgeCore.xcframework"

# Keep the Xcode project's local Swift glue in sync.
SWIFTPM_GLUE="$ROOT/clients/ios/Sources/ClipbridgeCore/clipbridge_core.swift"
mkdir -p "$(dirname "$SWIFTPM_GLUE")"
cp "$OUT/clipbridge_core.swift" "$SWIFTPM_GLUE"

echo
echo "✓ Built $OUT/ClipbridgeCore.xcframework"
echo "✓ Swift glue: $SWIFTPM_GLUE"
