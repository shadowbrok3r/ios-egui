#!/usr/bin/env bash
# Publish a WASM UI plugin to the appstore's plugin store.
#
#   AS_URL=https://appstore.example AS_KEY=<admin api key> \
#     scripts/publish-plugin.sh <plugin-dist-dir> ["what changed"]
#
# The directory is what `cargo egui-ios plugin build --out <dir>` stages:
#   <id>/manifest.toml, <id>/plugin.wasm  (+ optional plugin.cwasm / plugin.cwasm.key)
#
# Identity and permissions come from manifest.toml — the server stores what the
# publisher states rather than parsing the module.
#
# Pass --all to walk a staging root and publish every plugin under it.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

ALL=0
[ "${1:-}" = "--all" ] && { ALL=1; shift; }

[ -n "${AS_URL:-}" ] || die "set AS_URL (e.g. https://appstore.shadowbroker.app)"
[ -n "${AS_KEY:-}" ] || die "set AS_KEY (an admin API key)"
[ $# -ge 1 ] || die "usage: publish-plugin.sh [--all] <plugin-dir> [notes]"

DIR="${1%/}"
NOTES="${2:-}"
[ -d "$DIR" ] || die "no such directory: $DIR"

urlenc() { python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$1"; }

# `key = "value"` out of a manifest.toml, tolerating spacing.
manifest_get() {
    sed -n "s/^[[:space:]]*$2[[:space:]]*=[[:space:]]*\"\(.*\)\"[[:space:]]*$/\1/p" "$1" | head -1
}

publish_one() {
    local dir="$1"
    local manifest="$dir/manifest.toml"
    local wasm="$dir/plugin.wasm"
    [ -f "$manifest" ] || { echo "skip $dir — no manifest.toml" >&2; return 0; }
    [ -f "$wasm" ] || { echo "skip $dir — no plugin.wasm" >&2; return 0; }

    local id name version description abi perms
    id=$(manifest_get "$manifest" id)
    name=$(manifest_get "$manifest" name)
    version=$(manifest_get "$manifest" version)
    description=$(manifest_get "$manifest" description)
    abi=$(sed -n 's/^[[:space:]]*abi_version[[:space:]]*=[[:space:]]*\([0-9]*\).*/\1/p' "$manifest" | head -1)
    # permissions = ["net", "haptic"]  ->  net,haptic
    perms=$(sed -n 's/^[[:space:]]*permissions[[:space:]]*=[[:space:]]*\[\(.*\)\].*/\1/p' "$manifest" \
        | head -1 | tr -d '" ' )

    [ -n "$id" ] || { echo "skip $dir — manifest has no id" >&2; return 0; }
    [ -n "$version" ] || { echo "skip $dir — manifest has no version" >&2; return 0; }
    : "${abi:=1}"

    echo "publishing $id v$version"
    local q="version=$(urlenc "$version")&name=$(urlenc "$name")&abi_version=$abi"
    q="$q&description=$(urlenc "$description")&permissions=$(urlenc "$perms")&notes=$(urlenc "$NOTES")"
    curl -sS --fail-with-body -X POST --data-binary "@$wasm" \
        -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
        "$AS_URL/plugins/$id/upload?$q" || die "upload failed for $id"
    echo

    curl -sS --fail-with-body -X POST --data-binary "@$manifest" \
        -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
        "$AS_URL/plugins/$id/manifest" >/dev/null && echo "  manifest uploaded"

    # Optional icon, rendered from icon.svg by scripts/render-plugin-icons.sh.
    if [ -f "$dir/icon.png" ]; then
        curl -sS --fail-with-body -X POST --data-binary "@$dir/icon.png" \
            -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
            "$AS_URL/plugins/$id/icon" >/dev/null && echo "  icon uploaded"
    fi

    # AOT build, only loadable by the runtime whose fingerprint produced it.
    if [ -f "$dir/plugin.cwasm" ] && [ -f "$dir/plugin.cwasm.key" ]; then
        local key
        key=$(tr -cd 'A-Za-z0-9' < "$dir/plugin.cwasm.key")
        if [ -n "$key" ]; then
            curl -sS --fail-with-body -X POST --data-binary "@$dir/plugin.cwasm" \
                -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
                "$AS_URL/plugins/$id/cwasm?key=$(urlenc "$key")" >/dev/null \
                && echo "  cwasm uploaded (key $key)"
        fi
    fi
}

if [ "$ALL" = 1 ]; then
    found=0
    for d in "$DIR"/*/; do
        [ -f "$d/manifest.toml" ] || continue
        found=1
        publish_one "${d%/}"
    done
    [ "$found" = 1 ] || die "no plugin directories with a manifest.toml under $DIR"
else
    publish_one "$DIR"
fi
