//! A stand-in for `claude` in the integration tests (CLAUDESHIP_CMD).
//! The first argument picks what it does; every mode but `blast` puts the
//! terminal in raw mode first, as Claude Code does.
//!
//!   echo            write back whatever is typed; `q` quits
//!   alt             set bracketed paste and mouse mode, wipe the screen
//!                   (ESC[2J ESC[3J), switch to the alt screen, ask DA1, echo
//!   size            print `SIZE <rows> <cols>` now and on every SIGWINCH, echo
//!   count <n>       print READY, read n bytes, print `GOT <n> sum=<sum of bytes>`, exit 0
//!   exit <code>     print BYE and exit with that code
//!   ignore-hup      ignore SIGHUP and wait forever
//!   tstp            stop itself with SIGTSTP; print CONTINUED if a SIGCONT
//!                   followed (NOT-STOPPED if the stop was discarded), echo
//!   blast <mb>      write that many megabytes, then DONE, exit 0
//!   argv ...        print the arguments' raw bytes joined by `|`, exit 0
//!   fg              print FOREGROUND if it started in the terminal's
//!                   foreground group (else BACKGROUND), exit 0
//!   fds             print `FDS` and every open descriptor above 2, exit 0
//!   sleep <s>       print START, sleep, print END, exit 0 (jobs)
//!   ignore-term     ignore SIGTERM, print IGNORING-TERM, wait forever (jobs)
//!   ticks <n> <ms>  print `TICK <i>` n times, ms apart, exit 0 (jobs)
//!   -p <prompt> …   `claude -p` with stream-json (jobs): a `system` init and
//!                   an `assistant` line, then the prompt's command, then a
//!                   `result` "echo: <prompt>" (plus " resumed" under
//!                   `--resume`), the session id the resumed one or a new
//!                   one. Prompt commands: `sleep <s>`, `ignore-term`
//!                   (forever), `mb <n>` (n MB of assistant lines),
//!                   `exit <code>`, `stderr` (a line on stderr)

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static WINCH: AtomicBool = AtomicBool::new(false);
static CONTINUED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

extern "C" fn on_cont(_: libc::c_int) {
    CONTINUED.store(true, Ordering::Relaxed);
}

fn handle(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    // SAFETY: the handler only stores to an atomic. No SA_RESTART, so a
    // blocked read returns EINTR and the loop notices.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(sig, &action, std::ptr::null_mut());
    }
}

fn out(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

fn raw_mode() {
    // SAFETY: termios on fd 0, a struct on this frame.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) == 0 {
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }
}

fn print_size() {
    // SAFETY: TIOCGWINSZ writes a winsize.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(0, libc::TIOCGWINSZ as _, &mut size) };
    out(format!("SIZE {} {}\r\n", size.ws_row, size.ws_col).as_bytes());
}

/// One read from stdin; `None` at EOF or error, empty after a signal.
fn read_some(buffer: &mut [u8]) -> Option<usize> {
    // SAFETY: reading into our buffer.
    let n = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
    if n > 0 {
        return Some(n as usize);
    }
    if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
        return Some(0);
    }
    None
}

fn echo_loop() -> ! {
    let mut buffer = [0u8; 4096];
    loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            print_size();
        }
        let Some(n) = read_some(&mut buffer) else {
            std::process::exit(0)
        };
        if n == 0 {
            continue;
        }
        if buffer[..n].contains(&b'q') {
            std::process::exit(0);
        }
        out(&buffer[..n]);
    }
}

