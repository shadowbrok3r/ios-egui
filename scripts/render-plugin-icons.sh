#!/usr/bin/env bash
# Rasterize each plugin's icon.svg into the icon.png the store serves.
#
#   scripts/render-plugin-icons.sh plugins-dist
#
# Reads <dir>/<id>/icon.svg and writes <dir>/<id>/icon.png at 192px, which
# publish-plugin.sh then uploads.
set -euo pipefail

root="${1:?usage: render-plugin-icons.sh <plugin-dist-dir>}"
command -v rsvg-convert >/dev/null || { echo "rsvg-convert not installed" >&2; exit 1; }

shopt -s nullglob
for dir in "$root"/*/; do
    svg="${dir}icon.svg"
    [ -f "$svg" ] || continue
    rsvg-convert -w 192 -h 192 -o "${dir}icon.png" "$svg"
    echo "${dir}icon.png"
done
