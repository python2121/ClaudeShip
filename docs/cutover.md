# Cutover: the Rust hub on the Mac, then Linux

Run from a plain terminal (Terminal.app, Ghostty), never from a Claude
session the hub owns: step 2 ends every session it owns.

## Mac

1. Build first, so a build error doesn't strand you between hubs (from the
   repo root; `install.sh` builds again with `.env` loaded, which bakes your
   `BUNDLE_ID` into the hub's service label):
   ```bash
   cargo build --release -p claudeship
   ./build-app.sh
   ```
2. Stop the **old** Swift hub, with the old binary, before the install
   replaces it (this ends the sessions it owns; it never ran as a login
   service, so nothing restarts it):
   ```bash
   /Applications/ClaudeShip.app/Contents/MacOS/claudeship-cli hub stop --force
   ```
3. Install:
   ```bash
   ./install.sh
   ```
   It installs the app, `~/.local/bin/claudeship` (a real file now, replacing
   the symlink into the bundle), the login service (`claudeship hub
   install-service`: `~/Library/LaunchAgents/<BUNDLE_ID>.hub.plist`, which
   starts the hub), and the permission hook (`claudeship hub install-hook`,
   which replaces the old `ClaudeShip --permission-hook` entry in
   `~/.claude/settings.json` in the same pass).
4. Verify:
   ```bash
   claudeship hub status
   swift run ClaudeShip --scan
   claudeship hub link
   ```
   - The status shows the launchd-run hub; the scan lists your sessions.
   - Open the web page from the link; the directory loads.
   - The phone's "hub is a different version" banner is gone.
   - Trigger a permission prompt: Approve / Deny appear on the session row
     in the menubar overlay, the web page, and the phone.
5. Run the manual regression checklist in `docs/hub.md` ("Manual checklist").

## Roll back

From the repo root of the Rust build, in a plain terminal. Loading `.env`
first makes sure the commands use the same `BUNDLE_ID` (the service label)
the install did:

```bash
set -a; . ./.env; set +a
claudeship hub stop --force          # unloads <BUNDLE_ID>.hub from launchd and stops the hub (ends its sessions)
claudeship hub uninstall-service     # removes ~/Library/LaunchAgents/<BUNDLE_ID>.hub.plist
claudeship hub uninstall-hook        # removes the `claudeship permission-hook` entry
git switch --detach <last commit before the Rust hub>   # 7119e50 when this was written
./install.sh                         # the old build: Swift hub, re-links ~/.local/bin/claudeship
                                     # into the bundle, registers its own --permission-hook
```

`uninstall-hook` must run before the old install: the old installer only
recognises its own `--permission-hook` entry, so the Rust one would stay, and
once `~/.local/bin/claudeship` is the old binary again, `claudeship
permission-hook` would mean "run claude with that argument".

## Linux box

1. Install Rust: `curl https://sh.rustup.rs -sSf | sh`, then a new shell.
2. Clone the repo and `cd` into it.
3. Hub: `linux/scripts/install-hub.sh` (builds, installs `~/.local/bin/claudeship`,
   the `claudeship-hub` systemd user unit, the permission hook, and prints
   the pairing link). Optional, so the hub survives logout:
   `loginctl enable-linger $USER`.
4. Tray applet:
   ```bash
   cd linux
   ./scripts/setup.sh
   ./scripts/install-linux.sh --autostart
   ```
5. `claudeship hub link` prints the URLs (the box's tailnet address among
   them) and a QR code. Pair Safari on the Mac with the tailnet URL and the
   phone with the QR code.
   **SteamOS / distrobox:** do steps 1–5 inside one box (everything lives
   there); `install-service` writes a host unit that re-enters the box via
   `distrobox-enter`. `export TERMINAL=ghostty` for row clicks; don't
   `distrobox-export --bin` the binary; linger runs on the host:
   `distrobox-host-exec loginctl enable-linger $USER`.
6. Join the Mac's swarm (optional): on the box, `claudeship hub pair
   <link>` with the tailnet link the Mac's `claudeship hub link` prints (or
   pair both hubs on the phone and take its "Add to the swarm" step). Then
   `claudeship hub peers` on either machine lists the other as reachable,
   and the web page and the phone show both machines' sessions from
   either hub. Both hubs must run the same build (protocol); a hub refuses
   to relay to a peer on another one and names it.
7. Verify (see `docs/linux.md`): the tray glyph and popup follow a session's
   state; Approve/Deny reach the hub; clicking a row opens a terminal on
   `claudeship hub attach`; End session works; the web launch resolves
   `claude` from the login shell's PATH with bash `-l -i` quiet; `ulimit -n`
   is raised, `/dev/ptmx` is usable, and a UTF-8 paste round-trips (`IUTF8`).

Hub-only in a plain container (NAS): see [containers.md](containers.md).
