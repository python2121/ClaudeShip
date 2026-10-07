//! Terminal byte streams: what a program's output does to a terminal
//! (`modes`, `stream`), what can be replayed to one that attaches late
//! (`replay`), and what a person did versus what a terminal said on its own
//! (`input`).

pub mod input;
pub mod modes;
pub mod replay;
pub mod stream;
