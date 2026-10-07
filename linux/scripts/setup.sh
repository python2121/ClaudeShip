#!/usr/bin/env bash
# One-time dev setup: venv + PySide6 (+ ruff for the lint), all inside this folder.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

python3 -m venv "$HERE/.venv"
"$HERE/.venv/bin/python" -m pip install --upgrade pip
"$HERE/.venv/bin/python" -m pip install PySide6 ruff

# Record the distrobox name so the launcher can re-enter it from the host.
if [ -f /run/.containerenv ]; then
    name="${CONTAINER_ID:-$(sed -n 's/^name="\(.*\)"/\1/p' /run/.containerenv)}"
    if [ -n "$name" ]; then
        echo "$name" > "$HERE/.distrobox-name"
        echo "Recorded distrobox name: $name"
    fi
fi

echo "Done. Run: $HERE/bin/claudeship-tray"
