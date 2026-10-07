#!/usr/bin/env bash
# Build the ClaudeShip hub, install it as ~/.local/bin/claudeship, run it at
# login as a systemd user service, and register Claude Code's PermissionRequest
# hook so prompts can be answered from the tray, the web page, and the phone.
#
#   ./linux/scripts/install-hub.sh          (SKIP_HOOK=1: leave ~/.claude/settings.json alone;
#                                            SKIP_SERVICE=1: no service step; SKIP_BUILD=1: reuse target/release/claudeship)
#
# Plain container (docker/podman, no systemd): see docs/containers.md.
#
# Needs cargo (https://rustup.rs). Builds on this machine; no cross-compiling.
#
# SteamOS / other immutable hosts: run this INSIDE the distrobox, where cargo
# and claude live. The binary it builds runs only in the box; `install-service`
# notices the box and writes a unit for the host's `systemctl --user` whose
# ExecStart is `distrobox-enter -n <box> -- env … claudeship hub run` (box name
# from $CONTAINER_ID or /run/.containerenv). Never `distrobox-export --bin` it:
# $HOME is shared, so the exported wrapper would land on ~/.local/bin/claudeship
# itself. For the hub to outlive logout: distrobox-host-exec loginctl enable-linger "$USER"
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEST_DIR="$HOME/.local/bin"
DEST="$DEST_DIR/claudeship"

if [ "${SKIP_BUILD:-0}" != "1" ] && ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found. Install it from https://rustup.rs" >&2
    exit 1
fi

if [ "${SKIP_BUILD:-0}" = "1" ]; then
    [ -x "$ROOT/target/release/claudeship" ] || { echo "ERROR: SKIP_BUILD=1 but target/release/claudeship is missing" >&2; exit 1; }
else
    echo "==> cargo build --release -p claudeship"
    (cd "$ROOT" && cargo build --release -p claudeship)
fi

mkdir -p "$DEST_DIR"
# Copy beside the destination, then rename: a running hub keeps its old
# inode, and nothing ever sees a half-written binary.
cp "$ROOT/target/release/claudeship" "$DEST.new.$$"
chmod 755 "$DEST.new.$$"
mv -f "$DEST.new.$$" "$DEST"
echo "Installed $DEST"

if [ "${SKIP_SERVICE:-0}" != "1" ]; then
    "$DEST" hub install-service || echo "WARNING: service not installed — run: claudeship hub install-service (or claudeship hub run)" >&2
fi

# Idempotent; touches only its own entry in ~/.claude/settings.json.
if [ "${SKIP_HOOK:-0}" != "1" ]; then
    "$DEST" hub install-hook || echo "WARNING: permission hook not registered — run: claudeship hub install-hook" >&2
fi

echo
echo "Pair a browser or the phone with:"
"$DEST" hub link
