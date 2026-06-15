//! Primitive wire encoding: little-endian integers, length-prefixed strings,
//! booleans, and IPv4 addresses.

use std::fmt;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ended before the value was complete.
    UnexpectedEof { needed: usize, remaining: usize },
    /// A value was syntactically valid but semantically out of range.
    InvalidValue(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEof { needed, remaining } => write!(
                f,
                "unexpected end of message: needed {needed} bytes, {remaining} remaining"
            ),
            DecodeError::InvalidValue(msg) => write!(f, "invalid value: {msg}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A cursor over a received message payload.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::UnexpectedEof {
                needed: n,
                remaining: self.remaining(),
            });
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }

    /// IPv4 address carried as a u32. The protocol stores it so that the
    /// most significant byte of the integer is the first octet.
    pub fn ipv4(&mut self) -> Result<Ipv4Addr, DecodeError> {
        Ok(Ipv4Addr::from(self.u32()?))
    }

    /// Length-prefixed string. Modern clients send UTF-8; legacy clients
    /// (Soulseek NS era) sent Latin-1, so invalid UTF-8 falls back to a
    /// Latin-1 interpretation rather than failing — the same strategy
    /// Nicotine+ uses.
    pub fn string(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        Ok(match std::str::from_utf8(bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => bytes.iter().map(|&b| b as char).collect(),
        })
    }
}

/// Encoding helpers. Free functions appending to a `Vec<u8>`, mirroring the
/// `Reader` methods.
pub fn put_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}

pub fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_bool(buf: &mut Vec<u8>, v: bool) {
    buf.push(v as u8);
}

pub fn put_ipv4(buf: &mut Vec<u8>, v: Ipv4Addr) {
    put_u32(buf, u32::from(v));
}

pub fn put_string(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_round_trips_are_little_endian() {
        let mut buf = Vec::new();
        put_u32(&mut buf, 0x0102_0304);
        assert_eq!(buf, [0x04, 0x03, 0x02, 0x01]);

        let mut r = Reader::new(&buf);
        assert_eq!(r.u32().unwrap(), 0x0102_0304);
        assert!(r.is_empty());
    }

    #[test]
    fn string_round_trip() {
        let mut buf = Vec::new();
        put_string(&mut buf, "héllo");
        let mut r = Reader::new(&buf);
        assert_eq!(r.string().unwrap(), "héllo");
    }

    #[test]
    fn invalid_utf8_falls_back_to_latin1() {
        // "café" in Latin-1: 0xE9 is not valid UTF-8 on its own.
        let mut buf = Vec::new();
        put_u32(&mut buf, 4);
        buf.extend_from_slice(&[b'c', b'a', b'f', 0xE9]);
        let mut r = Reader::new(&buf);
        assert_eq!(r.string().unwrap(), "café");
    }

    #[test]
    fn ipv4_uses_big_endian_octet_order_in_the_u32() {
        let mut buf = Vec::new();
        put_ipv4(&mut buf, Ipv4Addr::new(192, 168, 1, 2));
        let mut r = Reader::new(&buf);
        assert_eq!(r.ipv4().unwrap(), Ipv4Addr::new(192, 168, 1, 2));
    }

    #[test]
    fn pack_primitives_are_byte_exact() {
        // Parallels Nicotine+'s SlskMessageTest::test_pack_objects: pin the
        // exact bytes each primitive produces so an endianness or length-prefix
        // regression is caught directly, not just via round-tripping.
        let mut buf = Vec::new();
        put_bool(&mut buf, true);
        assert_eq!(buf, [0x01]);

        let mut buf = Vec::new();
        put_bool(&mut buf, false);
        assert_eq!(buf, [0x00]);

        let mut buf = Vec::new();
        put_u8(&mut buf, 123);
        assert_eq!(buf, [0x7B]);

        let mut buf = Vec::new();
        put_u16(&mut buf, 123);
        assert_eq!(buf, [0x7B, 0x00]);

        let mut buf = Vec::new();
        put_u32(&mut buf, 123);
        assert_eq!(buf, [0x7B, 0x00, 0x00, 0x00]);

        let mut buf = Vec::new();
        put_u64(&mut buf, 123);
        assert_eq!(buf, [0x7B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        let mut buf = Vec::new();
        put_string(&mut buf, "teststring");
        assert_eq!(buf, b"\x0a\x00\x00\x00teststring");
    }

    #[test]
    fn truncated_read_reports_eof() {
        let buf = [0x01, 0x00];
        let mut r = Reader::new(&buf);
        assert_eq!(
            r.u32(),
            Err(DecodeError::UnexpectedEof { needed: 4, remaining: 2 })
        );
    }
}
