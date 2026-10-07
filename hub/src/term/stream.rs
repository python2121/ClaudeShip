//! Walks a terminal output stream one escape sequence at a time. It keeps
//! `modes` current, and it returns the stream as it should be *replayed* to
//! a terminal that attaches later: identical, minus the sequences that must
//! not happen twice — queries (the new terminal would answer a question
//! asked long ago, and the answer would land in the program's input),
//! clipboard writes, notifications, and bells.
//!
//! Live clients always get the raw bytes; only the replay copy is filtered.

use super::modes::TerminalModes;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Bytes(Vec<u8>),
    /// The program wiped the screen and scrollback: nothing before this
    /// point is visible in any attached terminal, so the replay buffer
    /// can start over from these modes.
    Clear(TerminalModes),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringKind {
    Osc,
    Dcs,
    Apc,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    EscIntermediate,
    Csi,
    String(StringKind),
    StringEsc(StringKind),
    /// A string too long to hold: passed through unclassified.
    Pass(StringKind),
    PassEsc,
}

#[derive(Clone, Copy, Debug)]
struct ClearCandidate {
    /// 2 for `ESC[2J`, 3 for `ESC[3J`.
    kind: u8,
    /// Other sequences seen since.
    tokens: u32,
}

#[derive(Clone, Debug)]
pub struct TerminalStream {
    state: State,
    /// Bytes of the sequence being read; empty in ground state.
    pending: Vec<u8>,
    modes: TerminalModes,
    /// Half of an `ESC[2J` + `ESC[3J` pair, and how many other sequences
    /// have gone by since it.
    clear_candidate: Option<ClearCandidate>,
}

impl Default for TerminalStream {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_CSI: usize = 128;
const MAX_STRING: usize = 65_536;

fn int(text: &str) -> Option<i64> {
    text.parse().ok()
}

/// Swift's `split(separator:)`: empty pieces dropped.
fn fields(text: &str) -> impl Iterator<Item = &str> {
    text.split(';').filter(|s| !s.is_empty())
}

impl TerminalStream {
    pub fn new() -> Self {
        TerminalStream {
            state: State::Ground,
            pending: Vec::new(),
            modes: TerminalModes::new(),
            clear_candidate: None,
        }
    }

    pub fn modes(&self) -> &TerminalModes {
        &self.modes
    }

    /// The bytes of a sequence still being read — consumed from the stream
    /// but not yet part of any returned piece. A client that attaches
    /// between two reads needs these after the replay: the live stream it
    /// joins resumes mid-sequence, and without the first half the rest
    /// would print as text.
    pub fn pending_bytes(&self) -> Vec<u8> {
        let mut bytes = self.pending.clone();
        if matches!(self.state, State::StringEsc(_) | State::PassEsc) {
            bytes.push(0x1b);
        }
        bytes
    }

