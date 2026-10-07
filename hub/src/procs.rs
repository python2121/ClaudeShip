//! Process-tree questions: a pid's parent and its chain of ancestors, and
//! this machine's name. The one place with a per-OS answer for each.

/// The parent of `pid`, or `None` if it is gone (or unreadable).
#[cfg(target_os = "macos")]
pub fn parent_pid(pid: i32) -> Option<i32> {
    // `struct kinfo_proc` isn't in the libc crate; the field we want is
    // `kp_eproc.e_ppid`, at the same offset on arm64 and x86_64 (measured
    // with offsetof against the SDK headers: 560 of 648 bytes).
    const SIZE: usize = 648;
    const PPID_OFFSET: usize = 560;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buffer = [0u8; SIZE];
    let mut length = SIZE;
    // SAFETY: mib and buffer are valid for the lengths passed.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    // A pid that doesn't exist answers success with zero bytes.
    if rc != 0 || length != SIZE {
        return None;
    }
    let bytes: [u8; 4] = buffer[PPID_OFFSET..PPID_OFFSET + 4].try_into().ok()?;
    Some(i32::from_ne_bytes(bytes))
}

/// The parent of `pid`: field 4 of `/proc/<pid>/stat`, read after the
/// command name's closing parenthesis (the name may contain anything).
#[cfg(not(target_os = "macos"))]
pub fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_ppid(&stat)
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_stat_ppid(stat: &[u8]) -> Option<i32> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
    // After ")": state, then ppid.
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `pid` and up to `limit` of its ancestors, nearest first, stopping
/// before init/launchd.
pub fn ancestor_pids(pid: i32, limit: usize) -> Vec<i32> {
    let mut chain = vec![pid];
    let mut current = pid;
    for _ in 0..limit {
        match parent_pid(current) {
            Some(parent) if parent > 1 => {
                chain.push(parent);
                current = parent;
            }
            _ => break,
        }
    }
    chain
}

/// The default ancestry depth (the Swift walk's).
pub const ANCESTRY_LIMIT: usize = 16;

/// This machine's name, as a person would call it: `gethostname`, with
/// macOS's `.local` suffix dropped.
#[allow(dead_code)] // the web directory (phase 4) shows it
pub fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its length.
    let rc = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    if cfg!(target_os = "macos") {
        name.strip_suffix(".local")
            .map(str::to_string)
            .unwrap_or(name)
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_parent_is_found() {
        let me = std::process::id() as i32;
        // SAFETY: getppid has no preconditions.
        let parent = unsafe { libc::getppid() };
        assert_eq!(parent_pid(me), Some(parent));
        let chain = ancestor_pids(me, ANCESTRY_LIMIT);
        assert_eq!(chain[0], me);
        if parent > 1 {
            assert_eq!(chain.get(1), Some(&parent));
        }
        assert!(!chain.contains(&1), "the walk stops before init/launchd");
    }

    #[test]
    fn a_dead_pid_has_no_parent() {
        assert_eq!(parent_pid(i32::MAX - 7), None);
    }

    #[test]
    fn stat_line_with_a_hostile_name() {
        assert_eq!(parse_stat_ppid(b"42 (a) b (c) S 17 42 42 0"), Some(17));
        assert_eq!(parse_stat_ppid(b"42 (x"), None);
    }

    #[test]
    fn hostname_has_no_local_suffix() {
        assert!(!hostname().ends_with(".local"));
    }
}
