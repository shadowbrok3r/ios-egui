#!/usr/bin/env bash
# Publish a comfyui-android build to comfy-gate's self-update endpoint.
#
#   CG_URL=https://gate.example CG_KEY=<admin api key> \
#     scripts/publish-apk.sh ["what changed"] [path/to.apk]
#
# The in-app updater (Settings -> App updates) reads `GET /comfyui-android/update.json`
# and offers the build only when its versionCode is STRICTLY GREATER than the installed
# one, so bump `version` in examples/comfyui-android/Cargo.toml and rebuild first.
#
# This is the gate path, which is independent of the app store: scripts/publish-appstore.sh
# ships the same APK to https://appstore.shadowbroker.app. Run both to have either route work.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
NOTES="${1:-}"
APK="${2:-$REPO/target/release/apk/comfyui_android.apk}"
MANIFEST="$REPO/examples/comfyui-android/Cargo.toml"

[ -n "${CG_URL:-}" ] || die "set CG_URL (the comfy-gate base URL)"
[ -n "${CG_KEY:-}" ] || die "set CG_KEY (an admin API key)"
[ -f "$APK" ] || die "no APK at $APK — build it first:
  cd examples/comfyui-android && cargo egui-mobile build -a --release --features tls"

VERSION="${CG_VERSION:-$(sed -n 's/^version *= *"\([0-9.]*\)".*/\1/p' "$MANIFEST" | head -1)}"
[ -n "$VERSION" ] || die "no version = \"M.m.p\" in $MANIFEST"
IFS=. read -r MAJ MIN PAT <<< "$VERSION"
: "${MAJ:?}" "${MIN:?}" "${PAT:?}"
# cargo-apk2's rule; the gate never parses the APK, so the uploader states the number.
CODE=$(( (1 << 24) | (MAJ << 16) | (MIN << 8) | PAT ))

# The APK carries the versionCode Android actually enforces. Publishing a stale build under a
# bumped version makes every phone offer an update it then refuses to install.
AAPT2=$(command -v aapt2 2>/dev/null || ls -1 "$HOME"/Android/Sdk/build-tools/*/aapt2 2>/dev/null | sort -V | tail -1 || true)
if [ -n "$AAPT2" ]; then
    CODE_APK=$("$AAPT2" dump badging "$APK" 2>/dev/null | sed -n "s/.*versionCode='\([0-9]*\)'.*/\1/p" | head -1 || true)
    if [ -n "$CODE_APK" ] && [ "$CODE_APK" != "$CODE" ]; then
        die "APK versionCode $CODE_APK != $CODE from $VERSION — rebuild after bumping the version"
    fi
elif [ "$MANIFEST" -nt "$APK" ]; then
    die "$APK is older than $MANIFEST — rebuild it (install aapt2 for an exact check)"
fi

urlenc() { python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$1"; }

echo "publishing $APK to $CG_URL"
echo "  version=$VERSION code=$CODE size=$(( $(stat -c %s "$APK") / 1048576 )) MB"

# Streamed server-side, so one request regardless of size; the gate hashes it as it lands and
# writes the sidecar last, so an interrupted upload reads as "nothing published".
RESP=$(curl -sS --fail-with-body -X POST --data-binary "@$APK" \
    -H "x-api-key: $CG_KEY" \
    -H "Content-Type: application/octet-stream" \
    -H "Accept: application/json" \
    "$CG_URL/comfyui-android/upload-apk?version_code=$CODE&version_name=$(urlenc "$VERSION")&notes=$(urlenc "$NOTES")") \
    || die "upload failed: $RESP"
echo "$RESP"

# What the phone will see on its next check.
curl -sS -H "x-api-key: $CG_KEY" -H "Accept: application/json" \
    "$CG_URL/comfyui-android/update.json" 2>/dev/null | head -c 400
echo
