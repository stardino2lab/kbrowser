//! CLI <-> daemon wire format: `[u32 LE length][u8 type][payload]`.
//! Length covers the type byte plus payload. Stage 1 uses JSON frames only;
//! the type byte leaves room for pushed events and binary image frames.

use crate::json::{self, Value};
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: i64 = 1;
pub const T_REQUEST: u8 = 1;
pub const T_RESPONSE: u8 = 2;
#[allow(dead_code)]
pub const T_EVENT: u8 = 3;
#[allow(dead_code)]
pub const T_BINARY: u8 = 4;

const MAX_FRAME: usize = 64 * 1024 * 1024;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(payload.len() + 5);
    buf.extend_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
    buf.push(kind);
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame length"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let kind = body.remove(0);
    Ok((kind, body))
}

pub fn write_json(w: &mut impl Write, kind: u8, v: &Value) -> io::Result<()> {
    write_frame(w, kind, v.to_string().as_bytes())
}

pub fn read_json(r: &mut impl Read, want: u8) -> io::Result<Value> {
    let (kind, body) = read_frame(r)?;
    if kind != want {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected frame type"));
    }
    let text = String::from_utf8(body).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame not utf-8"))?;
    json::parse(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, T_REQUEST, b"{\"a\":1}").unwrap();
        write_frame(&mut buf, T_BINARY, &[0, 1, 2]).unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(read_frame(&mut c).unwrap(), (T_REQUEST, b"{\"a\":1}".to_vec()));
        assert_eq!(read_frame(&mut c).unwrap(), (T_BINARY, vec![0, 1, 2]));
        assert!(read_frame(&mut c).is_err());
    }

    #[test]
    fn rejects_bad_length() {
        assert!(read_frame(&mut Cursor::new(vec![0, 0, 0, 0])).is_err());
        assert!(read_frame(&mut Cursor::new(vec![0xff, 0xff, 0xff, 0xff, 1])).is_err());
    }
}
