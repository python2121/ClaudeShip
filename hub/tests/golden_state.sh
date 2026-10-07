#!/bin/sh
# Golden test for `GET /api/state` (rust-core-plan.md, phase 4): the running
# Swift hub against a private Rust hub with the same registry and root.
# Manual, for the overlap only (deleted in phase 5). Read-only towards the
# Swift hub: one GET with its cookie, nothing else.
#
#   hub/tests/golden_state.sh [path/to/claudeship]
#
# Masked before the diff: `now` and `protocol` (they differ by design),
# `jobId` (the Rust hub's addition), and what each hub owns — `hubId`,
# `attachable`, `viewers`, `mode`, `key` (h:<hub id> vs p:<pid>). Everything
# derived from the registry, the transcripts, and the root must match.
set -eu

BIN=${1:-target/debug/claudeship}
REAL="$HOME/Library/Application Support/ClaudeShip/hub"
REAL_PORT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("port", 7433))' "$REAL/config.json" 2>/dev/null || echo 7433)
ROOT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("root", "~/Documents/code"))' "$REAL/config.json" 2>/dev/null || echo "~/Documents/code")
PORT=${GOLDEN_PORT:-47439}
HOME_DIR=/tmp/cs-golden-$$
OUT=${TMPDIR:-/tmp}/claudeship-golden-$$
mkdir -p "$HOME_DIR" "$OUT"
python3 -c 'import json,sys; json.dump({"port": int(sys.argv[1]), "root": sys.argv[2]}, open(sys.argv[3], "w"))' "$PORT" "$ROOT" "$HOME_DIR/config.json"

cleanup() {
    CLAUDESHIP_HOME="$HOME_DIR" "$BIN" hub stop --force >/dev/null 2>&1 || true
    rm -rf "$HOME_DIR"
}
trap cleanup EXIT

CLAUDESHIP_HOME="$HOME_DIR" "$BIN" hub start >/dev/null
for _ in 1 2 3 4 5 6 7 8 9 10; do
    curl -s -o /dev/null "http://localhost:$PORT/" && break
    sleep 0.3
done

# Both at once, so the registry is the same moment for each.
curl -s -H "Cookie: claude_ship=$(cat "$REAL/token")" "http://localhost:$REAL_PORT/api/state" >"$OUT/swift.json" &
curl -s -H "Cookie: claude_ship=$(cat "$HOME_DIR/token")" "http://localhost:$PORT/api/state" >"$OUT/rust.json"
wait

mask() {
    python3 - "$1" <<'PY'
import json, sys
MASKED = {"now", "protocol", "jobId", "hubId", "attachable", "viewers", "mode", "key"}
def walk(v):
    if isinstance(v, dict):
        return {k: walk(x) for k, x in v.items() if k not in MASKED}
    if isinstance(v, list):
        return [walk(x) for x in v]
    return v
print(json.dumps(walk(json.load(open(sys.argv[1]))), indent=1, sort_keys=True, ensure_ascii=False))
PY
}
mask "$OUT/swift.json" >"$OUT/swift.masked"
mask "$OUT/rust.json" >"$OUT/rust.masked"
if diff -u "$OUT/swift.masked" "$OUT/rust.masked"; then
    echo "golden: /api/state matches ($(wc -c <"$OUT/swift.json") bytes from the Swift hub)"
    rm -rf "$OUT"
else
    echo "golden: differences above (raw responses in $OUT)"
    exit 1
fi
