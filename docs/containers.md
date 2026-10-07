# The hub in a plain headless container

For a NAS (docker/podman, host networking, no systemd, no distrobox) running
only the hub. `install-service` there prints "no service manager" and exits 0;
the container's command is the service.

## Image

```dockerfile
FROM archlinux:latest
RUN pacman -Sy --noconfirm base-devel git rustup nodejs npm openssh \
 && npm i -g @anthropic-ai/claude-code
RUN useradd -m -s /bin/bash ship
USER ship
WORKDIR /home/ship
RUN rustup default stable
RUN git clone <repo-url> ClaudeShip && cd ClaudeShip \
 && ~/.cargo/bin/cargo build --release -p claudeship \
 && SKIP_BUILD=1 SKIP_HOOK=1 ./linux/scripts/install-hub.sh
CMD ["/home/ship/.local/bin/claudeship", "hub", "run"]
```

(Debian works the same: `apt install build-essential git curl nodejs npm`,
rustup from rustup.rs.) The user needs a real shell in `/etc/passwd`:
sessions start through it. Mounting a volume over `/home/ship` hides what the
image put there, so run `install-hub.sh` (or copy the binary to
`~/.local/bin`) on first start if the volume is empty.

## Run

```yaml
services:
  claudeship:
    build: .
    network_mode: host      # the NAS runs Tailscale; the hub sees tailscale0
    restart: unless-stopped
    volumes:
      - ship-home:/home/ship  # ~/.claude, the hub home, projects
volumes:
  ship-home:
```

## First run

1. `docker compose exec claudeship claudeship hub link`, then open the link
   from a browser on another machine (pairs it).
2. Open a session there and log Claude in (`/login`) inside it.
3. `claudeship hub install-hook` for remote approvals.
4. Remote jobs: set `"jobs": true` in the hub's `config.json` (in the hub
   home under `$HOME`), restart the container.
5. Join the swarm: `claudeship hub pair <link>` with a member's link, or the
   web page's Computers panel.

## Check

- `claudeship hub status` shows the tailnet address.
- `ip link` shows `tailscale0` (if not, host networking is missing or the NAS
  Tailscale is down).
- A restart ends the sessions it owns; the volume keeps the transcripts.
