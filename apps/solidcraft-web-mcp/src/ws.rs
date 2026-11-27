//! The server side of a WebSocket (RFC 6455), as far as the tab link needs it: the handshake's
//! accept key, text messages in and out, ping, pong and close. No extensions, no subprotocols.
//! A browser is on the other end, but the bytes are hostile all the same: every length is checked
//! against a cap before anything is allocated for it.

use std::io::{Read, Write};

use base64::Engine as _;
use sha1::Digest as _;

/// The value RFC 6455 appends to a client's key before hashing it.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`.
pub fn accept_key(key: &str) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(key.trim().as_bytes());
    hasher.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// A message from the client.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    /// A binary message: the link sends none, so its bytes are not kept.
    Binary,
    Ping(Vec<u8>),
    Pong,
    Close,
}

const CONTINUATION: u8 = 0x0;
const TEXT: u8 = 0x1;
const BINARY: u8 = 0x2;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xA;

fn bad(what: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

/// One frame: whether it ends its message, its opcode and its unmasked payload.
fn read_frame(input: &mut impl Read, max: usize) -> std::io::Result<(bool, u8, Vec<u8>)> {
    let mut start = [0u8; 2];
    input.read_exact(&mut start)?;
    let [first, second] = start;
    if first & 0x70 != 0 {
        return Err(bad("reserved bits set (no extension was negotiated)"));
    }
    let (fin, opcode) = (first & 0x80 != 0, first & 0x0F);
    // A client's frames are always masked.
    if second & 0x80 == 0 {
        return Err(bad("unmasked client frame"));
    }
    let length = match second & 0x7F {
        126 => {
            let mut wide = [0u8; 2];
            input.read_exact(&mut wide)?;
            u64::from(u16::from_be_bytes(wide))
        }
        127 => {
            let mut wide = [0u8; 8];
            input.read_exact(&mut wide)?;
            u64::from_be_bytes(wide)
        }
        short => u64::from(short),
    };
    if opcode >= CLOSE && (length > 125 || !fin) {
        return Err(bad("oversized or fragmented control frame"));
    }
    if length > max as u64 {
        return Err(bad("message too large"));
    }
    let mut mask = [0u8; 4];
    input.read_exact(&mut mask)?;
    let mut payload = vec![0u8; length as usize];
    input.read_exact(&mut payload)?;
    for (byte, key) in payload.iter_mut().zip(mask.iter().cycle()) {
        *byte ^= key;
    }
    Ok((fin, opcode, payload))
}

/// The next whole message, its fragments joined, at most `max` bytes. A control frame between
/// the fragments of a message is returned first; the message's fragments read so far are kept in
/// `partial` for the next call.
pub fn read_message(input: &mut impl Read, partial: &mut Option<(u8, Vec<u8>)>, max: usize) -> std::io::Result<Message> {
    loop {
        let held = partial.as_ref().map_or(0, |(_, bytes)| bytes.len());
        let (fin, opcode, payload) = read_frame(input, max.saturating_sub(held))?;
        match opcode {
            PING => return Ok(Message::Ping(payload)),
            PONG => return Ok(Message::Pong),
            CLOSE => return Ok(Message::Close),
            TEXT | BINARY => {
                if partial.is_some() {
                    return Err(bad("a new message began inside a fragmented one"));
                }
                *partial = Some((opcode, payload));
            }
            CONTINUATION => match partial.as_mut() {
                Some((_, bytes)) => bytes.extend_from_slice(&payload),
                None => return Err(bad("a continuation frame without a message")),
            },
            _ => return Err(bad("unknown opcode")),
        }
        if fin && let Some((kind, bytes)) = partial.take() {
            return Ok(match kind {
                TEXT => Message::Text(String::from_utf8(bytes).map_err(|_| bad("text message is not UTF-8"))?),
                _ => Message::Binary,
            });
        }
    }
}

/// Write one unfragmented, unmasked frame (a server's frames are never masked).
fn write_frame(output: &mut impl Write, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => head.push(n as u8),
        n if n <= usize::from(u16::MAX) => {
            head.push(126);
            head.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            head.push(127);
            head.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    output.write_all(&head)?;
    output.write_all(payload)?;
    output.flush()
}

pub fn write_text(output: &mut impl Write, text: &str) -> std::io::Result<()> {
    write_frame(output, TEXT, text.as_bytes())
}

pub fn write_ping(output: &mut impl Write) -> std::io::Result<()> {
    write_frame(output, PING, &[])
}

pub fn write_pong(output: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    write_frame(output, PONG, payload.get(..125.min(payload.len())).unwrap_or_default())
}

/// A close frame with status `code` (1000: normal, 1008: policy violation).
pub fn write_close(output: &mut impl Write, code: u16) -> std::io::Result<()> {
    write_frame(output, CLOSE, &code.to_be_bytes())
}

#[cfg(test)]
pub(crate) mod testing {
    //! The client's side, for the tests' stand-in tab.
    use std::io::{Read, Write};

    /// Write one masked frame, as a browser does.
    pub fn write_client_frame(output: &mut impl Write, fin: bool, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
        match payload.len() {
            n if n < 126 => out.push(0x80 | n as u8),
            n if n <= usize::from(u16::MAX) => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, k)| b ^ k));
        output.write_all(&out)?;
        output.flush()
    }

    pub fn write_client_text(output: &mut impl Write, text: &str) -> std::io::Result<()> {
        write_client_frame(output, true, super::TEXT, text.as_bytes())
    }

    /// Read one server frame (unmasked): its opcode and payload.
    pub fn read_server_frame(input: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
        let mut start = [0u8; 2];
        input.read_exact(&mut start)?;
        let length = match start[1] & 0x7F {
            126 => {
                let mut wide = [0u8; 2];
                input.read_exact(&mut wide)?;
                u64::from(u16::from_be_bytes(wide))
            }
            127 => {
                let mut wide = [0u8; 8];
                input.read_exact(&mut wide)?;
                u64::from_be_bytes(wide)
            }
            short => u64::from(short),
        };
        let mut payload = vec![0u8; length as usize];
        input.read_exact(&mut payload)?;
        Ok((start[0] & 0x0F, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn the_accept_key_is_the_one_the_rfc_gives() {
        // RFC 6455, section 1.3.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn messages_round_trip_whole_and_fragmented() {
        let mut wire = Vec::new();
        write_client_text(&mut wire, "hello").unwrap();
        write_client_frame(&mut wire, false, TEXT, b"frag").unwrap();
        write_client_frame(&mut wire, true, PING, b"p").unwrap();
        write_client_frame(&mut wire, true, CONTINUATION, b"ment").unwrap();
        let long = "x".repeat(70_000);
        write_client_text(&mut wire, &long).unwrap();
        write_client_frame(&mut wire, true, CLOSE, &[]).unwrap();
        let mut input = wire.as_slice();
        let mut partial = None;
        let mut next = || read_message(&mut input, &mut partial, 1 << 20).unwrap();
        assert_eq!(next(), Message::Text("hello".into()));
        assert_eq!(next(), Message::Ping(b"p".to_vec()), "a control frame comes through between fragments");
        assert_eq!(next(), Message::Text("fragment".into()));
        assert_eq!(next(), Message::Text(long));
        assert_eq!(next(), Message::Close);
    }

    #[test]
    fn hostile_frames_are_refused_without_allocating_for_them() {
        let read = |wire: &[u8], max: usize| read_message(&mut &wire[..], &mut None, max).map_err(|e| e.to_string());
        // Unmasked, as a server's frame would be.
        assert!(read(&[0x81, 0x01, b'x'], 100).unwrap_err().contains("unmasked"));
        // A length of 2^63 bytes.
        let mut huge = vec![0x81, 0xFF];
        huge.extend_from_slice(&(1u64 << 63).to_be_bytes());
        assert!(read(&huge, 100).unwrap_err().contains("too large"));
        // Fragments that pass the cap together.
        let mut wire = Vec::new();
        write_client_frame(&mut wire, false, TEXT, &[b'a'; 60]).unwrap();
        write_client_frame(&mut wire, true, CONTINUATION, &[b'a'; 60]).unwrap();
        assert!(read(&wire, 100).unwrap_err().contains("too large"));
        // Not UTF-8.
        let mut wire = Vec::new();
        write_client_frame(&mut wire, true, TEXT, &[0xFF, 0xFE]).unwrap();
        assert!(read(&wire, 100).unwrap_err().contains("UTF-8"));
        // A continuation with nothing to continue, and a truncated frame.
        let mut wire = Vec::new();
        write_client_frame(&mut wire, true, CONTINUATION, b"x").unwrap();
        assert!(read(&wire, 100).unwrap_err().contains("continuation"));
        assert!(read(&[0x81], 100).is_err());
    }

    #[test]
    fn server_frames_carry_their_length_in_every_size_class() {
        for n in [0usize, 125, 126, 65_535, 65_536] {
            let text = "y".repeat(n);
            let mut wire = Vec::new();
            write_text(&mut wire, &text).unwrap();
            let (opcode, payload) = read_server_frame(&mut wire.as_slice()).unwrap();
            assert_eq!((opcode, payload.len()), (TEXT, n));
        }
    }
}
