#!/usr/bin/env bash
# Build (unless SKIP_BUILD=1), stop any running instance, replace the bundle
# in /Applications, and restart. Idempotent — safe to re-run after every code
# change.
set -euo pipefail

cd "$(dirname "$0")"

# A test hub's environment must never leak into the install: `open` hands
# the launched app this shell's variables, and the hub started below reads
# them too — either would then look at a throwaway hub directory.
unset CLAUDESHIP_HOME CLAUDESHIP_CMD CLAUDESHIP_WEB

APP_NAME="ClaudeShip"
APP_BUNDLE="${APP_NAME}.app"
DEST="/Applications/${APP_BUNDLE}"
LABEL="com.andrewnowicki.claudeship"
PLIST="${HOME}/Library/LaunchAgents/${LABEL}.plist"
UID_NUM="$(id -u)"

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  ./build-app.sh
fi

# One-time move from the app's former name (ClaudeStatus): its settings,
# the hub's socket/token/config, and the VS Code bridge files all live in
# the Application Support folder, which simply moves. A hub that is still
# running keeps answering at the moved socket path, so its sessions
# survive; new sessions need it restarted (noted at the end).
OLD_SUPPORT="${HOME}/Library/Application Support/ClaudeStatus"
NEW_SUPPORT="${HOME}/Library/Application Support/${APP_NAME}"
if [[ -d "${OLD_SUPPORT}" && ! -d "${NEW_SUPPORT}" ]]; then
  echo "==> moving Application Support/ClaudeStatus → ${APP_NAME}"
  mv "${OLD_SUPPORT}" "${NEW_SUPPORT}"
fi
if launchctl print "gui/${UID_NUM}/com.andrewnowicki.claudestatus" >/dev/null 2>&1; then
  launchctl bootout "gui/${UID_NUM}/com.andrewnowicki.claudestatus" 2>/dev/null || true
fi
if pgrep -x ClaudeStatus >/dev/null 2>&1; then
  echo "==> stopping the old ClaudeStatus menubar app"
  pkill -x ClaudeStatus || true
  sleep 0.5
fi
rm -rf "/Applications/ClaudeStatus.app"
rm -f "${HOME}/.local/bin/claudeandrew"

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
# Matched by the app's exact command line, never by process name: the hub
# is the same binary under another name and must survive an install.
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

# Register the PermissionRequest hook in ~/.claude/settings.json so approvals
# can be answered from the app. Idempotent (only touches its own marked
# entry, prints "Nothing to do" when already present). SKIP_HOOK=1 to opt
# out; remove later with: ClaudeShip --uninstall-hook
if [[ "${SKIP_HOOK:-0}" != "1" ]]; then
  echo "==> registering Claude Code permission hook"
  "${DEST}/Contents/MacOS/${APP_NAME}" --install-hook
fi

# The session hub: put `claudeship` on PATH and make sure a hub is up, so
# the web app answers. A hub that is already running is left alone — it owns
# live sessions, and restarting it would end them; it keeps running the
# previous build until `claudeship hub stop`. SKIP_HUB=1 opts out.
if [[ "${SKIP_HUB:-0}" != "1" ]]; then
  HUB_BIN="${DEST}/Contents/MacOS/claudeship-cli"
  mkdir -p "${HOME}/.local/bin"
  ln -sf "${HUB_BIN}" "${HOME}/.local/bin/claudeship"
  echo "==> linked ${HOME}/.local/bin/claudeship"
  if "${HUB_BIN}" hub status >/dev/null 2>&1; then
    echo "==> session hub already running (previous build until: claudeship hub stop)"
  else
    echo "==> starting session hub"
  fi
  "${HUB_BIN}" hub start || echo "WARNING: session hub did not start" >&2
  echo "==> to open the web app in a browser (or on the phone): claudeship hub link"
  if ps -Ao command | grep "^/Applications/ClaudeStatus.app/.*hub run" >/dev/null; then  # not -q: pipefail + SIGPIPE
    echo "NOTE: the running hub is the old ClaudeStatus build, whose bundle is now gone;"
    echo "      its current sessions keep working, but new ones will fail to start until"
    echo "      you restart it (ends those sessions): claudeship hub stop && claudeship hub start"
  elif ps -Ao command | grep "^${APP_CMD} --cli hub run" >/dev/null; then
    echo "NOTE: the running hub was started under the app's own name (a build where the"
    echo "      CLI copy collided with it); it keeps working, but restart it when its"
    echo "      sessions are done so it runs as claudeship-cli: claudeship hub stop && claudeship hub start"
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
