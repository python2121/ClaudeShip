#!/usr/bin/env bash
# Build (unless SKIP_BUILD=1), stop any running instance, replace the bundle
# in /Applications, and restart. Idempotent — safe to re-run after every code
# change. Also builds the session hub (the Rust `claudeship` binary in hub/,
# needs cargo), installs it as ~/.local/bin/claudeship, registers the
# PermissionRequest hook through it, and installs the hub's login service.
#
# First run after the move to the Rust hub: the old Swift hub (the bundle's
# `claudeship-cli`) is still running and holds the socket, the port, and the
# lock. Stop it by hand first, with the *old* binary, when its sessions are
# done — this script never stops a running hub:
#   ~/.local/bin/claudeship hub stop      (still the symlink into the old bundle)
set -euo pipefail

cd "$(dirname "$0")"

# A test hub's environment must never leak into the install: `open` hands
# the launched app this shell's variables, and the hub started below reads
# them too — either would then look at a throwaway hub directory.
unset CLAUDESHIP_HOME CLAUDESHIP_CMD CLAUDESHIP_WEB

APP_NAME="ClaudeShip"
APP_BUNDLE="${APP_NAME}.app"
DEST="/Applications/${APP_BUNDLE}"
# The LaunchAgent label is the bundle id (BUNDLE_ID in .env, as in build-app.sh).
if [[ -f .env ]]; then
  set -a; . ./.env; set +a
fi
LABEL="${BUNDLE_ID:-com.example.claudeship}"
PLIST="${HOME}/Library/LaunchAgents/${LABEL}.plist"
UID_NUM="$(id -u)"

if [[ "${SKIP_HUB:-0}" != "1" ]] && ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: cargo not found — the session hub is Rust (hub/). Install it from https://rustup.rs," >&2
  echo "       or run with SKIP_HUB=1 SKIP_HOOK=1 to install the menubar app alone." >&2
  exit 1
fi

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  ./build-app.sh
  if [[ "${SKIP_HUB:-0}" != "1" ]]; then
    echo "==> cargo build --release -p claudeship"
    cargo build --release -p claudeship
  fi
fi

if [[ ! -d "${APP_BUNDLE}" ]]; then
  echo "ERROR: ${APP_BUNDLE} not built — run ./build-app.sh first" >&2
  exit 1
fi

# Stop the LaunchAgent first so launchd doesn't respawn the old binary
# mid-replace. If it's not loaded, this is a no-op.
if launchctl print "gui/${UID_NUM}/${LABEL}" >/dev/null 2>&1; then
  echo "==> stopping LaunchAgent"
  launchctl bootout "gui/${UID_NUM}/${LABEL}" 2>/dev/null || true
fi

# Belt-and-braces — covers manually-launched instances not under launchd.
# Matched by the app's exact command line, never by process name (a
# leftover Swift hub ran as the bundle's binary under another name).
APP_CMD="${DEST}/Contents/MacOS/${APP_NAME}"
if pgrep -fx "${APP_CMD}" >/dev/null 2>&1; then
  echo "==> killing running ${APP_NAME}"
  pkill -fx "${APP_CMD}" || true
  # Wait for it to actually exit before we overwrite the binary.
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -fx "${APP_CMD}" >/dev/null 2>&1 || break
    sleep 0.2
  done
  if pgrep -fx "${APP_CMD}" >/dev/null 2>&1; then
    echo "==> ${APP_NAME} didn't exit, sending SIGKILL"
    pkill -9 -fx "${APP_CMD}" || true
    sleep 0.5
  fi
fi

echo "==> installing to ${DEST}"
rm -rf "${DEST}"
cp -R "${APP_BUNDLE}" "${DEST}"

# Verify the installed copy still has a valid signature after the move.
if ! codesign --verify --verbose=1 "${DEST}" >/dev/null 2>&1; then
  echo "ERROR: ${DEST} fails signature verification after install" >&2
  exit 1
fi

