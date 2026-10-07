//! The recent output of a session, kept so a terminal that attaches later
//! can be brought to the same screen. Bounded: when it overflows, whole
//! chunks drop off the front and `base` remembers the modes in force at the
//! new start, so the replay still begins from the right terminal state.

use std::collections::VecDeque;

use super::modes::TerminalModes;

const CHUNK_TARGET: usize = 32_768;
pub const DEFAULT_CAPACITY: usize = 4 << 20;

#[derive(Clone, Debug)]
struct Chunk {
    data: Vec<u8>,
    /// The terminal state once `data` has been written.
    modes: TerminalModes,
}

#[derive(Clone, Debug)]
pub struct ReplayBuffer {
    chunks: VecDeque<Chunk>,
    base: TerminalModes,
    total: usize,
    capacity: usize,
}

impl Default for ReplayBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl ReplayBuffer {
    pub fn new(capacity: usize) -> Self {
        ReplayBuffer {
            chunks: VecDeque::new(),
            base: TerminalModes::new(),
            total: 0,
            capacity,
        }
    }

    #[allow(dead_code)] // the web state (phase 4)
    pub fn base(&self) -> &TerminalModes {
        &self.base
    }

    #[allow(dead_code)] // the web state (phase 4)
    pub fn total(&self) -> usize {
        self.total
    }

    #[allow(dead_code)] // the web state (phase 4)
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// `modes_after` is the terminal state once `data` has been written.
    pub fn append(&mut self, data: &[u8], modes_after: &TerminalModes) {
        if data.is_empty() {
            return;
        }
        match self.chunks.back_mut() {
            Some(last) if last.data.len() + data.len() <= CHUNK_TARGET => {
                last.data.extend_from_slice(data);
                last.modes = modes_after.clone();
            }
            _ => self.chunks.push_back(Chunk {
                data: data.to_vec(),
                modes: modes_after.clone(),
            }),
        }
        self.total += data.len();
        while self.total > self.capacity && self.chunks.len() > 1 {
            let dropped = self.chunks.pop_front().expect("more than one chunk");
            self.total -= dropped.data.len();
            self.base = dropped.modes;
        }
    }

    /// Start over after the program cleared screen and scrollback.
    pub fn reset(&mut self, base: TerminalModes) {
        let clear = b"\x1b[H\x1b[2J\x1b[3J".to_vec();
        self.total = clear.len();
        self.chunks = VecDeque::from([Chunk {
            data: clear,
            modes: base.clone(),
        }]);
        self.base = base;
    }

    /// Everything a freshly reset terminal needs to show the current screen.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut data = self.base.restore_sequence();
        data.reserve(self.total);
        for chunk in &self.chunks {
            data.extend_from_slice(&chunk.data);
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_from_the_front_and_restores_modes() {
        let mut replay = ReplayBuffer::new(100_000);
        let mut set = TerminalModes::new();
        set.dec.insert(2004, true);
        replay.append(&[0x61; 40_000], &set);
        replay.append(&[0x62; 40_000], &set);
        assert_eq!(
            replay.snapshot().len(),
            80_000,
            "replay: under capacity keeps everything, no preamble"
        );
        replay.append(&[0x63; 40_000], &set);
        let trimmed = replay.snapshot();
        assert!(
            trimmed.len() < 100_100,
            "replay: over capacity drops from the front"
        );
        assert_eq!(
            &trimmed[..8],
            b"\x1b[?2004h",
            "replay: trimmed replay opens by restoring modes"
        );
        assert_eq!(
            trimmed.last(),
            Some(&0x63),
            "replay: newest output survives"
        );
        replay.reset(TerminalModes::new());
        replay.append(b"x", &TerminalModes::new());
        assert_eq!(
            replay.snapshot(),
            b"\x1b[H\x1b[2J\x1b[3Jx",
            "replay: reset starts from a cleared screen"
        );
    }

    #[test]
    fn small_writes_coalesce_into_chunks() {
        let mut replay = ReplayBuffer::new(100_000);
        for _ in 0..1000 {
            replay.append(&[0x61; 100], &TerminalModes::new());
        }
        assert_eq!(replay.chunks.len(), 4, "100 KB in 32 KB chunks");
        assert_eq!(replay.total(), 100_000);
    }

    #[test]
    fn a_single_oversized_chunk_is_kept() {
        let mut replay = ReplayBuffer::new(10);
        replay.append(&[0x61; 50], &TerminalModes::new());
        assert_eq!(
            replay.snapshot().len(),
            50,
            "the newest chunk is never dropped"
        );
        replay.append(b"", &TerminalModes::new());
        assert_eq!(replay.total(), 50, "empty writes ignored");
    }

    #[test]
    fn reset_carries_the_modes() {
        let mut replay = ReplayBuffer::default();
        let mut set = TerminalModes::new();
        set.dec.insert(2004, true);
        replay.append(b"old", &TerminalModes::new());
        replay.reset(set.clone());
        assert_eq!(replay.base(), &set);
        assert_eq!(replay.snapshot(), b"\x1b[?2004h\x1b[H\x1b[2J\x1b[3J");
    }
}
