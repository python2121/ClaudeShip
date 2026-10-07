//! Being the terminal for a session: keystrokes up, screen bytes down,
//! untouched in both directions, until the session ends or this terminal
//! goes away (which only detaches — the session keeps running in the hub).

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

use serde_json::{Value, json};

use super::client::{fail, write_all_fd};
use super::terminal_size;
use crate::frame::{self, FrameDecoder, Kind, PROTOCOL};
use crate::term::stream::TerminalStream;

/// Written to by the signal handler; the loop polls the other end. A
/// handler may only do async-signal-safe work, and a one-byte write to a
/// pipe is the classic way to get the news out.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    let byte = sig as u8;
    // SAFETY: write is async-signal-safe; the byte lives on this frame.
    unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
}

const DISCONNECTED: &str =
    "disconnected from the hub. If it is still running, so is the session: claudeship hub status";

struct Terminal {
    original: libc::termios,
    raw: bool,
    /// Mirrors what the session has switched on in *this* terminal, so it
    /// can be switched back off if we leave before the program does.
    screen: TerminalStream,
}

impl Terminal {
    fn restore(&mut self) {
        if !self.raw {
            return;
        }
        let reset = self.screen.modes().reset_sequence();
        if !reset.is_empty() {
            write_all_fd(1, &reset);
        }
        // SAFETY: restoring the attributes read at the start.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.original) };
        self.raw = false;
    }

    fn leave(&mut self, code: i32, note: Option<&str>) -> ! {
        self.restore();
        if let Some(note) = note {
            eprint!("\r\n[claudeship: {note}]\r\n");
        }
        std::process::exit(code);
    }
}

pub fn run(stream: UnixStream, hello: Value, clear_first: bool) -> ! {
    let fd = stream.as_raw_fd();

    let mut pipe = [0; 2];
    // SAFETY: pipe fills two descriptors; the handlers are installed only
    // once the write end is published.
    unsafe {
        if libc::pipe(pipe.as_mut_ptr()) != 0 {
            fail("pipe failed");
        }
        for &p in &pipe {
            crate::pty::set_cloexec(p);
        }
        crate::pty::set_nonblocking(pipe[1]);
        SIGNAL_PIPE.store(pipe[1], Ordering::Relaxed);
        // Each handler reports its own signal number. SIGINT and SIGQUIT
        // never come from the keyboard in raw mode, but a `kill -INT` from
        // outside must still leave the terminal as it was found.
        for sig in [
            libc::SIGWINCH,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
        ] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // Raw before the hub is even asked: from here on a keystroke is input
    // for the session (it waits in the tty until we forward it), not a
    // Ctrl+C that would kill this client and strand a session the hub has
    // already started.
    // SAFETY: termios is plain data.
    let mut term = Terminal {
        original: unsafe { std::mem::zeroed() },
        raw: false,
        screen: TerminalStream::new(),
    };
    // SAFETY: tcgetattr/cfmakeraw/tcsetattr on stdin with structs we own.
    unsafe {
        if libc::tcgetattr(0, &mut term.original) == 0 {
            let mut raw = term.original;
            libc::cfmakeraw(&mut raw);
            term.raw = libc::tcsetattr(0, libc::TCSANOW, &raw) == 0;
        }
    }
    if !write_all_fd(fd, &frame::encode_json(Kind::Hello, &hello)) {
        term.restore();
        fail("lost the hub");
    }

    let mut attached = false;
    let mut decoder = FrameDecoder::new();
    let mut buffer = vec![0u8; 65_536];
    loop {
        // Keystrokes aren't read until the hub has answered; until then
        // they wait in the tty.
        let mut fds = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 0,
                events: if attached { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: pipe[0],
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: three pollfds on this frame.
        if unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) } < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            term.leave(1, Some("poll failed"));
        }

        if fds[2].revents != 0 {
            // SAFETY: reading into our buffer.
            let n = unsafe { libc::read(pipe[0], buffer.as_mut_ptr().cast(), 64) };
            if n > 0 {
                let signals = &buffer[..n as usize];
                if let Some(&fatal) = signals.iter().find(|&&s| i32::from(s) != libc::SIGWINCH) {
                    term.leave(128 + i32::from(fatal), None);
                }
                if let Some((rows, cols)) = terminal_size() {
                    write_all_fd(
                        fd,
                        &frame::encode_json(Kind::Resize, &json!({"rows": rows, "cols": cols})),
                    );
                }
            }
        }

        if fds[0].revents != 0 {
            // SAFETY: reading into our buffer.
            let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n < 0 && matches!(errno(), libc::EINTR | libc::EAGAIN) {
                continue;
            }
            let frames = if n > 0 {
                decoder.feed(&buffer[..n as usize])
            } else {
                None
            };
            let Some(frames) = frames else {
                term.leave(1, Some(DISCONNECTED));
            };
            for (kind, payload) in frames {
                match kind {
                    Kind::Attached => {
                        let theirs = frame::json(&payload)
                            .get("protocol")
                            .and_then(Value::as_i64);
                        if theirs != Some(i64::from(PROTOCOL)) {
                            term.leave(
                                1,
                                Some(
                                    "the running hub is a different build than this command — the session \
                                     it just started is still there (claudeship hub status)",
                                ),
                            );
                        }
                        attached = true;
                        if clear_first {
                            write_all_fd(1, b"\x1b[H\x1b[2J");
                        }
                    }
                    Kind::Output => {
                        term.screen.feed(&payload);
                        if !write_all_fd(1, &payload) {
                            term.leave(1, None);
                        }
                    }
                    Kind::Exit => {
                        let code = frame::json(&payload)
                            .get("code")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        term.leave(code as i32, None);
                    }
                    Kind::Error => {
                        term.restore();
                        let message = frame::json(&payload)
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("the hub refused")
                            .to_string();
                        fail(&message);
                    }
                    _ => {}
                }
            }
        }

        if fds[1].revents != 0 {
            // SAFETY: reading into our buffer.
            let n = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n < 0 && matches!(errno(), libc::EINTR | libc::EAGAIN) {
                continue;
            }
            // The terminal is gone. Leave quietly; the session stays.
            if n <= 0 {
                term.leave(0, None);
            }
            if !write_all_fd(fd, &frame::encode(Kind::Input, &buffer[..n as usize])) {
                term.leave(1, Some(DISCONNECTED));
            }
        }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
