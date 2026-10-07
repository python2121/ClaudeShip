//! Starts a program on a pseudo-terminal the hub owns.

use std::ffi::{CStr, CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

pub struct Spawned {
    /// The session leader: the supervisor (`hub supervise`), whose child
    /// is the program.
    pub pid: i32,
    /// The pty master: non-blocking, close-on-exec.
    pub master: OwnedFd,
}

fn os_error(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

/// Runs `argv` in `cwd` on a fresh pty of the given size, in a session of
/// its own, under the supervisor (this binary, `hub supervise`), which
/// stays as the session leader and runs the program as its foreground
/// job — see `supervisor.rs` for why the program can't lead the session.
///
/// `std::process::Command` forks; everything the child does before exec
/// is in `pre_exec` and async-signal-safe: a new session, the slave as
/// fds 0–2 (which, opened by a session leader without a terminal, makes it
/// the controlling terminal; `TIOCSCTTY` makes sure), every other
/// descriptor close-on-exec, default signals.
pub fn spawn(
    supervisor: &Path,
    argv: &[OsString],
    cwd: &Path,
    env: &[(OsString, OsString)],
    rows: u16,
    cols: u16,
) -> Result<Spawned, String> {
    if argv.is_empty() {
        return Err("nothing to run".into());
    }
    // SAFETY: plain libc calls on descriptors this function owns; every
    // early return closes what was opened (OwnedFd).
    let (master, slave_path, slave) = unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if fd < 0 {
            return Err(os_error("posix_openpt"));
        }
        let master = OwnedFd::from_raw_fd(fd);
        set_cloexec(fd);
        if libc::grantpt(fd) != 0 || libc::unlockpt(fd) != 0 {
            return Err(os_error("pty setup"));
        }
        // ptsname's buffer is static; copied out at once, and only the hub
        // task ever opens ptys.
        let name = libc::ptsname(fd);
        if name.is_null() {
            return Err(os_error("ptsname"));
        }
        let slave_path = CStr::from_ptr(name).to_owned();
        // Hold the slave open across the spawn: it carries the size and
        // termios set here, and keeps the master from reading EOF before
        // the child has opened its own copy.
        let s = libc::open(
            slave_path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        if s < 0 {
            return Err(os_error(&format!("open {}", slave_path.to_string_lossy())));
        }
        (master, slave_path, OwnedFd::from_raw_fd(s))
    };
    set_size(slave.as_raw_fd(), rows, cols);
    // SAFETY: termios is plain data; tcgetattr fills it.
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut term) == 0 {
            term.c_iflag |= libc::IUTF8;
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &term);
        }
    }

    let mut command = Command::new(supervisor);
    command
        .arg("hub")
        .arg("supervise")
        .args(argv)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("PWD", cwd)
        .current_dir(cwd);
    let path: CString = slave_path;
    let limit = descriptor_limit();
    // SAFETY: the closure runs between fork and exec and only makes
    // async-signal-safe calls on memory allocated before the fork.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = libc::open(path.as_ptr(), libc::O_RDWR);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            for target in 0..3 {
                if fd != target && libc::dup2(fd, target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if fd > 2 {
                libc::close(fd);
            }
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            cloexec_above_stdio(limit);
            reset_signals();
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", supervisor.display()))?;
    drop(slave);
    set_nonblocking(master.as_raw_fd());
    Ok(Spawned {
        pid: child.id() as i32,
        master,
    })
}

/// Every signal back to its default action and none blocked, so a child
/// starts clean whatever this process ignores or blocks. Async-signal-safe.
pub fn reset_signals() {
    // SAFETY: signal and sigprocmask are async-signal-safe; the set lives
    // on this stack frame.
    unsafe {
        for sig in 1..32 {
            if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
}

/// One past the highest descriptor this process can hold (capped, for a
/// soft limit set absurdly high). Computed before a fork: getrlimit isn't
/// on the async-signal-safe list.
pub fn descriptor_limit() -> i32 {
    // SAFETY: getrlimit fills a struct on this frame.
    let soft = unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur
        } else {
            1024
        }
    };
    soft.min(1 << 16) as i32
}

/// Every descriptor from 3 up to `limit` close-on-exec, so nothing this
/// process holds (or inherited without the flag, or opened on another
/// thread a moment before the fork) reaches the program — what
/// POSIX_SPAWN_CLOEXEC_DEFAULT did for the Swift hub. Async-signal-safe.
pub fn cloexec_above_stdio(limit: i32) {
    for fd in 3..limit {
        // SAFETY: fcntl on a descriptor number; a closed one just fails.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

pub fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a descriptor the caller owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

pub fn set_nonblocking(fd: RawFd) {
    // SAFETY: fcntl on a descriptor the caller owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

fn set_size(fd: RawFd, rows: u16, cols: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads a winsize.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ as _, &size);
    }
}

/// Tell the program its window changed (the kernel sends SIGWINCH).
pub fn resize(master: RawFd, rows: u16, cols: u16) {
    set_size(master, rows, cols);
}

/// The process group in the foreground of the pty — the program, as
/// opposed to the supervisor that leads the session.
pub fn foreground_group(master: RawFd) -> Option<i32> {
    let mut group: libc::pid_t = 0;
    // SAFETY: TIOCGPGRP writes a pid_t.
    let rc = unsafe { libc::ioctl(master, libc::TIOCGPGRP as _, &mut group) };
    (rc == 0 && group > 1).then_some(group)
}
