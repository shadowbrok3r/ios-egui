#!/usr/bin/env bash
# Publish an APK build to the appstore.
#
#   AS_URL=https://appstore.example AS_KEY=<admin api key> \
#     scripts/publish.sh [--commit] [--changelog PATH] <slug> <crate-dir | path/to.apk> ["what changed"]
#
# With a crate dir: version is read from its Cargo.toml, the APK is found under
# the nearest target/release/apk/. With an .apk path: set AS_VERSION=M.m.p.
# versionCode = (1<<24) | (major<<16) | (minor<<8) | patch  (cargo-apk2 rule).
#
# The launcher icon is extracted from the APK and uploaded so the store can show
# it. The notes are recorded as a changelog entry server-side and prepended to
# CHANGELOG.md; --commit also git-commits that file.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

COMMIT=0
CHANGELOG=""
while [ $# -gt 0 ]; do
    case "$1" in
        --commit) COMMIT=1; shift ;;
        --changelog) CHANGELOG="${2:-}"; [ -n "$CHANGELOG" ] || die "--changelog needs a path"; shift 2 ;;
        --no-changelog) NO_CHANGELOG=1; shift ;;
        --no-icon) NO_ICON=1; shift ;;
        --) shift; break ;;
        -*) die "unknown flag $1" ;;
        *) break ;;
    esac
done

