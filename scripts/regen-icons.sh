#!/usr/bin/env bash
# Re-render every platform's icon set from assets/icon.svg, in place.
#
# Targets:
#   iOS     — clients/ios/Resources/Assets.xcassets/AppIcon.appiconset/icon-1024.png
#             (RGB, no alpha — Apple rejects icons with alpha channels)
#   Android — clients/android/app/src/main/res/mipmap-{m,h,xh,xxh,xxxh}dpi/ic_launcher.png
#   macOS   — clients/macos/Resources/AppIcon.icns
#   Windows — clients/windows/icons/{32x32,64x64,128x128,128x128@2x,icon}.png
#             clients/windows/icons/icon.ico  (multi-size, generated via sips→ImageMagick or tauri-cli)
#
# Run on macOS (uses sips + iconutil). Requires librsvg:
#     brew install librsvg
# .ico generation needs either ImageMagick (`brew install imagemagick`) or
# `cargo tauri icon` available; the script falls back gracefully and warns.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/assets/icon.svg"

[[ -f "$SRC" ]] || { echo "missing $SRC" >&2; exit 1; }
command -v rsvg-convert >/dev/null || { echo "rsvg-convert not found — brew install librsvg" >&2; exit 1; }
command -v sips >/dev/null         || { echo "sips not found (macOS only script)" >&2; exit 1; }
command -v iconutil >/dev/null     || { echo "iconutil not found (macOS only script)" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Render the SVG once at full resolution; downsample with sips for crisper
# small-size results than re-rasterising the SVG at each tiny size.
echo "==> rendering master 1024×1024 PNG"
rsvg-convert -w 1024 -h 1024 "$SRC" -o "$TMP/master.png"

# render_size <px> <out>
render_size() {
  local px="$1" out="$2"
  rsvg-convert -w "$px" -h "$px" "$SRC" -o "$out"
}

# ---------- iOS ----------
echo "==> iOS"
IOS_DIR="$ROOT/clients/ios/Resources/Assets.xcassets/AppIcon.appiconset"
mkdir -p "$IOS_DIR"
# App Store / iOS reject AppIcons with an alpha channel. The SVG bg fully
# covers the canvas, so there is nothing semantically transparent to flatten;
# we just have to drop the alpha plane. sips can't toggle it on PNG, but
# round-tripping through JPEG (which has no alpha) and back to PNG yields a
# true RGB PNG. Quality 100 keeps the synthetic-graphic icon visually lossless.
rsvg-convert -w 1024 -h 1024 -b "#1d4ed8" "$SRC" -o "$TMP/ios-rgba.png"
sips -s format jpeg -s formatOptions 100 "$TMP/ios-rgba.png" --out "$TMP/ios.jpg" >/dev/null
sips -s format png "$TMP/ios.jpg" --out "$IOS_DIR/icon-1024.png" >/dev/null

# ---------- Android ----------
echo "==> Android"
ANDROID_RES="$ROOT/clients/android/app/src/main/res"
# Standard launcher densities (px): mdpi=48 hdpi=72 xhdpi=96 xxhdpi=144 xxxhdpi=192.
declare -a A_DENSITIES=("mdpi:48" "hdpi:72" "xhdpi:96" "xxhdpi:144" "xxxhdpi:192")
for entry in "${A_DENSITIES[@]}"; do
  density="${entry%:*}"
  px="${entry#*:}"
  dir="$ANDROID_RES/mipmap-$density"
  mkdir -p "$dir"
  render_size "$px" "$dir/ic_launcher.png"
  # roundIcon — Android masks ic_launcher anyway on API 26+, but pre-26
  # devices honour the round variant separately. Same source PNG works.
  cp "$dir/ic_launcher.png" "$dir/ic_launcher_round.png"
done

# ---------- macOS ----------
echo "==> macOS"
MAC_RES="$ROOT/clients/macos/Resources"
mkdir -p "$MAC_RES"
ICONSET="$TMP/AppIcon.iconset"
mkdir -p "$ICONSET"
# Apple's required iconset sizes: 16, 32, 64, 128, 256, 512, 1024 + @2x variants.
declare -a MAC_SIZES=(\
  "16:icon_16x16.png" \
  "32:icon_16x16@2x.png" \
  "32:icon_32x32.png" \
  "64:icon_32x32@2x.png" \
  "128:icon_128x128.png" \
  "256:icon_128x128@2x.png" \
  "256:icon_256x256.png" \
  "512:icon_256x256@2x.png" \
  "512:icon_512x512.png" \
  "1024:icon_512x512@2x.png" \
)
for entry in "${MAC_SIZES[@]}"; do
  px="${entry%:*}"
  name="${entry#*:}"
  render_size "$px" "$ICONSET/$name"
done
iconutil -c icns "$ICONSET" -o "$MAC_RES/AppIcon.icns"

# ---------- Windows ----------
echo "==> Windows"
WIN_DIR="$ROOT/clients/windows/icons"
mkdir -p "$WIN_DIR"
render_size 32  "$WIN_DIR/32x32.png"
render_size 64  "$WIN_DIR/64x64.png"
render_size 128 "$WIN_DIR/128x128.png"
render_size 256 "$WIN_DIR/128x128@2x.png"
render_size 512 "$WIN_DIR/icon.png"
# .icns is reused by Tauri's macOS bundle target; emit one too for symmetry
# even though we ship Windows here.
cp "$MAC_RES/AppIcon.icns" "$WIN_DIR/icon.icns"

# Multi-size .ico — sips can't write ICO. Try ImageMagick first, then fall
# back to `cargo tauri icon`. If neither is available, the existing icon.ico
# is left alone (it tracks the same SVG, so only drifts if the SVG is edited).
if command -v magick >/dev/null; then
  magick "$WIN_DIR/32x32.png" "$WIN_DIR/64x64.png" "$WIN_DIR/128x128.png" \
         "$WIN_DIR/128x128@2x.png" "$WIN_DIR/icon.png" "$WIN_DIR/icon.ico"
elif command -v convert >/dev/null; then
  convert "$WIN_DIR/32x32.png" "$WIN_DIR/64x64.png" "$WIN_DIR/128x128.png" \
          "$WIN_DIR/128x128@2x.png" "$WIN_DIR/icon.png" "$WIN_DIR/icon.ico"
elif command -v cargo >/dev/null && cargo tauri --version >/dev/null 2>&1; then
  # `cargo tauri icon` would also blast UWP / iOS / Android outputs into
  # WIN_DIR — we only want the .ico. Run it, snatch the .ico, and wipe the
  # rest so our other platforms' assets aren't double-managed.
  ( cd "$ROOT/clients/windows" && cargo tauri icon "$SRC" >/dev/null )
  rm -rf "$WIN_DIR/android" "$WIN_DIR/ios"
  rm -f  "$WIN_DIR"/Square*Logo.png "$WIN_DIR/StoreLogo.png"
  # Tauri also rewrites the size PNGs from the same SVG; re-run our own
  # render so the file timestamps and bit-exact bytes stay deterministic.
  render_size 32  "$WIN_DIR/32x32.png"
  render_size 64  "$WIN_DIR/64x64.png"
  render_size 128 "$WIN_DIR/128x128.png"
  render_size 256 "$WIN_DIR/128x128@2x.png"
  render_size 512 "$WIN_DIR/icon.png"
else
  echo "    [warn] no .ico renderer found (install imagemagick: brew install imagemagick)"
  echo "    existing $WIN_DIR/icon.ico left untouched"
fi

echo
echo "✓ icons regenerated from $SRC"
