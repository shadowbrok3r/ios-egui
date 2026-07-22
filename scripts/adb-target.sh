#!/usr/bin/env bash
# Shared adb target resolution for scripts in this directory.
# Source after setting ADB (path to adb binary). Optional inputs:
#   TCP    — wireless adb host[:port]; connected and selected
#   SERIAL — explicit serial / transport id (--serial)
# Honors ANDROID_SERIAL only when that device is currently online; otherwise
# auto-picks the sole online device (or errors if zero/many).
# Sets: ADBS (argv array), ADB_SERIAL (selected id).

adb_online_serials() {
  # Drop the header and offline/unauthorized rows.
  "$ADB" devices | awk 'NR > 1 && $2 == "device" { print $1 }'
}

adb_resolve_target() {
  local want="${SERIAL:-}"
  local -a online=()
  local s

  if [ -n "${TCP:-}" ]; then
    case "$TCP" in *:*) ;; *) TCP="$TCP:5555" ;; esac
    "$ADB" connect "$TCP" >/dev/null
    want="$TCP"
  fi

  # Stale ANDROID_SERIAL must not pin a missing phone when another is plugged in.
  if [ -z "$want" ] && [ -n "${ANDROID_SERIAL:-}" ]; then
    if adb_online_serials | grep -qxF -- "$ANDROID_SERIAL"; then
      want="$ANDROID_SERIAL"
    else
      echo "warn: ANDROID_SERIAL=$ANDROID_SERIAL is offline; ignoring" >&2
    fi
  fi

  mapfile -t online < <(adb_online_serials)

  if [ -n "$want" ]; then
    if ! "$ADB" -s "$want" get-state >/dev/null 2>&1; then
      echo "adb device '$want' not found / offline" >&2
      "$ADB" devices -l >&2
      return 1
    fi
    ADB_SERIAL="$want"
    ADBS=("$ADB" -s "$want")
    echo "adb device: $ADB_SERIAL"
    return 0
  fi

  if [ "${#online[@]}" -eq 1 ]; then
    ADB_SERIAL="${online[0]}"
    ADBS=("$ADB" -s "$ADB_SERIAL")
    echo "adb device: $ADB_SERIAL"
    return 0
  fi
  if [ "${#online[@]}" -eq 0 ]; then
    echo "no adb devices online" >&2
    "$ADB" devices -l >&2
    return 1
  fi
  echo "multiple adb devices; pass --serial SERIAL or --tcp HOST:PORT:" >&2
  for s in "${online[@]}"; do
    echo "  $s" >&2
  done
  "$ADB" devices -l >&2
  return 1
}