[ -n "${AS_URL:-}" ] || die "set AS_URL (e.g. https://appstore.shadowbroker.app)"
[ -n "${AS_KEY:-}" ] || die "set AS_KEY (an admin API key)"
[ $# -ge 2 ] || die "usage: publish.sh [--commit] [--changelog PATH] <slug> <crate-dir|apk-path> [notes]"

SLUG="$1"
TARGET="$2"
NOTES="${3:-}"

if [ -d "$TARGET" ]; then
    MANIFEST="$TARGET/Cargo.toml"
    [ -f "$MANIFEST" ] || die "no Cargo.toml in $TARGET"
    VERSION=$(sed -n 's/^version *= *"\([0-9.]*\)".*/\1/p' "$MANIFEST" | head -1)
    [ -n "$VERSION" ] || die "no version = \"M.m.p\" in $MANIFEST (workspace-inherited? pass the APK path and AS_VERSION instead)"
    CRATE=$(sed -n 's/^name *= *"\(.*\)".*/\1/p' "$MANIFEST" | head -1)
    APK_FILE="${CRATE//-/_}.apk"
    APK=""
    for base in "$TARGET" "$TARGET/.." "$TARGET/../.."; do
        if [ -f "$base/target/release/apk/$APK_FILE" ]; then
            APK="$base/target/release/apk/$APK_FILE"
            break
        fi
    done
    [ -n "$APK" ] || die "no target/release/apk/$APK_FILE near $TARGET — build it first"
    : "${CHANGELOG:=$TARGET/CHANGELOG.md}"
else
    APK="$TARGET"
    [ -f "$APK" ] || die "no such file: $APK"
    MANIFEST=""
    VERSION="${AS_VERSION:-}"
    [ -n "$VERSION" ] || die "set AS_VERSION=M.m.p when publishing a bare APK path"
    : "${CHANGELOG:=$(git -C "$(dirname "$APK")" rev-parse --show-toplevel 2>/dev/null || dirname "$APK")/CHANGELOG.md}"
fi

IFS=.- read -r MAJ MIN PAT _ <<< "$VERSION"
: "${MAJ:?}" "${MIN:?}" "${PAT:?}"
CODE=$(( (1 << 24) | (MAJ << 16) | (MIN << 8) | PAT ))

AAPT2=$(command -v aapt2 2>/dev/null || ls -1 "$HOME"/Android/Sdk/build-tools/*/aapt2 2>/dev/null | sort -V | tail -1 || true)

# The APK carries the versionCode Android actually enforces. Publishing a stale build
# under a bumped version makes every phone offer an update it then refuses to install.
if [ -n "$AAPT2" ]; then
    # `|| true`: aapt2 exits nonzero on anything it cannot parse, and under
    # `set -e -o pipefail` that would abort the publish with no message.
    BADGING=$("$AAPT2" dump badging "$APK" 2>/dev/null || true)
    CODE_APK=$(printf '%s' "$BADGING" | sed -n "s/.*versionCode='\([0-9]*\)'.*/\1/p" | head -1 || true)
    if [ -n "$CODE_APK" ] && [ "$CODE_APK" != "$CODE" ]; then
        die "APK versionCode $CODE_APK != $CODE from $VERSION — rebuild the APK after bumping the version"
    fi
    # An APK without a launcher activity installs, reports as present, and has no
    # home-screen entry — indistinguishable from a failed install on the phone.
    if [ -n "$BADGING" ] && ! printf '%s' "$BADGING" | grep -q '^launchable-activity:'; then
        die "$APK declares no launcher activity — add [[package.metadata.android.application.activity]] with a MAIN/LAUNCHER intent_filter"
    fi
elif [ -n "$MANIFEST" ] && [ "$MANIFEST" -nt "$APK" ]; then
    die "$APK is older than $MANIFEST — rebuild it (install aapt2 for an exact check)"
fi

urlenc() { python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$1"; }

echo "publishing $APK"
echo "  slug=$SLUG version=$VERSION code=$CODE"

APK_SIZE=$(stat -c %s "$APK")
# A proxy in front of the store may cap request bodies (Cloudflare's is 100 MB), so
# anything near that goes up as a sequence of appends instead of one request.
CHUNK_MB="${AS_CHUNK_MB:-64}"
THRESHOLD=$(( ${AS_CHUNK_THRESHOLD_MB:-80} * 1024 * 1024 ))

if [ "$APK_SIZE" -le "$THRESHOLD" ]; then
    RESP=$(curl -sS --fail-with-body -X POST --data-binary "@$APK" \
        -H "x-api-key: $AS_KEY" \
        -H "Accept: application/json" \
        "$AS_URL/$SLUG/upload-apk?version_code=$CODE&version_name=$(urlenc "$VERSION")&notes=$(urlenc "$NOTES")") \
        || die "upload failed: $RESP"
    echo "$RESP"
else
    echo "  $(( APK_SIZE / 1048576 )) MB — uploading in ${CHUNK_MB} MB chunks"
    INIT=$(curl -sS --fail-with-body -X POST \
        -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
        "$AS_URL/$SLUG/upload-init?version_code=$CODE&version_name=$(urlenc "$VERSION")&notes=$(urlenc "$NOTES")") \
        || die "upload-init failed: $INIT"
    UPLOAD_ID=$(printf '%s' "$INIT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("upload_id",""))')
    [ -n "$UPLOAD_ID" ] || die "upload-init returned no upload_id: $INIT"

    abort_upload() {
        curl -sS -X POST -H "x-api-key: $AS_KEY" \
            "$AS_URL/$SLUG/upload-abort?upload_id=$UPLOAD_ID" >/dev/null 2>&1 || true
    }
    trap abort_upload EXIT

    OFFSET=0
    CHUNK_BYTES=$(( CHUNK_MB * 1048576 ))
    while [ "$OFFSET" -lt "$APK_SIZE" ]; do
        n=$(( (OFFSET / 1048576) ))
        printf '    %s / %s MB\r' "$(( OFFSET / 1048576 ))" "$(( APK_SIZE / 1048576 ))"
        CHUNK_RESP=$(dd if="$APK" bs=1048576 skip="$n" count="$CHUNK_MB" status=none \
            | curl -sS --fail-with-body -X POST --data-binary @- \
                -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
                -H "Content-Type: application/octet-stream" \
                "$AS_URL/$SLUG/upload-chunk?upload_id=$UPLOAD_ID&offset=$OFFSET") \
            || die "chunk at offset $OFFSET failed: $CHUNK_RESP"
        OFFSET=$(printf '%s' "$CHUNK_RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("received",0))')
        [ "$OFFSET" -gt 0 ] || die "server reported no progress: $CHUNK_RESP"
    done
    printf '    %s / %s MB\n' "$(( APK_SIZE / 1048576 ))" "$(( APK_SIZE / 1048576 ))"

    RESP=$(curl -sS --fail-with-body -X POST \
        -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
        "$AS_URL/$SLUG/upload-finish?upload_id=$UPLOAD_ID") \
        || die "upload-finish failed: $RESP"
    trap - EXIT
    echo "$RESP"
fi

# ---- launcher icon -------------------------------------------------------
# Best-effort: an APK built without an icon resource simply has none, and the
# store falls back to a letter tile.
if [ -z "${NO_ICON:-}" ] && [ -n "$AAPT2" ] && command -v unzip >/dev/null; then
    ICON_RES=$("$AAPT2" dump badging "$APK" 2>/dev/null \
        | sed -n "s/^application-icon-\([0-9]*\):'\(.*\)'$/\1 \2/p" \
        | sort -rn | awk '$2 ~ /\.png$/ { print $2; exit }' || true)
    if [ -z "$ICON_RES" ]; then
        # Adaptive icons (mipmap-anydpi-v26/ic_launcher.xml) expose no PNG to aapt2.
        # Prefer the composed ic_launcher.png over a bare adaptive layer — the
        # background layer is often the largest file but is just a colour field.
        for pat in '/ic_launcher[.]png$' '/ic_launcher_foreground[.]png$' '[.]png$'; do
            ICON_RES=$(unzip -l "$APK" 2>/dev/null \
                | awk -v p="$pat" '$4 ~ /^res\/mipmap/ && $4 ~ p { print $1, $4 }' \
                | sort -rn | awk 'NR==1 { print $2 }' || true)
            [ -n "$ICON_RES" ] && break
        done
    fi
    if [ -n "$ICON_RES" ]; then
        TMP_ICON=$(mktemp --suffix=.png)
        if unzip -p "$APK" "$ICON_RES" > "$TMP_ICON" 2>/dev/null && [ -s "$TMP_ICON" ]; then
            ICON_RESP=$(curl -sS -X POST --data-binary "@$TMP_ICON" \
                -H "x-api-key: $AS_KEY" -H "Accept: application/json" \
                "$AS_URL/$SLUG/icon" 2>&1) \
                && echo "  icon: $ICON_RES -> $ICON_RESP" \
                || echo "  icon upload failed (not fatal): $ICON_RESP" >&2
        fi
        rm -f "$TMP_ICON"
    else
        echo "  icon: none in APK (store will show a letter tile)"
    fi
fi

# ---- changelog -----------------------------------------------------------
# The server already recorded this release; keep a copy in the repo too.
if [ -n "$NOTES" ] && [ -n "$CHANGELOG" ] && [ -z "${NO_CHANGELOG:-}" ]; then
    # Newest entry goes directly under the "# Changelog" title, not above it.
    python3 - "$CHANGELOG" "$VERSION" "$(date +%Y-%m-%d)" "$NOTES" <<'PY'
import os, sys
path, version, today, notes = sys.argv[1:5]
entry = f"## v{version} — {today}\n\n{notes.rstrip()}\n"
old = ""
if os.path.exists(path):
    old = open(path).read()
if f"## v{version} —" in old:
    print(f"  changelog: v{version} already present in {path}, not duplicating")
    raise SystemExit
if old.lstrip().startswith("# "):
    head, _, rest = old.lstrip().partition("\n")
    body = f"{head}\n\n{entry}\n{rest.lstrip()}"
    action = "prepended"
elif old.strip():
    body = f"# Changelog\n\n{entry}\n{old.lstrip()}"
    action = "prepended"
else:
    body = f"# Changelog\n\n{entry}"
    action = "created"
open(path, "w").write(body.rstrip() + "\n")
print(f"  changelog: {action} v{version} in {path}")
PY

    if [ "$COMMIT" = 1 ]; then
        REPO=$(git -C "$(dirname "$CHANGELOG")" rev-parse --show-toplevel 2>/dev/null) \
            || die "--commit: $CHANGELOG is not in a git repo"
        git -C "$REPO" add -- "$CHANGELOG" ${MANIFEST:+"$MANIFEST"}
        if git -C "$REPO" diff --cached --quiet; then
            echo "  commit: nothing staged"
        else
            git -C "$REPO" commit -q -m "release $SLUG v$VERSION" -m "$NOTES"
            echo "  commit: $(git -C "$REPO" log -1 --format=%h) release $SLUG v$VERSION"
        fi
    fi
fi
