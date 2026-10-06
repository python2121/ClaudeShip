#!/usr/bin/env bash
# Builds assets/ClaudeShip.icns for the menubar app from web/icon.svg (the
# one drawing every surface uses), via ios/make-icon.swift for the 1024 px
# master and iconutil for the rest. Run from the repo root after changing
# the SVG; build-app.sh copies the result into the bundle.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p assets
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
swift ios/make-icon.swift "$TMP/master.png" --rounded >/dev/null
SET="$TMP/ClaudeShip.iconset"
mkdir -p "$SET"
for px in 16 32 128 256 512; do
  sips -z $px $px "$TMP/master.png" --out "$SET/icon_${px}x${px}.png" >/dev/null
  double=$((px * 2))
  sips -z $double $double "$TMP/master.png" --out "$SET/icon_${px}x${px}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o assets/ClaudeShip.icns
echo "wrote assets/ClaudeShip.icns"
