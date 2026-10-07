#!/usr/bin/env bash
# Put the ClaudeShip tray applet in the application menu, and optionally
# start it on login.
#
#   ./scripts/install-linux.sh [--autostart]
#
# Because a distrobox shares $HOME with the host, running this inside the box
# registers the entry for the host session too (the launcher re-enters the
# box automatically).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APPS="$HOME/.local/share/applications"
DESKTOP="$APPS/claudeship-tray.desktop"

mkdir -p "$APPS"
sed -e "s|@ROOT@|$HERE|g" "$HERE/claudeship-tray.desktop.in" > "$DESKTOP"
chmod +x "$HERE/bin/claudeship-tray"
update-desktop-database "$APPS" 2>/dev/null || true

echo "Installed $DESKTOP"

if [ "${1:-}" = "--autostart" ]; then
    mkdir -p "$HOME/.config/autostart"
    cp "$DESKTOP" "$HOME/.config/autostart/claudeship-tray.desktop"
    echo "Autostart on login enabled."
fi