    pub fn feed(&mut self, input: &[u8]) -> Vec<Piece> {
        let mut out = Vec::with_capacity(input.len());
        let mut pieces = Vec::new();
        for &byte in input {
            self.step(byte, &mut out, &mut pieces);
        }
        if !out.is_empty() {
            pieces.push(Piece::Bytes(out));
        }
        pieces
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>, pieces: &mut Vec<Piece>) {
        match self.state {
            State::Ground => {
                if b == 0x1b {
                    self.state = State::Esc;
                    self.pending = vec![b];
                } else {
                    self.clear_candidate = None;
                    if b != 0x07 {
                        out.push(b); // a replayed bell would ring again
                    }
                }
            }

            State::Esc => match b {
                0x5b => self.open(b, State::Csi),
                0x5d => self.open(b, State::String(StringKind::Osc)),
                0x50 => self.open(b, State::String(StringKind::Dcs)),
                0x5f => self.open(b, State::String(StringKind::Apc)),
                0x58 | 0x5e => self.open(b, State::String(StringKind::Other)),
                0x1b => {
                    out.append(&mut self.pending);
                    self.pending = vec![b];
                }
                0x20..=0x2f => self.open(b, State::EscIntermediate),
                _ => {
                    if b == 0x63 {
                        self.modes = TerminalModes::new(); // RIS: full reset
                    }
                    self.pending.push(b);
                    self.emit_pending(out);
                }
            },

            State::EscIntermediate => {
                if b == 0x1b {
                    out.append(&mut self.pending);
                    self.pending = vec![b];
                    self.state = State::Esc;
                } else {
                    self.pending.push(b);
                    if b >= 0x30 || self.pending.len() > 16 {
                        self.emit_pending(out);
                    }
                }
            }

            State::Csi => {
                if b == 0x1b {
                    out.append(&mut self.pending);
                    self.pending = vec![b];
                    self.state = State::Esc;
                } else if (0x40..=0x7e).contains(&b) {
                    self.pending.push(b);
                    self.finish_csi(out, pieces);
                } else {
                    self.pending.push(b);
                    if self.pending.len() > MAX_CSI {
                        self.emit_pending(out);
                    }
                }
            }

            State::String(kind) => {
                if b == 0x07 && kind == StringKind::Osc {
                    self.pending.push(b);
                    self.finish_string(kind, 1, out);
                } else if b == 0x1b {
                    self.state = State::StringEsc(kind);
                } else {
                    self.pending.push(b);
                    if self.pending.len() > MAX_STRING {
                        out.append(&mut self.pending);
                        self.state = State::Pass(kind);
                    }
                }
            }

            State::StringEsc(kind) => {
                if b == 0x5c {
                    self.pending.extend_from_slice(&[0x1b, 0x5c]);
                    self.finish_string(kind, 2, out);
                } else {
                    // ESC without `\` aborts the string and starts a new sequence.
                    out.append(&mut self.pending);
                    self.pending = vec![0x1b];
                    self.state = State::Esc;
                    self.step(b, out, pieces);
                }
            }

            State::Pass(kind) => {
                if b == 0x1b {
                    self.state = State::PassEsc;
                } else {
                    out.push(b);
                    if b == 0x07 && kind == StringKind::Osc {
                        self.state = State::Ground;
                    }
                }
            }

            State::PassEsc => {
                if b == 0x5c {
                    out.extend_from_slice(&[0x1b, 0x5c]);
                    self.state = State::Ground;
                } else {
                    self.pending = vec![0x1b];
                    self.state = State::Esc;
                    self.step(b, out, pieces);
                }
            }
        }
    }

    fn open(&mut self, b: u8, state: State) {
        self.pending.push(b);
        self.state = state;
    }

    fn emit_pending(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.pending);
        self.state = State::Ground;
        self.bump_token();
    }

    fn bump_token(&mut self) {
        if let Some(mut candidate) = self.clear_candidate {
            candidate.tokens += 1;
            self.clear_candidate = if candidate.tokens > 2 {
                None
            } else {
                Some(candidate)
            };
        }
    }

    fn finish_csi(&mut self, out: &mut Vec<u8>, pieces: &mut Vec<Piece>) {
        let n = self.pending.len();
        let last = self.pending[n - 1];
        let params = self.pending[2..n - 1].to_vec();
        self.track(&params, last);

        let clear_kind = match (last, params.as_slice()) {
            (0x4a, b"2") => Some(2),
            (0x4a, b"3") => Some(3),
            _ => None,
        };
        match clear_kind {
            Some(kind) if !self.modes.in_alt_screen() => {
                if let Some(candidate) = self.clear_candidate
                    && candidate.kind != kind
                {
                    // Screen and scrollback both wiped: everything so far is gone
                    // from every attached terminal, so drop it from the replay too.
                    out.clear();
                    *pieces = vec![Piece::Clear(self.modes.clone())];
                    self.clear_candidate = None;
                    self.pending.clear();
                    self.state = State::Ground;
                    return;
                }
                self.clear_candidate = Some(ClearCandidate { kind, tokens: 0 });
            }
            _ => self.bump_token(),
        }

        if !Self::is_query_csi(&params, last) {
            out.extend_from_slice(&self.pending);
        }
        self.pending.clear();
        self.state = State::Ground;
    }

    fn finish_string(&mut self, kind: StringKind, terminator_length: usize, out: &mut Vec<u8>) {
        let n = self.pending.len();
        let payload = &self.pending[2..n - terminator_length];
        if !Self::is_replay_unsafe_string(kind, payload) {
            out.extend_from_slice(&self.pending);
        }
        self.pending.clear();
        self.state = State::Ground;
        self.bump_token();
    }