fn main() {
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if raw.first().is_some_and(|a| a == "argv") {
        use std::os::unix::ffi::OsStrExt;
        let joined: Vec<&[u8]> = raw[1..].iter().map(|a| a.as_bytes()).collect();
        out(&joined.join(&b'|'));
        return;
    }
    if raw.first().is_some_and(|a| a == "fg") {
        // Before anything else touches the terminal.
        // SAFETY: plain queries.
        let foreground = unsafe { libc::tcgetpgrp(0) == libc::getpgrp() };
        out(if foreground {
            b"FOREGROUND\r\n"
        } else {
            b"BACKGROUND\r\n"
        });
        return;
    }
    if raw.first().is_some_and(|a| a == "fds") {
        let open: Vec<String> = (3..1024)
            // SAFETY: probing descriptor numbers.
            .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0)
            .map(|fd: i32| fd.to_string())
            .collect();
        out(format!("FDS [{}]\r\n", open.join(",")).as_bytes());
        return;
    }
    let args: Vec<String> = raw
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mode = args.first().map(String::as_str).unwrap_or("echo");
    if mode == "-p" {
        print_mode(&args);
    }
    match mode {
        "sleep" => {
            out(b"START\n");
            std::thread::sleep(std::time::Duration::from_secs_f64(
                args.get(1).and_then(|a| a.parse().ok()).unwrap_or(1.0),
            ));
            out(b"END\n");
            return;
        }
        "ignore-term" => {
            // SAFETY: setting a disposition.
            unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
            out(b"IGNORING-TERM\n");
            loop {
                // SAFETY: pause has no preconditions.
                unsafe { libc::pause() };
            }
        }
        "ticks" => {
            let n: u64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(3);
            let ms: u64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(100);
            for i in 0..n {
                out(format!("TICK {i}\n").as_bytes());
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            return;
        }
        _ => {}
    }
    let number = |default: u64| args.get(1).and_then(|a| a.parse().ok()).unwrap_or(default);
    if mode != "blast" {
        raw_mode();
    }
    match mode {
        "alt" => {
            out(b"\x1b[?2004h\x1b[?1000hPRE\r\n\x1b[2J\x1b[3J\x1b[?1049hALT-SCREEN\x1b[c\r\n");
            echo_loop();
        }
        "size" => {
            handle(libc::SIGWINCH, on_winch);
            print_size();
            echo_loop();
        }
        "count" => {
            let want = number(8192) as usize;
            // Only now raw: anything typed before this was cooked-mode input.
            out(b"READY\r\n");
            let (mut got, mut sum) = (0usize, 0u64);
            let mut buffer = [0u8; 4096];
            while got < want {
                let Some(n) = read_some(&mut buffer) else {
                    break;
                };
                got += n;
                sum += buffer[..n].iter().map(|&b| u64::from(b)).sum::<u64>();
            }
            out(format!("GOT {got} sum={sum}\r\n").as_bytes());
        }
        "exit" => {
            out(b"BYE\r\n");
            std::process::exit(number(0) as i32);
        }
        "ignore-hup" => {
            // SAFETY: setting a disposition.
            unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
            out(b"IGNORING\r\n");
            loop {
                // SAFETY: pause has no preconditions.
                unsafe { libc::pause() };
            }
        }
        "tstp" => {
            handle(libc::SIGCONT, on_cont);
            out(b"STOPPING\r\n");
            // SAFETY: signalling ourselves.
            unsafe { libc::kill(libc::getpid(), libc::SIGTSTP) };
            // A stop takes effect before kill returns; if we get here
            // without a SIGCONT, the stop was discarded.
            std::thread::sleep(std::time::Duration::from_millis(50));
            if CONTINUED.load(Ordering::Relaxed) {
                out(b"CONTINUED\r\n");
            } else {
                out(b"NOT-STOPPED\r\n");
            }
            echo_loop();
        }
        "blast" => {
            let chunk = vec![b'x'; 1 << 20];
            for _ in 0..number(1) {
                out(&chunk);
            }
            out(b"\r\nDONE\r\n");
        }
        _ => {
            out(b"READY\r\n");
            echo_loop();
        }
    }
}

/// `claude -p <prompt> … --output-format stream-json [--resume <id>]`.
fn print_mode(args: &[String]) -> ! {
    let prompt = args.get(1).cloned().unwrap_or_default();
    let value_of = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let resumed = value_of("--resume");
    let session = resumed.clone().unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0) as u64;
        let pid = u64::from(std::process::id());
        format!(
            "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
            (nanos >> 16) as u32,
            (pid & 0xffff) as u16,
            nanos & 0xfff,
            (nanos >> 12) & 0xfff,
            nanos & 0xffff_ffff_ffff
        )
    });
    let line = |v: serde_json::Value| out(format!("{v}\n").as_bytes());
    line(serde_json::json!({
        "type": "system", "subtype": "init", "session_id": session,
        "permissionMode": value_of("--permission-mode"), "cwd": std::env::current_dir().ok(),
    }));
    line(serde_json::json!({"type": "assistant", "session_id": session, "message": {"content": [{"type": "text", "text": prompt}]}}));
    let words: Vec<&str> = prompt.split_whitespace().collect();
    let number = |i: usize| words.get(i).and_then(|w| w.parse::<f64>().ok()).unwrap_or(1.0);
    let mut code = 0;
    match words.first().copied() {
        Some("sleep") => std::thread::sleep(std::time::Duration::from_secs_f64(number(1))),
        Some("ignore-term") => {
            // SAFETY: setting a disposition; pause has no preconditions.
            unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
            loop {
                unsafe { libc::pause() };
            }
        }
        Some("mb") => {
            let filler = "y".repeat(1000);
            for _ in 0..(number(1) as usize * 1000) {
                line(serde_json::json!({"type": "assistant", "text": filler}));
            }
        }
        Some("exit") => code = number(1) as i32,
        Some("stderr") => eprintln!("a line on stderr"),
        _ => {}
    }
    let mut text = format!("echo: {prompt}");
    if resumed.is_some() {
        text.push_str(" resumed");
    }
    line(serde_json::json!({
        "type": "result", "subtype": "success", "is_error": code != 0,
        "result": text, "session_id": session,
    }));
    std::process::exit(code);
}
