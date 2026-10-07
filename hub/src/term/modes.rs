//! The terminal modes a program has switched on, as far as they matter for
//! handing the screen to another terminal: a client that attaches late
//! needs them replayed, and one that detaches early needs them undone.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalModes {
    /// DEC private modes (`CSI ? n h/l`) from `TRACKED` that have been set
    /// or reset explicitly. Ordered, so the sequences below come out sorted.
    pub dec: BTreeMap<i64, bool>,
    /// Kitty keyboard protocol flag stack (`CSI > flags u` / `CSI < n u`).
    pub kitty: Vec<i64>,
    /// xterm modifyOtherKeys level (`CSI > 4 ; n m`).
    pub modify_other_keys: i64,
}

impl TerminalModes {
    pub const ALT_SCREEN: [i64; 3] = [47, 1047, 1049];
    /// Cursor keys, cursor visibility, alt screen, mouse reporting, focus
    /// reporting, bracketed paste, color-scheme change reports.
    pub const TRACKED: [i64; 14] = [
        1, 25, 47, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1047, 1049, 2004, 2031,
    ];

    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_tracked(mode: i64) -> bool {
        Self::TRACKED.contains(&mode)
    }

    fn is_set(&self, mode: i64) -> bool {
        self.dec.get(&mode) == Some(&true)
    }

    pub fn in_alt_screen(&self) -> bool {
        Self::ALT_SCREEN.iter().any(|&m| self.is_set(m))
    }

    /// Takes a terminal in its default state to this one.
    pub fn restore_sequence(&self) -> Vec<u8> {
        let mut s = String::new();
        for &mode in &Self::ALT_SCREEN {
            if self.is_set(mode) {
                s += &format!("\x1b[?{mode}h");
            }
        }
        for (&mode, &on) in &self.dec {
            if Self::ALT_SCREEN.contains(&mode) {
                continue;
            }
            if mode == 25 {
                if !on {
                    s += "\x1b[?25l";
                }
            } else if on {
                s += &format!("\x1b[?{mode}h");
            }
        }
        for flags in &self.kitty {
            s += &format!("\x1b[>{flags}u");
        }
        if self.modify_other_keys > 0 {
            s += &format!("\x1b[>4;{}m", self.modify_other_keys);
        }
        s.into_bytes()
    }

    /// Takes a terminal in this state back to its defaults. Empty when
    /// nothing is set, so a clean exit writes nothing extra.
    pub fn reset_sequence(&self) -> Vec<u8> {
        let mut s = String::new();
        if !self.kitty.is_empty() {
            s += &format!("\x1b[<{}u", self.kitty.len());
        }
        if self.modify_other_keys > 0 {
            s += "\x1b[>4;0m";
        }
        for (&mode, &on) in &self.dec {
            if Self::ALT_SCREEN.contains(&mode) {
                continue;
            }
            if mode == 25 {
                if !on {
                    s += "\x1b[?25h";
                }
            } else if on {
                s += &format!("\x1b[?{mode}l");
            }
        }
        for &mode in &Self::ALT_SCREEN {
            if self.is_set(mode) {
                s += &format!("\x1b[?{mode}l");
            }
        }
        if !s.is_empty() {
            s += "\x1b[0m";
        }
        s.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_needs_no_restore() {
        assert!(TerminalModes::new().restore_sequence().is_empty());
        assert!(TerminalModes::new().reset_sequence().is_empty());
    }

    #[test]
    fn cursor_shown_explicitly_needs_nothing() {
        // `?25h` is the default: neither restoring nor resetting writes it.
        let mut m = TerminalModes::new();
        m.dec.insert(25, true);
        assert!(m.restore_sequence().is_empty());
        assert!(m.reset_sequence().is_empty());
    }

    #[test]
    fn modes_reset_explicitly_need_nothing() {
        let mut m = TerminalModes::new();
        m.dec.insert(2004, false);
        m.dec.insert(1049, false);
        assert!(!m.in_alt_screen());
        assert!(m.restore_sequence().is_empty());
    }
}
