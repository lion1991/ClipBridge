#!/usr/bin/env bash
# Build a macOS xcframework for ClipbridgeCore.
#
# Output:
#   build/macos/ClipbridgeCore.xcframework  — drop into Xcode project
#   build/macos/clipbridge_core.swift       — generated Swift glue

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OUT="$ROOT/build/macos"
HEADERS="$OUT/headers"
rm -rf "$OUT"
mkdir -p "$OUT" "$HEADERS"

PROFILE="release"
PROFILE_DIR="release"

echo "==> compiling for aarch64-apple-darwin"
cargo build --$PROFILE -p clipbridge-core --target aarch64-apple-darwin

echo "==> compiling for x86_64-apple-darwin"
cargo build --$PROFILE -p clipbridge-core --target x86_64-apple-darwin

echo "==> creating universal static lib"
mkdir -p "$OUT/universal"
lipo -create \
  "target/aarch64-apple-darwin/$PROFILE_DIR/libclipbridge_core.a" \
  "target/x86_64-apple-darwin/$PROFILE_DIR/libclipbridge_core.a" \
  -output "$OUT/universal/libclipbridge_core.a"

echo "==> generating swift bindings + header"
cargo run -p uniffi-bindgen -- generate \
  --library "target/aarch64-apple-darwin/$PROFILE_DIR/libclipbridge_core.dylib" \
  --language swift \
  --out-dir "$OUT"

# Move the C header + modulemap into the headers folder for the xcframework.
mv "$OUT/clipbridge_coreFFI.h" "$HEADERS/"
mv "$OUT/clipbridge_coreFFI.modulemap" "$HEADERS/module.modulemap"

echo "==> creating xcframework"
rm -rf "$OUT/ClipbridgeCore.xcframework"
xcodebuild -create-xcframework \
  -library "$OUT/universal/libclipbridge_core.a" \
  -headers "$HEADERS" \
  -output "$OUT/ClipbridgeCore.xcframework"

# Keep the SwiftPM target's source file in sync. The xcframework itself is
# symlinked under clients/macos but the .swift glue is a real copy and would
# otherwise drift.
SWIFTPM_GLUE="$ROOT/clients/macos/Sources/ClipbridgeCore/clipbridge_core.swift"
if [[ -f "$SWIFTPM_GLUE" || -d "$(dirname "$SWIFTPM_GLUE")" ]]; then
  cp "$OUT/clipbridge_core.swift" "$SWIFTPM_GLUE"
  echo "✓ Synced clients/macos Swift glue"
fi

echo
echo "✓ Built $OUT/ClipbridgeCore.xcframework"
echo "✓ Swift glue: $OUT/clipbridge_core.swift"
