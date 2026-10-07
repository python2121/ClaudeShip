//! Tells what a person did at a terminal from what the terminal said on its
//! own. Terminals answer the program's queries, report focus changes, and
//! (with motion tracking on) report the pointer merely passing over — all
//! through the same input stream as keystrokes. Only the person's actions
//! should make a screen the one the session is sized for; otherwise two
//! attached screens, each answering the same query, would take the size
//! from each other forever.

pub fn is_user_activity(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            return true; // an ordinary key
        }
        if i + 1 >= bytes.len() {
            return true; // the Esc key
        }
        match bytes[i + 1] {
            0x5b => {
                // CSI
                let mut end = i + 2;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                if end >= bytes.len() {
                    return true;
                }
                if bytes[end] == 0x4d && end == i + 2 {
                    // Legacy (X10) mouse: three raw bytes follow the M.
                    if end + 3 >= bytes.len() {
                        return true;
                    }
                    let button = i64::from(bytes[end + 1]) - 32;
                    if !(button & 32 != 0 && button & 64 == 0 && button & 3 == 3) {
                        return true;
                    }
                    i = end + 4;
                    continue;
                }
                if !is_report(&bytes[i + 2..end], bytes[end]) {
                    return true;
                }
                i = end + 1;
            }
            0x5d | 0x50 | 0x5f | 0x5e | 0x58 => {
                // OSC / DCS / APC / PM / SOS: always an answer
                let mut end = i + 2;
                let mut terminated = false;
                while end < bytes.len() {
                    if bytes[end] == 0x07 {
                        terminated = true;
                        end += 1;
                        break;
                    }
                    if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == 0x5c {
                        terminated = true;
                        end += 2;
                        break;
                    }
                    end += 1;
                }
                if !terminated {
                    return true;
                }
                i = end;
            }
            _ => return true, // Alt+key and the like
        }
    }
    false
}

fn is_report(params: &[u8], last: u8) -> bool {
    match last {
        // focus in / out
        0x49 | 0x4f => params.is_empty(),
        // cursor position report
        0x52 => {
            !params.is_empty()
                && params
                    .iter()
                    .all(|&b| b.is_ascii_digit() || b == 0x3b || b == 0x3f)
        }
        // device attributes
        0x63 => matches!(params.first(), Some(0x3f | 0x3e)),
        // status reports, window reports
        0x6e | 0x74 => true,
        // DECRPM
        0x79 => params.contains(&0x24),
        // kitty keyboard flags report
        0x75 => params.first() == Some(&0x3f),
        // SGR mouse: only bare pointer motion is not an action
        0x4d | 0x6d => {
            if params.first() != Some(&0x3c) {
                return false;
            }
            let text = String::from_utf8_lossy(&params[1..]);
            let Some(button) = text
                .split(';')
                .find(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
            else {
                return false;
            };
            button & 32 != 0 && button & 64 == 0 && button & 3 == 3
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity(text: &str) -> bool {
        is_user_activity(text.as_bytes())
    }

    #[test]
    fn whose_keystroke_was_that() {
        let cases: &[(&str, bool, &str)] = &[
            ("a", true, "input: a key"),
            ("\r", true, "input: return"),
            ("\x1b", true, "input: the Esc key"),
            ("\x1b[A", true, "input: an arrow key"),
            ("\x1b[200~pasted\x1b[201~", true, "input: a paste"),
            ("\x1bb", true, "input: Alt+key"),
            ("\x1b[<0;10;5M", true, "input: a mouse click"),
            ("\x1b[<64;10;5M", true, "input: the scroll wheel"),
            ("\x1b[<32;10;5M", true, "input: a drag"),
            ("\x1b[<35;10;5M", false, "input: the pointer merely moving"),
            ("\x1b[I", false, "input: focus gained"),
            ("\x1b[O", false, "input: focus lost"),
            ("\x1b[?62;22c", false, "input: device attributes answer"),
            ("\x1b[24;80R", false, "input: cursor position answer"),
            ("\x1b[?1u", false, "input: keyboard flags answer"),
            ("\x1b[?2026;2$y", false, "input: mode report"),
            (
                "\x1b]11;rgb:1515/1414/1313\x1b\\\x1b[?997;1n",
                false,
                "input: color answers",
            ),
            (
                "\x1bP>|xterm.js(5.5.0)\x1b\\",
                false,
                "input: version answer",
            ),
            ("\x1b[I\x1b[?62c", false, "input: several reports together"),
            ("\x1b[Ix", true, "input: a report followed by a key"),
            ("", false, "input: nothing"),
        ];
        for &(text, want, name) in cases {
            assert_eq!(activity(text), want, "{name}");
        }
    }

    #[test]
    fn legacy_encoded_mouse() {
        assert!(
            !is_user_activity(&[0x1b, 0x5b, 0x4d, 32 + 35, 40, 40]),
            "input: legacy-encoded pointer motion"
        );
        assert!(
            is_user_activity(&[0x1b, 0x5b, 0x4d, 32, 40, 40]),
            "input: legacy-encoded click"
        );
        assert!(
            is_user_activity(&[0x1b, 0x5b, 0x4d, 32 + 35, 40]),
            "a truncated legacy report counts as typing"
        );
    }

    #[test]
    fn unterminated_answers_count_as_typing() {
        assert!(activity("\x1b]11;rgb:1515/1414"), "an OSC cut off mid-way");
        assert!(activity("\x1b[24;80"), "a CSI cut off mid-way");
    }

    #[test]
    fn modified_keys_are_typing() {
        assert!(activity("\x1b[1;5A"), "Ctrl+Up");
        assert!(activity("\x1b[97;5u"), "kitty-encoded Ctrl+a");
        assert!(
            activity("\x1b[5I"),
            "focus with parameters is not a focus report"
        );
    }
}
