#!/usr/bin/env bash
# Build ClaudeShip as a proper .app bundle so macOS treats it as a menubar
# accessory app (LSUIElement). Output: ./ClaudeShip.app
set -euo pipefail

CONFIG="${CONFIG:-release}"
APP_NAME="ClaudeShip"
BUNDLE_ID="com.andrewnowicki.claudeship"
APP_DIR="${APP_NAME}.app"

cd "$(dirname "$0")"

# Load SIGN_IDENTITY (and any other secrets) from .env if present.
if [[ -f .env ]]; then
  set -a; . ./.env; set +a
fi

# The Command Line Tools cannot compile SwiftUI's `@State` (a macro whose
# plugin ships only in Xcode, as of the macOS 27 SDK). Use `@ViewState` from
# Sources/ClaudeShip/ViewState.swift instead. This check keeps a machine
# that happens to have Xcode from reintroducing it.
if grep -rnE '^[^/]*@State([^A-Za-z0-9_]|$)' Sources/ >/dev/null; then
  echo "error: '@State' does not build with the Command Line Tools; use '@ViewState' (see Sources/ClaudeShip/ViewState.swift):" >&2
  grep -rnE '^[^/]*@State([^A-Za-z0-9_]|$)' Sources/ >&2
  exit 1
fi

echo "==> swift build -c ${CONFIG}"
swift build -c "${CONFIG}"

BIN_PATH="$(swift build -c "${CONFIG}" --show-bin-path)/${APP_NAME}"
if [[ ! -x "${BIN_PATH}" ]]; then
  echo "Built binary not found at ${BIN_PATH}" >&2
  exit 1
fi

echo "==> assembling ${APP_DIR}"
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS"
mkdir -p "${APP_DIR}/Contents/Resources"

cp "${BIN_PATH}" "${APP_DIR}/Contents/MacOS/${APP_NAME}"

# The session hub and its terminal client are the same binary under another
# name (App.main dispatches on argv[0]). A real copy rather than a symlink,
# so the hub's process name differs from the menubar app's — install.sh
# restarts the app by name, which must never take the hub (and every
# session it owns) down with it. Not "claudeship": on a case-insensitive
# filesystem that is the same file as "ClaudeShip".
HUB_NAME="claudeship-cli"
cp "${BIN_PATH}" "${APP_DIR}/Contents/MacOS/${HUB_NAME}"

# The hub's web app: static files it serves from Resources/web.
cp -R web "${APP_DIR}/Contents/Resources/web"

cat >"${APP_DIR}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>${APP_NAME}</string>
  <key>CFBundleDisplayName</key>
  <string>Claude Ship</string>
  <key>CFBundleIdentifier</key>
  <string>${BUNDLE_ID}</string>
  <key>CFBundleExecutable</key>
  <string>${APP_NAME}</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSAppleEventsUsageDescription</key>
  <string>Claude Ship brings a session's terminal to the front when you click its row.</string>
</dict>
</plist>
PLIST

# Signing: prefer a stable identity from SIGN_IDENTITY (set it in .env) so the
# Login Items label stays friendly, but fall back to ad-hoc when none exists.
# Unlike ClaudeUsage, this app owns no keychain items, so an unstable ad-hoc
# cdhash costs nothing — no ACLs to go stale.
SIGN_IDENTITY="${SIGN_IDENTITY:-}"
if [[ -n "${SIGN_IDENTITY}" ]] && security find-identity -p codesigning -v 2>/dev/null | grep -q "\"${SIGN_IDENTITY}\""; then
  echo "==> codesigning with identity: ${SIGN_IDENTITY}"
  SIGN_AS="${SIGN_IDENTITY}"
else
  echo "==> codesigning ad-hoc (set SIGN_IDENTITY in .env for a stable identity)"
  SIGN_AS="-"
fi
# Nested code first: the bundle signature seals it.
codesign --force --sign "${SIGN_AS}" --identifier "${BUNDLE_ID}.hub" "${APP_DIR}/Contents/MacOS/${HUB_NAME}"
codesign --force --sign "${SIGN_AS}" --identifier "${BUNDLE_ID}" "${APP_DIR}"

echo "==> done: $(pwd)/${APP_DIR}"
