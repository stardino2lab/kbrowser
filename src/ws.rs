//! Minimal RFC 6455 WebSocket client for plaintext localhost (Chrome CDP).

use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

const MAX_MESSAGE: u64 = 256 * 1024 * 1024;

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

pub enum Msg {
    Text(String),
    #[allow(dead_code)] // CDP is text-only today
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Close,
}

pub struct WsWriter {
    s: TcpStream,
    seed: u64,
}

pub struct WsReader {
    r: BufReader<TcpStream>,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Build one client frame (FIN set, masked).
pub fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 14);
    f.push(0x80 | opcode);
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n <= 0xFFFF {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    f
}

/// Read one frame: (fin, opcode, unmasked payload).
pub fn decode_frame(r: &mut impl Read) -> io::Result<(bool, u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h)?;
    let fin = h[0] & 0x80 != 0;
    let opcode = h[0] & 0x0F;
    let masked = h[1] & 0x80 != 0;
    let mut len = (h[1] & 0x7F) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        len = u64::from_be_bytes(b);
    }
    if len > MAX_MESSAGE {
        return Err(bad("websocket frame too large"));
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    Ok((fin, opcode, payload))
}

pub fn connect(port: u16, path: &str) -> io::Result<(WsReader, WsWriter)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    // No Origin header: Chrome would otherwise demand --remote-allow-origins.
    let key = crate::util::base64_encode(&crate::util::random_bytes(16));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes())?;
    // Read the response head byte by byte so no frame bytes are consumed.
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b)?;
        head.push(b[0]);
        if head.len() > 16 * 1024 {
            return Err(bad("websocket handshake too long"));
        }
    }
    let head = String::from_utf8_lossy(&head);
    if !head.starts_with("HTTP/1.1 101") {
        return Err(bad(&format!(
            "websocket handshake rejected: {}",
            head.lines().next().unwrap_or("")
        )));
    }
    s.set_read_timeout(None)?;
    let seed = u64::from_le_bytes(crate::util::random_bytes(8).try_into().unwrap()) | 1;
    Ok((WsReader { r: BufReader::with_capacity(1 << 16, s.try_clone()?) }, WsWriter { s, seed }))
}

impl WsWriter {
    fn next_mask(&mut self) -> [u8; 4] {
        // xorshift64; masking is anti-proxy-cache obfuscation, not security.
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed as u32).to_le_bytes()
    }
    pub fn send(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mask = self.next_mask();
        self.s.write_all(&encode_frame(opcode, payload, mask))
    }
    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send(OP_TEXT, text.as_bytes())
    }
    pub fn shutdown(&self) {
        let _ = self.s.shutdown(Shutdown::Both);
    }
}

impl WsReader {
    /// Next complete message, with fragments reassembled.
    pub fn read(&mut self) -> io::Result<Msg> {
        let mut acc: Vec<u8> = Vec::new();
        let mut kind = 0u8;
        loop {
            let (fin, opcode, payload) = decode_frame(&mut self.r)?;
            match opcode {
                OP_PING => return Ok(Msg::Ping(payload)),
                OP_PONG => continue,
                OP_CLOSE => return Ok(Msg::Close),
                OP_TEXT | OP_BINARY => {
                    kind = opcode;
                    acc = payload;
                }
                OP_CONT => {
                    if acc.len() as u64 + payload.len() as u64 > MAX_MESSAGE {
                        return Err(bad("websocket message too large"));
                    }
                    acc.extend_from_slice(&payload);
                }
                _ => return Err(bad("unknown websocket opcode")),
            }
            if fin {
                return Ok(if kind == OP_BINARY {
                    Msg::Binary(acc)
                } else {
                    Msg::Text(String::from_utf8_lossy(&acc).into_owned())
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(n: usize) {
        let payload: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
        let frame = encode_frame(OP_TEXT, &payload, [1, 2, 3, 4]);
        let (fin, op, got) = decode_frame(&mut Cursor::new(frame)).unwrap();
        assert!(fin);
        assert_eq!(op, OP_TEXT);
        assert_eq!(got, payload);
    }

    #[test]
    fn frame_lengths() {
        for n in [0, 1, 125, 126, 127, 65535, 65536, 200_000] {
            roundtrip(n);
        }
    }

    #[test]
    fn header_layout() {
        let f = encode_frame(OP_TEXT, b"Hello", [0x37, 0xfa, 0x21, 0x3d]);
        // RFC 6455 section 5.7 masked "Hello" example.
        assert_eq!(f, [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]);
        assert_eq!(encode_frame(OP_TEXT, &[0; 126], [0; 4])[1], 0x80 | 126);
        assert_eq!(encode_frame(OP_TEXT, &[0; 70000], [0; 4])[1], 0x80 | 127);
    }

    #[test]
    fn unmasked_server_frame() {
        let (fin, op, p) = decode_frame(&mut Cursor::new(vec![0x81, 0x05, b'H', b'e', b'l', b'l', b'o'])).unwrap();
        assert!(fin && op == OP_TEXT);
        assert_eq!(p, b"Hello");
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut f = vec![0x82, 127];
        f.extend_from_slice(&(u64::MAX).to_be_bytes());
        assert!(decode_frame(&mut Cursor::new(f)).is_err());
    }
}