    fn track(&mut self, params: &[u8], last: u8) {
        let Some((&lead, rest)) = params.split_first() else {
            return;
        };
        let rest = String::from_utf8_lossy(rest);
        let rest = rest.as_ref();
        let modes = &mut self.modes;
        match (lead, last) {
            // CSI ? … h / l
            (0x3f, 0x68) | (0x3f, 0x6c) => {
                for part in fields(rest) {
                    if let Some(mode) = int(part)
                        && TerminalModes::is_tracked(mode)
                    {
                        modes.dec.insert(mode, last == 0x68);
                    }
                }
            }
            // CSI > flags u — push
            (0x3e, 0x75) => {
                if modes.kitty.len() < 16 {
                    modes
                        .kitty
                        .push(fields(rest).next().and_then(int).unwrap_or(0));
                }
            }
            // CSI < n u — pop
            (0x3c, 0x75) => {
                let n = int(rest).unwrap_or(1).max(1);
                let n = usize::try_from(n)
                    .unwrap_or(usize::MAX)
                    .min(modes.kitty.len());
                let keep = modes.kitty.len() - n;
                modes.kitty.truncate(keep);
            }
            // CSI = flags ; mode u — change the current flags
            (0x3d, 0x75) => {
                let Some(top) = modes.kitty.last_mut() else {
                    return;
                };
                let parts: Vec<&str> = rest.split(';').collect();
                let flags = int(parts[0]).unwrap_or(0);
                let mode = if parts.len() > 1 {
                    int(parts[1]).unwrap_or(1)
                } else {
                    1
                };
                match mode {
                    2 => *top |= flags,
                    3 => *top &= !flags,
                    _ => *top = flags,
                }
            }
            // CSI > 4 ; n m — modifyOtherKeys; bare CSI > m resets
            (0x3e, 0x6d) => {
                let parts: Vec<&str> = rest.split(';').collect();
                if rest.is_empty() || parts[0] == "4" {
                    modes.modify_other_keys = if parts.len() > 1 {
                        int(parts[1]).unwrap_or(0)
                    } else {
                        0
                    };
                }
            }
            // CSI > 4 n — modifyOtherKeys off
            (0x3e, 0x6e) if rest.is_empty() || rest == "4" => modes.modify_other_keys = 0,
            _ => {}
        }
    }

    /// CSI sequences that ask the terminal to report something back.
    pub fn is_query_csi(params: &[u8], last: u8) -> bool {
        match last {
            0x63 => true,                          // DA1/2/3
            0x6e => params.first() != Some(&0x3e), // DSR — but CSI > n disables a key-modifier option
            0x75 => params.first() == Some(&0x3f), // kitty keyboard query: CSI ? u
            0x70 => params.contains(&0x24),        // DECRQM: CSI [?] n $ p
            0x71 => params.first() == Some(&0x3e), // XTVERSION: CSI > q
            0x78 => !params.contains(&0x24),       // DECREQTPARM (but `$ x` is a rectangle fill)
            0x74 => {
                // window reports: CSI 11/13–21 t
                let text = String::from_utf8_lossy(params);
                fields(&text)
                    .next()
                    .and_then(int)
                    .is_some_and(|n| n == 11 || (13..=21).contains(&n))
            }
            _ => false,
        }
    }

