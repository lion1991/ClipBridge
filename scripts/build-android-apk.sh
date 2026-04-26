#!/usr/bin/env bash
# Full Android build: Rust core (.so per arch) + UniFFI Kotlin glue +
# Gradle assembly → signed APK ready to sideload.
#
# Output:
#   build/android/ClipBridge.apk
#
# Requires: cargo-ndk, rustup targets aarch64/armv7/x86_64-linux-android,
#           Android SDK + NDK (auto-detected from ~/Library/Android/sdk).
#
# Install on device:
#   adb install -r build/android/ClipBridge.apk
#   …or copy the APK to the phone and tap to install (allow "unknown sources").

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

OUT="$ROOT/build/android"
mkdir -p "$OUT"

JNI_LIBS="$ROOT/clients/android/app/src/main/jniLibs"
mkdir -p "$JNI_LIBS"

PROFILE="${PROFILE:-release}"
PROFILE_FLAG=""
if [[ "$PROFILE" == "release" ]]; then
  PROFILE_FLAG="--release"
fi

# 1. Cross-compile clipbridge-core for the three Android ABIs we ship.
#    minSdkVersion 24 (Android 7) is the floor for ChaCha20-Poly1305 hardware
#    acceleration on most devices and matches the app's compileSdk pick.
echo "==> 1/3 cargo ndk build (profile=$PROFILE)"
cargo ndk \
  --platform 24 \
  -t arm64-v8a \
  -t armeabi-v7a \
  -t x86_64 \
  -o "$JNI_LIBS" \
  build $PROFILE_FLAG -p clipbridge-core

# 2. UniFFI bindgen can't introspect Android ELF on a macOS host; use a host
#    dylib for metadata extraction (the bindings are arch-independent).
echo "==> 2/3 generating Kotlin bindings"
KOTLIN_OUT="$ROOT/clients/android/app/src/main/java"
mkdir -p "$KOTLIN_OUT"
cargo build -p clipbridge-core
cargo run -p uniffi-bindgen -- generate \
  --library "target/debug/libclipbridge_core.dylib" \
  --language kotlin \
  --out-dir "$KOTLIN_OUT"

# 3. Gradle assembly. We use `assembleDebug` rather than `assembleRelease`
#    because the release buildType has no signingConfig — release APKs would
#    come out unsigned and uninstallable. The debug buildType uses the
#    auto-generated debug keystore, which sideloads cleanly (same "personal
#    artifact" tier as the iOS TIPA / macOS ad-hoc-signed .app).
echo "==> 3/3 gradle assembleDebug"
cd "$ROOT/clients/android"
./gradlew --console=plain assembleDebug

APK_SRC="$ROOT/clients/android/app/build/outputs/apk/debug/app-debug.apk"
[[ -f "$APK_SRC" ]] || { echo "expected $APK_SRC, gradle output not found" >&2; exit 1; }

APK_OUT="$OUT/ClipBridge.apk"
cp "$APK_SRC" "$APK_OUT"

SIZE=$(du -sh "$APK_OUT" | cut -f1)
echo
echo "✓ APK:       $APK_OUT ($SIZE)"

# 4. If a device is plugged in (and authorised), install straight to it.
#    Skips silently when nothing's connected — useful for unattended/CI runs.
#    Set SKIP_INSTALL=1 to disable. With multiple devices, installs to each.
if [[ "${SKIP_INSTALL:-0}" == "1" ]]; then
  echo "✓ Install:   adb install -r $APK_OUT  (auto-install skipped: SKIP_INSTALL=1)"
else
  ADB="$(command -v adb || true)"
  if [[ -z "$ADB" && -x "$HOME/Library/Android/sdk/platform-tools/adb" ]]; then
    ADB="$HOME/Library/Android/sdk/platform-tools/adb"
  fi
  if [[ -z "$ADB" ]]; then
    echo "✓ Install:   adb install -r $APK_OUT  (adb not on PATH, skipping auto-install)"
  else
    # `adb devices` rows past the header look like: "<serial>\tdevice" for an
    # authorised, ready phone (vs. "unauthorized" / "offline" / "no permissions").
    # Use only fully-ready ones; bail quietly if there are none.
    DEVICES=$("$ADB" devices | awk 'NR>1 && $2=="device" {print $1}')
    if [[ -z "$DEVICES" ]]; then
      echo "✓ Install:   no adb device ready — connect a phone (USB debugging on) to auto-install"
    else
      echo "==> 4/4 installing to attached device(s)"
      while IFS= read -r serial; do
        [[ -n "$serial" ]] || continue
        echo "    -> $serial"
        # -r reinstall, -d allow downgrade in case the phone has a newer build
        "$ADB" -s "$serial" install -r -d "$APK_OUT"
      done <<<"$DEVICES"
    fi
  fi
fi