# VS Code bridge extension (vendored in vscode-extension/): lets a row click
# select the exact integrated terminal a session runs in. Packaged here as a
# .vsix (zip + manifest, no npm) and installed through each editor's own CLI;
# editors that aren't installed are skipped. SKIP_VSCODE_EXT=1 opts out.
# Open editor windows pick up a new version after a reload.
if [[ "${SKIP_VSCODE_EXT:-0}" != "1" ]]; then
  VSIX="$(vscode-extension/pack.sh .build)"
  for cli in \
    "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code" \
    "/Applications/Visual Studio Code - Insiders.app/Contents/Resources/app/bin/code-insiders" \
    "/Applications/Cursor.app/Contents/Resources/app/bin/cursor" \
    "/Applications/VSCodium.app/Contents/Resources/app/bin/codium"; do
    [[ -x "${cli}" ]] || continue
    echo "==> installing VS Code bridge extension via $(basename "${cli}")"
    if ! "${cli}" --install-extension "${VSIX}" --force >/dev/null 2>&1; then
      echo "WARNING: bridge extension install failed for ${cli}" >&2
    fi
  done
fi

# The session hub: install `claudeship` (a real file, no longer a symlink
# into the bundle) by copying to a temp name and renaming over the old one,
# so a running hub keeps the binary it has mapped. A hub that is already
# running is never restarted here — it owns live sessions; it keeps running
# the previous build until `claudeship hub restart` (install-service prints
# that hint when the running build differs). SKIP_HUB=1 opts out.
HUB_BIN="${HOME}/.local/bin/claudeship"
if [[ "${SKIP_HUB:-0}" != "1" ]]; then
  BUILT_HUB="target/release/claudeship"
  if [[ ! -x "${BUILT_HUB}" ]]; then
    echo "ERROR: ${BUILT_HUB} not built — run: cargo build --release -p claudeship" >&2
    exit 1
  fi
  mkdir -p "${HOME}/.local/bin"
  cp "${BUILT_HUB}" "${HUB_BIN}.new.$$"
  mv -f "${HUB_BIN}.new.$$" "${HUB_BIN}"
  echo "==> installed ${HUB_BIN}"
  # A LaunchAgent that runs `claudeship hub run` at login and keeps it up.
  echo "==> installing the session hub's login service"
  "${HUB_BIN}" hub install-service || echo "WARNING: session hub service did not install" >&2
  echo "==> to open the web app in a browser (or on the phone): claudeship hub link"
fi

# Register the PermissionRequest hook in ~/.claude/settings.json so approvals
# can be answered from the overlay, the web page, and the phone (the hook
# helper talks to the hub). Idempotent, touches only its own entry, and
# replaces the old Swift helper's `--permission-hook` entry in the same pass.
# SKIP_HOOK=1 to opt out.
if [[ "${SKIP_HOOK:-0}" != "1" ]]; then
  if [[ -x "${HUB_BIN}" ]]; then
    echo "==> registering Claude Code permission hook"
    "${HUB_BIN}" hub install-hook || echo "WARNING: permission hook not registered — run: claudeship hub install-hook" >&2
  else
    echo "WARNING: ${HUB_BIN} missing — permission hook not registered" >&2
  fi
fi

# Start it. Prefer the LaunchAgent if the user has set one up — that way
# launchd will keep it alive and restart it on crash. Otherwise just open.
if [[ -f "${PLIST}" ]]; then
  echo "==> bootstrapping LaunchAgent"
  launchctl bootstrap "gui/${UID_NUM}" "${PLIST}"
else
  echo "==> opening ${DEST}"
  open "${DEST}"
fi

# Confirm it actually started.
sleep 1
if pgrep -fx "${APP_CMD}" >/dev/null 2>&1; then
  echo "==> done — ${APP_NAME} running (pid $(pgrep -fx "${APP_CMD}"))"
else
  echo "WARNING: ${APP_NAME} doesn't appear to be running." >&2
  exit 1
fi