    /// OSC/DCS/APC strings that must not be replayed: queries, clipboard
    /// writes (OSC 52), and desktop notifications (OSC 9 / 99 / 777; OSC 9;4
    /// is a progress bar, which is state and stays).
    fn is_replay_unsafe_string(kind: StringKind, payload: &[u8]) -> bool {
        let text = String::from_utf8_lossy(payload);
        match kind {
            StringKind::Osc => {
                let fields: Vec<&str> = text.split(';').collect();
                let code = fields[0];
                if fields[1..].contains(&"?") {
                    return true;
                }
                if code == "52" || code == "99" || code == "777" {
                    return true;
                }
                if code == "9" {
                    return !(fields.len() > 1 && fields[1] == "4");
                }
                false
            }
            StringKind::Dcs => text.starts_with("+q") || text.starts_with("$q"),
            StringKind::Apc => text.starts_with('G') && text.contains("a=q"),
            StringKind::Other => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: &str = "\x1b";

    fn replayed(pieces: &[Piece]) -> String {
        pieces
            .iter()
            .map(|p| match p {
                Piece::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                Piece::Clear(_) => "<CLEAR>".to_string(),
            })
            .collect()
    }

    fn s(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn modes_tracked_restored_and_reset() {
        let mut st = TerminalStream::new();
        st.feed(
            format!(
                "{ESC}[?1049h{ESC}[?25l{ESC}[?2004h{ESC}[?1004h{ESC}[>1u{ESC}[>4;2m{ESC}[?1004l"
            )
            .as_bytes(),
        );
        let m = st.modes();
        assert_eq!(m.dec.get(&1049), Some(&true), "modes: alt screen set");
        assert_eq!(m.dec.get(&25), Some(&false), "modes: cursor hidden");
        assert_eq!(
            m.dec.get(&1004),
            Some(&false),
            "modes: focus reporting set then reset"
        );
        assert_eq!(m.kitty, vec![1], "modes: kitty flags pushed");
        assert_eq!(m.modify_other_keys, 2, "modes: modifyOtherKeys level");
        assert_eq!(
            s(&m.restore_sequence()),
            format!("{ESC}[?1049h{ESC}[?25l{ESC}[?2004h{ESC}[>1u{ESC}[>4;2m"),
            "modes: restore enters alt screen first"
        );
        assert_eq!(
            s(&m.reset_sequence()),
            format!("{ESC}[<1u{ESC}[>4;0m{ESC}[?25h{ESC}[?2004l{ESC}[?1049l{ESC}[0m"),
            "modes: reset leaves alt screen last"
        );
        st.feed(format!("{ESC}[<u{ESC}[>4m{ESC}[?1049l{ESC}[?25h{ESC}[?2004l").as_bytes());
        assert!(
            st.modes().reset_sequence().is_empty(),
            "modes: nothing to undo after the program cleans up"
        );
        assert!(
            TerminalModes::new().restore_sequence().is_empty(),
            "modes: default state needs no restore"
        );
    }

    #[test]
    fn split_sequence_reassembled_in_order() {
        let mut st = TerminalStream::new();
        let a = st.feed(format!("ab{ESC}[?20").as_bytes());
        let b = st.feed(b"04hcd");
        assert_eq!(replayed(&a) + &replayed(&b), format!("ab{ESC}[?2004hcd"));
        assert_eq!(
            st.modes().dec.get(&2004),
            Some(&true),
            "stream: split sequence still tracked"
        );
    }

    #[test]
    fn queries_and_one_shot_effects_dropped_from_replay() {
        let mut st = TerminalStream::new();
        let noisy = format!(
            "A{ESC}[c{ESC}[>0q{ESC}[6n{ESC}[?u{ESC}[?2026$p{ESC}[14tB\
             {ESC}]11;?\x07{ESC}]52;c;aGk=\x07{ESC}]9;done{ESC}\\{ESC}]777;notify;x;y\x07\
             {ESC}P+q544e{ESC}\\{ESC}_Gi=1,a=q;AAAA{ESC}\\\x07C"
        );
        assert_eq!(replayed(&st.feed(noisy.as_bytes())), "ABC");
    }

    #[test]
    fn styling_titles_progress_and_other_state_kept() {
        let mut st = TerminalStream::new();
        let kept = format!(
            "{ESC}[1;31mred{ESC}[0m{ESC}]0;title\x07{ESC}]9;4;1;50\x07{ESC}[2 q{ESC}[8;30;100t{ESC}[1;1;2;2$x{ESC}(B"
        );
        assert_eq!(replayed(&st.feed(kept.as_bytes())), kept);
    }

    #[test]
    fn soft_reset_is_not_a_query() {
        assert!(!TerminalStream::is_query_csi(b"!", 0x70));
    }

    #[test]
    fn csi_gt_4_n_is_a_setting_not_a_status_query() {
        assert!(!TerminalStream::is_query_csi(b">4", 0x6e));
    }

    #[test]
    fn window_report_queries_by_number() {
        assert!(TerminalStream::is_query_csi(b"11", 0x74));
        assert!(TerminalStream::is_query_csi(b"18", 0x74));
        assert!(
            TerminalStream::is_query_csi(b";14", 0x74),
            "empty fields skipped as in Swift"
        );
        assert!(
            !TerminalStream::is_query_csi(b"22;0", 0x74),
            "title stack push is state"
        );
        assert!(!TerminalStream::is_query_csi(b"", 0x74));
    }

    #[test]
    fn half_a_sequence_is_pending() {
        let mut st = TerminalStream::new();
        st.feed(format!("text{ESC}[38;2;12").as_bytes());
        assert_eq!(
            s(&st.pending_bytes()),
            format!("{ESC}[38;2;12"),
            "stream: half a CSI is pending"
        );
        st.feed(b";34;56m");
        assert!(
            st.pending_bytes().is_empty(),
            "stream: nothing pending once it completes"
        );
        st.feed(format!("{ESC}]0;title{ESC}").as_bytes());
        assert_eq!(
            s(&st.pending_bytes()),
            format!("{ESC}]0;title{ESC}"),
            "stream: a string caught at its terminator's ESC"
        );
    }

    #[test]
    fn modify_other_keys_turned_off() {
        let mut st = TerminalStream::new();
        st.feed(format!("{ESC}[>4;2m{ESC}[>4n").as_bytes());
        assert_eq!(
            st.modes().modify_other_keys,
            0,
            "modes: CSI > 4 n turns modifyOtherKeys off"
        );
        st.feed(format!("{ESC}[>4;1m{ESC}[>m").as_bytes());
        assert_eq!(
            st.modes().modify_other_keys,
            0,
            "modes: bare CSI > m resets it"
        );
    }

    #[test]
    fn kitty_stack_push_pop_and_change() {
        let mut st = TerminalStream::new();
        st.feed(format!("{ESC}[>1u{ESC}[>3u{ESC}[=4;2u").as_bytes());
        assert_eq!(st.modes().kitty, vec![1, 7], "set bits on the top entry");
        st.feed(format!("{ESC}[=2;3u").as_bytes());
        assert_eq!(st.modes().kitty, vec![1, 5], "clear bits on the top entry");
        st.feed(format!("{ESC}[=9u").as_bytes());
        assert_eq!(st.modes().kitty, vec![1, 9], "replace the top entry");
        st.feed(format!("{ESC}[<5u").as_bytes());
        assert!(
            st.modes().kitty.is_empty(),
            "popping more than the stack holds empties it"
        );
        st.feed(format!("{ESC}[=9u").as_bytes());
        assert!(
            st.modes().kitty.is_empty(),
            "changing an empty stack does nothing"
        );
        for _ in 0..20 {
            st.feed(format!("{ESC}[>1u").as_bytes());
        }
        assert_eq!(st.modes().kitty.len(), 16, "the stack is bounded");
    }

    #[test]
    fn untracked_dec_modes_ignored() {
        let mut st = TerminalStream::new();
        st.feed(format!("{ESC}[?7;2004;12h").as_bytes());
        assert_eq!(st.modes().dec.len(), 1);
        assert_eq!(st.modes().dec.get(&2004), Some(&true));
    }

    #[test]
    fn ris_resets_modes() {
        let mut st = TerminalStream::new();
        st.feed(format!("{ESC}[?2004h{ESC}c").as_bytes());
        assert_eq!(st.modes(), &TerminalModes::new());
    }

    #[test]
    fn clear_pair_drops_what_came_before() {
        let mut st = TerminalStream::new();
        st.feed(format!("{ESC}[?2004h").as_bytes());
        let cleared = st.feed(format!("old{ESC}[2J{ESC}[3J{ESC}[Hnew").as_bytes());
        assert_eq!(replayed(&cleared), format!("<CLEAR>{ESC}[Hnew"));
        match cleared.first() {
            Some(Piece::Clear(m)) => assert_eq!(
                m.dec.get(&2004),
                Some(&true),
                "stream: clear carries the modes in force"
            ),
            _ => panic!("stream: clear piece present"),
        }
    }

    #[test]
    fn clear_pair_in_either_order_and_split_across_reads() {
        let mut st = TerminalStream::new();
        let a = st.feed(format!("old{ESC}[3J").as_bytes());
        assert_eq!(replayed(&a), format!("old{ESC}[3J"));
        let b = st.feed(format!("{ESC}[H{ESC}[2Jnew").as_bytes());
        assert_eq!(
            replayed(&b),
            "<CLEAR>new",
            "one sequence between the two is tolerated"
        );
    }

    #[test]
    fn clear_pair_tolerates_two_sequences_but_not_three() {
        let mut st = TerminalStream::new();
        let two = format!("{ESC}[2J{ESC}[H{ESC}[0m{ESC}[3J");
        assert_eq!(replayed(&st.feed(two.as_bytes())), "<CLEAR>");
        let mut st = TerminalStream::new();
        let three = format!("{ESC}[2J{ESC}[H{ESC}[0m{ESC}[1m{ESC}[3J");
        assert_eq!(replayed(&st.feed(three.as_bytes())), three);
    }

    #[test]
    fn text_between_the_two_is_not_a_clear_pair() {
        let mut st = TerminalStream::new();
        let not_cleared = format!("{ESC}[2Jtext{ESC}[3J");
        assert_eq!(replayed(&st.feed(not_cleared.as_bytes())), not_cleared);
    }

    #[test]
    fn a_clear_inside_the_alt_screen_keeps_the_main_screen_replay() {
        let mut st = TerminalStream::new();
        let alt_clear = format!("{ESC}[?1049h{ESC}[2J{ESC}[3J");
        assert_eq!(replayed(&st.feed(alt_clear.as_bytes())), alt_clear);
    }

    #[test]
    fn same_clear_twice_is_not_a_pair() {
        let mut st = TerminalStream::new();
        let twice = format!("{ESC}[2J{ESC}[2J");
        assert_eq!(replayed(&st.feed(twice.as_bytes())), twice);
    }

    #[test]
    fn esc_without_backslash_aborts_a_string() {
        let mut st = TerminalStream::new();
        // The aborted OSC is passed through as-is, then the CSI is read normally.
        let input = format!("{ESC}]0;ti{ESC}[6nX");
        assert_eq!(replayed(&st.feed(input.as_bytes())), format!("{ESC}]0;tiX"));
    }

    #[test]
    fn overlong_string_passes_through_unclassified() {
        let mut st = TerminalStream::new();
        let body = "a".repeat(MAX_STRING + 10);
        let input = format!("{ESC}]52;c;{body}\x07Z");
        // Too long to classify: replayed whole (it was never held).
        assert_eq!(replayed(&st.feed(input.as_bytes())), input);
        assert!(st.pending_bytes().is_empty());
        let mut st = TerminalStream::new();
        let input = format!("{ESC}P{body}{ESC}\\Z");
        assert_eq!(replayed(&st.feed(input.as_bytes())), input);
    }

    #[test]
    fn overlong_csi_passes_through() {
        let mut st = TerminalStream::new();
        let input = format!("{ESC}[{}c", "1;".repeat(100));
        assert_eq!(
            replayed(&st.feed(input.as_bytes())),
            input,
            "too long to be a query we classify"
        );
    }

    #[test]
    fn bells_dropped_but_osc_bell_terminator_kept() {
        let mut st = TerminalStream::new();
        let input = format!("a\x07b{ESC}]2;t\x07");
        assert_eq!(
            replayed(&st.feed(input.as_bytes())),
            format!("ab{ESC}]2;t\x07")
        );
    }

    #[test]
    fn double_escape_flushes_the_first() {
        let mut st = TerminalStream::new();
        let input = format!("{ESC}{ESC}[1m");
        assert_eq!(replayed(&st.feed(input.as_bytes())), input);
    }

    #[test]
    fn charset_designation_kept() {
        let mut st = TerminalStream::new();
        let input = format!("{ESC}(0q{ESC}(B");
        assert_eq!(replayed(&st.feed(input.as_bytes())), input);
    }
}
