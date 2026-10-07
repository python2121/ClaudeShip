//! Framing between the hub and a terminal client on the Unix socket: one
//! type byte, a big-endian u32 length, then the payload. Control frames
//! carry JSON; input/output frames carry raw terminal bytes untouched.

use serde_json::{Map, Value};

/// Bumped whenever the hub, the terminal client, or the web page would
/// misunderstand an older peer. The hub outlives installs (it owns live
/// sessions), so a new client or page can meet an old hub; each side
/// states this number and says so plainly when they differ.
pub const PROTOCOL: u32 = 3;

pub const MAX_PAYLOAD: usize = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    /// client → hub: JSON {op, …}
    Hello = 0x48,
    /// client → hub: keystrokes
    Input = 0x49,
    /// client → hub: JSON {rows, cols}
    Resize = 0x52,
    /// hub → client: JSON {id}
    Attached = 0x41,
    /// hub → client: terminal output
    Output = 0x4F,
    /// hub → client: JSON {code}
    Exit = 0x58,
    /// hub → client: JSON {message}
    Error = 0x45,
    /// hub → client: JSON answer to a one-shot op
    Reply = 0x4C,
}

impl Kind {
    pub fn from_byte(byte: u8) -> Option<Kind> {
        Some(match byte {
            0x48 => Kind::Hello,
            0x49 => Kind::Input,
            0x52 => Kind::Resize,
            0x41 => Kind::Attached,
            0x4F => Kind::Output,
            0x58 => Kind::Exit,
            0x45 => Kind::Error,
            0x4C => Kind::Reply,
            _ => return None,
        })
    }
}

pub fn encode(kind: Kind, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(kind as u8);
    let n = u32::try_from(payload.len()).expect("frame payload over 4 GB");
    frame.extend_from_slice(&n.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub fn encode_json(kind: Kind, object: &Value) -> Vec<u8> {
    encode(
        kind,
        &serde_json::to_vec(object).unwrap_or_else(|_| b"{}".to_vec()),
    )
}

/// A control frame's JSON object; empty when it isn't one.
pub fn json(payload: &[u8]) -> Map<String, Value> {
    match serde_json::from_slice(payload) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Reassembles frames from a byte stream that splits them arbitrarily.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The complete frames now available, or `None` when the stream is not
    /// speaking the protocol (unknown type, oversized length) and the
    /// connection should be dropped.
    pub fn feed(&mut self, data: &[u8]) -> Option<Vec<(Kind, Vec<u8>)>> {
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();
        let mut at = 0;
        while self.buffer.len() - at >= 5 {
            let head = &self.buffer[at..at + 5];
            let kind = Kind::from_byte(head[0])?;
            let length = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
            if length > MAX_PAYLOAD {
                return None;
            }
            if self.buffer.len() - at < 5 + length {
                break;
            }
            frames.push((kind, self.buffer[at + 5..at + 5 + length].to_vec()));
            at += 5 + length;
        }
        self.buffer.drain(..at);
        Some(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_split_across_reads() {
        let hello = encode_json(Kind::Hello, &json!({"op": "status"}));
        let output = encode(Kind::Output, &[0x1b, 0x5b, 0x48, 0x00, 0xff]);
        let stream = [hello, output].concat();
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.feed(&stream[..7]).map(|f| f.len()),
            Some(0),
            "frames: a partial frame yields nothing yet"
        );
        let rest = decoder.feed(&stream[7..]).expect("valid stream");
        assert_eq!(
            rest.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![Kind::Hello, Kind::Output],
            "frames: both frames once the bytes arrive"
        );
        assert_eq!(
            json(&rest[0].1).get("op").and_then(Value::as_str),
            Some("status"),
            "frames: JSON payload round-trips"
        );
        assert_eq!(
            rest[1].1,
            vec![0x1b, 0x5b, 0x48, 0x00, 0xff],
            "frames: binary payload untouched"
        );
    }

    #[test]
    fn unknown_type_byte_is_a_protocol_error() {
        assert!(FrameDecoder::new().feed(b"GET / HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn oversized_length_is_a_protocol_error() {
        assert!(
            FrameDecoder::new()
                .feed(&[0x4f, 0x7f, 0xff, 0xff, 0xff])
                .is_none()
        );
        let mut at_limit = vec![0x4f];
        at_limit.extend_from_slice(&(MAX_PAYLOAD as u32).to_be_bytes());
        assert_eq!(
            FrameDecoder::new().feed(&at_limit).map(|f| f.len()),
            Some(0),
            "exactly the limit is allowed (and waits)"
        );
    }

    #[test]
    fn header_layout_and_empty_payload() {
        assert_eq!(encode(Kind::Exit, b""), vec![0x58, 0, 0, 0, 0]);
        assert_eq!(&encode(Kind::Input, &[7; 0x0102])[..5], &[0x49, 0, 0, 1, 2]);
        let mut decoder = FrameDecoder::new();
        let frames = decoder.feed(&encode(Kind::Exit, b"")).unwrap();
        assert_eq!(frames, vec![(Kind::Exit, vec![])]);
    }

    #[test]
    fn byte_at_a_time() {
        let stream = [
            encode(Kind::Resize, br#"{"rows":1}"#),
            encode(Kind::Reply, b"{}"),
        ]
        .concat();
        let mut decoder = FrameDecoder::new();
        let mut frames = Vec::new();
        for b in &stream {
            frames.extend(decoder.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(
            frames.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![Kind::Resize, Kind::Reply]
        );
    }

    #[test]
    fn kind_bytes_match_the_swift_hub() {
        for (kind, byte) in [
            (Kind::Hello, b'H'),
            (Kind::Input, b'I'),
            (Kind::Resize, b'R'),
            (Kind::Attached, b'A'),
            (Kind::Output, b'O'),
            (Kind::Exit, b'X'),
            (Kind::Error, b'E'),
            (Kind::Reply, b'L'),
        ] {
            assert_eq!(kind as u8, byte);
            assert_eq!(Kind::from_byte(byte), Some(kind));
        }
    }

    #[test]
    fn json_of_a_non_object_is_empty() {
        assert!(json(b"[1,2]").is_empty());
        assert!(json(b"not json").is_empty());
    }
}
