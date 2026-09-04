#!/usr/bin/env bash
# Rasterize an app's launcher SVG into the mipmap densities cargo-apk2 packages.
#
#   scripts/render-icons.sh examples/privaxy-android
#
# Reads <app>/icon/ic_launcher.svg and writes <app>/res/mipmap-<density>/ic_launcher.png.
# Re-run after editing the SVG; the PNGs are committed so a build needs no image tooling.
set -euo pipefail

app="${1:?usage: render-icons.sh <app-dir>}"
src="$app/icon/ic_launcher.svg"
[ -f "$src" ] || { echo "no $src" >&2; exit 1; }
command -v rsvg-convert >/dev/null || { echo "rsvg-convert not installed" >&2; exit 1; }

# Launcher icon edge in px per density bucket (48dp at 1x/1.5x/2x/3x/4x).
for spec in mdpi:48 hdpi:72 xhdpi:96 xxhdpi:144 xxxhdpi:192; do
    density="${spec%%:*}"
    px="${spec##*:}"
    out="$app/res/mipmap-$density"
    mkdir -p "$out"
    rsvg-convert -w "$px" -h "$px" -o "$out/ic_launcher.png" "$src"
    echo "$out/ic_launcher.png (${px}x${px})"
done
