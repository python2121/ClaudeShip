//! The leader of a session's pty (`claudeship hub supervise <argv>`): runs
//! the program as a foreground job and does the one piece of a shell's
//! work the program can't do without — resuming it when it suspends.
//!
//! A session leader with no parent in its own session is an *orphaned
//! process group*, and the kernel discards SIGTSTP sent to one. Claude Code
//! suspends on Ctrl+Z by putting the terminal back to cooked mode, printing
//! "run `fg`", and stopping itself; as a bare leader it would do the first
//! two, fail silently at the third, and sit in that half-suspended state
//! for good, since the SIGCONT that would restore its screen never comes.
//! With this process as its parent the stop is real, and is answered at
//! once with SIGCONT: Ctrl+Z becomes a redraw instead of a dead session.
//! Orphaned-group semantics are POSIX, so this holds on Linux too.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

/// The job's pid (= its process group), for the signal handlers.
static JOB: AtomicI32 = AtomicI32::new(0);

/// Seconds a hung-up job gets before it is killed: the hub's own grace
/// (`TERMINATE_GRACE`), kept here as well because after `hub stop` there is
/// no hub left to escalate, and a program that ignores SIGHUP would
/// otherwise outlive it for good, with this process, on a dead pty.
const HANGUP_GRACE: u32 = 5;

extern "C" fn forward(sig: libc::c_int) {
    let job = JOB.load(Ordering::Relaxed);
    if job > 0 {
        // SAFETY: kill and alarm are async-signal-safe.
        unsafe {
            libc::kill(-job, sig);
            if sig == libc::SIGHUP {
                libc::alarm(HANGUP_GRACE);
            }
        }
    }
}

/// SIGTERM is the hub's escalation: the program ignored the hang-up.
/// SIGALRM is the same, `HANGUP_GRACE` after one, hub or no hub.
extern "C" fn escalate(_: libc::c_int) {
    let job = JOB.load(Ordering::Relaxed);
    if job > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(-job, libc::SIGKILL) };
    }
}

/// The shell's convention: the exit status, or 128 + the fatal signal.
pub fn exit_code(status: i32) -> i32 {
    let signal = status & 0x7f;
    if signal == 0 {
        (status >> 8) & 0xff
    } else {
        128 + signal
    }
}

pub fn strerror(code: i32) -> String {
    // SAFETY: strerror returns a valid C string, copied out at once.
    unsafe {
        std::ffi::CStr::from_ptr(libc::strerror(code))
            .to_string_lossy()
            .into_owned()
    }
}

fn set_handler(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    // SAFETY: installs a handler that only calls async-signal-safe code.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(sig, &action, std::ptr::null_mut());
    }
}

pub fn run(argv: Vec<OsString>) -> ! {
    if argv.is_empty() {
        std::process::exit(2);
    }
    // SAFETY: signal-mask and disposition calls in a single-threaded
    // process; the sets live on this frame.
    let held = unsafe {
        // We hand the terminal to the job and take it back, from the
        // background; none of that may stop us.
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        // Held until the handlers below exist, so a hang-up that lands in
        // the microseconds around the spawn is forwarded rather than fatal.
        let mut held: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut held);
        for sig in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT, libc::SIGQUIT] {
            libc::sigaddset(&mut held, sig);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &held, std::ptr::null_mut());
        held
    };

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).process_group(0);
    // SAFETY: tcsetpgrp and reset_signals are async-signal-safe. The job
    // takes the terminal itself, in its own group (already set) and while
    // SIGTTOU is still ignored: done from here instead, it would race the
    // exec, and a program that sets raw mode at once would do so from the
    // background and be stopped for it. Then the job must not inherit our
    // ignored SIGTSTP/SIGTTOU/SIGTTIN, or Ctrl+Z would do nothing.
    unsafe {
        command.pre_exec(|| {
            libc::tcsetpgrp(0, libc::getpid());
            crate::pty::reset_signals();
            Ok(())
        });
    }
    let pid = match command.spawn() {
        Ok(child) => child.id() as i32,
        Err(e) => {
            let reason = e
                .raw_os_error()
                .map(strerror)
                .unwrap_or_else(|| e.to_string());
            eprint!(
                "claudeship: cannot run {}: {reason}\r\n",
                argv[0].to_string_lossy()
            );
            std::process::exit(127);
        }
    };
    // Again from this side, should the job's own call have failed.
    // SAFETY: plain libc calls; fd 0 is the pty slave.
    unsafe {
        libc::tcsetpgrp(0, pid);
    }

    // Asked to end — by the hub, or by the kernel when the hub is gone and
    // the pty with it (which hangs up only the session leader) — pass it on
    // and keep waiting. Dying here instead would revoke the terminal under
    // the program mid-cleanup, report our death as its exit status, and
    // leave a program that shrugs off SIGHUP running where the hub can no
    // longer find it.
    JOB.store(pid, Ordering::Relaxed);
    // Before SIGHUP's handler, which arms the alarm.
    set_handler(libc::SIGALRM, escalate);
    set_handler(libc::SIGTERM, escalate);
    for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
        set_handler(sig, forward);
    }
    // SAFETY: as above.
    unsafe {
        libc::sigprocmask(libc::SIG_UNBLOCK, &held, std::ptr::null_mut());
    }

    loop {
        let mut status = 0;
        // SAFETY: waitpid on our own child.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if waited < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            std::process::exit(1);
        }
        if libc::WIFSTOPPED(status) {
            // Stopped: put it back in the foreground and carry on.
            // SAFETY: as above.
            unsafe {
                libc::tcsetpgrp(0, pid);
                libc::kill(-pid, libc::SIGCONT);
            }
            continue;
        }
        // Reaped: its pid is free for reuse, so no handler may signal it.
        JOB.store(0, Ordering::Relaxed);
        std::process::exit(exit_code(status));
    }
}

#[cfg(test)]
mod tests {
    use super::exit_code;

    #[test]
    fn exit_codes_follow_the_shell() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(libc::SIGKILL), 137);
        assert_eq!(exit_code(libc::SIGHUP | 0x80), 129, "core-dump bit ignored");
    }
}
