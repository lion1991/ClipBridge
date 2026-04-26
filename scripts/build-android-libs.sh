#!/usr/bin/env bash
# Build clipbridge-core for Android NDK targets and copy .so files into
# clients/android/app/src/main/jniLibs.
#
# Requires: cargo-ndk, rustup targets aarch64/armv7/x86_64-linux-android,
#           Android NDK (auto-detected from ~/Library/Android/sdk/ndk).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
  if [[ -d "$HOME/Library/Android/sdk/ndk" ]]; then
    NDK_VER="$(ls -1 "$HOME/Library/Android/sdk/ndk" | sort -V | tail -1)"
    export ANDROID_NDK_HOME="$HOME/Library/Android/sdk/ndk/$NDK_VER"
    echo "ANDROID_NDK_HOME=$ANDROID_NDK_HOME"
  else
    echo "Set ANDROID_NDK_HOME or install NDK at ~/Library/Android/sdk/ndk" >&2
    exit 1
  fi
fi

JNI_LIBS="$ROOT/clients/android/app/src/main/jniLibs"
mkdir -p "$JNI_LIBS"

PROFILE="${PROFILE:-release}"
PROFILE_FLAG=""
PROFILE_DIR="debug"
if [[ "$PROFILE" == "release" ]]; then
  PROFILE_FLAG="--release"
  PROFILE_DIR="release"
fi

echo "==> cargo ndk build (profile=$PROFILE)"
# minSdkVersion 24 (Android 7) is the floor for ChaCha20-Poly1305 hardware
# acceleration on most devices and matches the app's compileSdk pick.
cargo ndk \
  --platform 24 \
  -t arm64-v8a \
  -t armeabi-v7a \
  -t x86_64 \
  -o "$JNI_LIBS" \
  build $PROFILE_FLAG -p clipbridge-core

echo "==> generating Kotlin bindings"
KOTLIN_OUT="$ROOT/clients/android/app/src/main/java"
mkdir -p "$KOTLIN_OUT"
# uniffi-bindgen can't introspect Android ELF on macOS host; use a host
# dylib for metadata extraction (the bindings are arch-independent).
cargo build -p clipbridge-core
cargo run -p uniffi-bindgen -- generate \
  --library "target/debug/libclipbridge_core.dylib" \
  --language kotlin \
  --out-dir "$KOTLIN_OUT"

echo
echo "✓ .so libs in $JNI_LIBS"
echo "✓ Kotlin glue in $KOTLIN_OUT"
