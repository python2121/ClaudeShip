#!/usr/bin/env bash
# Compile the iPhone app for the simulator — the "does it still build" check.
# Needs full Xcode (the iOS SDK); the Command Line Tools alone can't do it.
#
#   ./build.sh            # build
#   ./build.sh run        # build, then install + launch on the booted simulator
set -euo pipefail
cd "$(dirname "$0")"

APP=build/Build/Products/Debug-iphonesimulator/ClaudeHub.app
BUNDLE=com.python21.ClaudeHub

xcodebuild -project ClaudeHub.xcodeproj -scheme ClaudeHub \
  -destination 'generic/platform=iOS Simulator' -derivedDataPath build \
  -skipPackagePluginValidation \
  CODE_SIGNING_ALLOWED=NO build 2>&1 \
  | grep -E 'error:|warning: .*\.swift|BUILD (SUCCEEDED|FAILED)' | grep -v '^\s*|' || true
test -d "$APP" || { echo "error: no app at $APP" >&2; exit 1; }

if [[ "${1:-}" == "run" ]]; then
  xcrun simctl bootstatus booted -b >/dev/null 2>&1 || xcrun simctl boot "iPhone 17e"
  xcrun simctl install booted "$APP"
  xcrun simctl terminate booted "$BUNDLE" 2>/dev/null || true
  xcrun simctl launch booted "$BUNDLE"
fi
