"""Which command opens a terminal running something, and where the
`claudeship` / `claude` binaries are.

Pure stdlib; the lookups (`which`, `exists`) and the environment are
parameters so the choice is tested without a desktop. Order: `$TERMINAL -e …`
(if on PATH), `ghostty`, `konsole` (or, inside a distrobox without one, the
host's through `distrobox-host-exec`), then `xdg-terminal-exec …`.

A desktop session started from the application menu or autostart often has
no `~/.local/bin` on PATH, which is exactly where `claudeship` and `claude`
are installed, so both are resolved to absolute paths before use.
"""
from __future__ import annotations

import os
import shlex
import shutil


def resolve_binary(name: str, env: dict | None = None, which=shutil.which,
                   exists=os.path.exists) -> str:
    env = os.environ if env is None else env
    found = which(name, path=env.get("PATH"))
    if found:
        return found
    home = env.get("HOME") or os.path.expanduser("~")
    for candidate in (os.path.join(home, ".local", "bin", name),
                      os.path.join(home, ".npm-global", "bin", name),
                      os.path.join("/usr/local/bin", name)):
        if exists(candidate):
            return candidate
    return name


# Per-emulator "start in this directory" flags, as a function of the cwd.
# Anything not listed gets `sh -c 'cd … && exec …'` inside the command.
_WORKDIR = {
    "ghostty": lambda cwd: [f"--working-directory={cwd}"],
    "konsole": lambda cwd: ["--workdir", cwd],
    "foot": lambda cwd: ["--working-directory", cwd],
    "alacritty": lambda cwd: ["--working-directory", cwd],
    "kitty": lambda cwd: ["--directory", cwd],
}

_CONTAINERENV = "/run/.containerenv"
_DOCKERENV = "/.dockerenv"


def _read_text(path: str) -> str:
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def in_container(exists=os.path.exists) -> bool:
    return exists(_CONTAINERENV) or exists(_DOCKERENV)


def container_name(env: dict, read_text=_read_text) -> str:
    """The distrobox's name: $CONTAINER_ID, else `name="…"` in containerenv."""
    name = env.get("CONTAINER_ID", "").strip()
    if name:
        return name
    for line in read_text(_CONTAINERENV).splitlines():
        key, sep, value = line.partition("=")
        if sep and key.strip() == "name":
            return value.strip().strip("\"'")
    return ""


def _with_dir(emulator: list[str], cwd: str, argv: list[str],
              name: str | None = None) -> list[str]:
    """`emulator -e argv…` started in `cwd`; the emulator's own flag when known,
    else a `cd` inside the command (still an argument list, no string built)."""
    flags = _WORKDIR.get(name or os.path.basename(emulator[0]))
    if flags:
        return [*emulator, *flags(cwd), "-e", *argv]
    return [*emulator, "-e", "sh", "-c", 'cd "$1" && shift && exec "$@"', "sh", cwd, *argv]


def terminal_command(argv: list[str], cwd: str, env: dict | None = None,
                     which=shutil.which, exists=os.path.exists,
                     read_text=_read_text) -> list[str] | None:
    """The full command line that opens a terminal running `argv` in `cwd`,
    or None when no terminal is known. Order: `$TERMINAL`, ghostty, konsole
    (the host's, through distrobox, when we're in a box without one),
    xdg-terminal-exec."""
    env = os.environ if env is None else env
    path = env.get("PATH")
    terminal = env.get("TERMINAL", "").strip()
    if terminal:
        # $TERMINAL is conventionally a bare program name, but may carry flags.
        parts = shlex.split(terminal)
        if parts and which(parts[0], path=path):
            return _with_dir(parts, cwd, argv)
    if which("ghostty", path=path):
        return _with_dir(["ghostty"], cwd, argv)
    if which("konsole", path=path):
        return _with_dir(["konsole"], cwd, argv)
    if in_container(exists) and which("distrobox-host-exec", path=path):
        box = container_name(env, read_text)
        if box:
            return _with_dir(["distrobox-host-exec", "konsole"], cwd,
                             ["distrobox-enter", "-n", box, "--", *argv], name="konsole")
    if which("xdg-terminal-exec", path=path):
        return ["xdg-terminal-exec", *argv]
    return None


def attach_argv(claudeship: str, hub_id: str) -> list[str]:
    return [claudeship, "hub", "attach", hub_id]


def background_attach_argv(claude: str, attach_id: str) -> list[str]:
    return [claude, "attach", attach_id]


def hub_start_argv(claudeship: str) -> list[str]:
    return [claudeship, "hub", "start"]
