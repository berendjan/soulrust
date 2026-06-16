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

    /// The unread remainder, consuming it. Used for messages that carry an
    /// opaque trailing payload (e.g. a distributed embedded message's inner
    /// bytes).
    pub fn rest(&mut self) -> &'a [u8] {
        let slice = &self.buf[self.pos..];
        self.pos = self.buf.len();
        slice
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

    /// Decodes a Soulseek file size. The original Soulseek NS client has a bug
    /// where files larger than 2 GiB are transmitted with the low 32 bits
    /// holding the real size and the high 32 bits set to `0xFFFFFFFF` (garbage),
    /// which a naive `u64` read inflates to ~16 EiB. Nicotine+'s
    /// `SlskMessage.unpack_file_size` detects this by checking the
    /// most-significant byte (wire offset +7): if it is `255`, only the low 32
    /// bits are the size and the high word is discarded. Eight bytes are always
    /// consumed either way.
    pub fn file_size(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take(8)?;
        if bytes[7] == 255 {
            Ok(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64)
        } else {
            Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
        }
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }

    /// Reads `n` raw bytes in a single bounded copy (e.g. a picture blob),
    /// faulting up front if fewer than `n` remain.
    pub fn bytes(&mut self, n: usize) -> Result<Vec<u8>, DecodeError> {
        Ok(self.take(n)?.to_vec())
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
    fn bool_decodes_any_nonzero_byte_as_true() {
        // Nicotine+ unpack_bool is `bool(self._message[self._offset])`: it reads
        // a single byte and treats any nonzero value as true, not only 0x01.
        let buf = [0x00u8, 0x01, 0xFF];
        let mut r = Reader::new(&buf);
        assert!(!r.bool().unwrap());
        assert!(r.bool().unwrap());
        assert!(r.bool().unwrap());
        assert!(r.is_empty());
    }

    #[test]
    fn u16_reader_is_little_endian_and_two_bytes_wide() {
        // Nicotine+ UINT16_UNPACK is Struct("<H"): two little-endian bytes.
        // (Its unpack_uint16 advances the internal offset by 4, but that only
        // works because obfuscated_port — the sole uint16 — is the last field
        // in its message; the field itself is two bytes on the wire.) Pin that
        // our reader consumes exactly two bytes by reading a trailing u8 after.
        let mut buf = Vec::new();
        put_u16(&mut buf, 0x0102);
        put_u8(&mut buf, 0x7B);
        assert_eq!(buf, [0x02, 0x01, 0x7B]);

        let mut r = Reader::new(&buf);
        assert_eq!(r.u16().unwrap(), 0x0102);
        assert_eq!(r.u8().unwrap(), 0x7B);
        assert!(r.is_empty());
    }

    #[test]
    fn u64_round_trips_full_width() {
        // Nicotine+ UINT64_UNPACK is Struct("<Q"): eight little-endian bytes.
        let mut buf = Vec::new();
        put_u64(&mut buf, 0x0102_0304_0506_0708);
        assert_eq!(buf, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);

        let mut r = Reader::new(&buf);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert!(r.is_empty());
    }

    #[test]
    fn empty_string_round_trips() {
        // Nicotine+ pack_string of "" encodes UINT32_PACK(0) with no payload;
        // unpack_string then decodes a zero-length slice to "".
        let mut buf = Vec::new();
        put_string(&mut buf, "");
        assert_eq!(buf, [0x00, 0x00, 0x00, 0x00]);

        let mut r = Reader::new(&buf);
        assert_eq!(r.string().unwrap(), "");
        assert!(r.is_empty());
    }

    #[test]
    fn ipv4_is_stored_little_endian_octet_order_on_the_wire() {
        // Nicotine+ unpack_ip reverses the four wire bytes before inet_ntoa:
        // `inet_ntoa(self._message[start:start+4].tobytes()[::-1])`. So the
        // first octet of the dotted address is the LAST byte on the wire.
        // For 192.168.1.2 the wire bytes are therefore [2, 1, 168, 192].
        let mut buf = Vec::new();
        put_ipv4(&mut buf, Ipv4Addr::new(192, 168, 1, 2));
        assert_eq!(buf, [0x02, 0x01, 0xA8, 0xC0]);

        let mut r = Reader::new(&buf);
        assert_eq!(r.ipv4().unwrap(), Ipv4Addr::new(192, 168, 1, 2));
    }

    #[test]
    fn file_size_applies_soulseek_ns_overflow_workaround() {
        // Nicotine+ unpack_file_size (slskmessages.py:3208): if the
        // most-significant byte (offset +7) is 255, the high word is garbage
        // from the Soulseek NS >2 GiB bug, so only the low 32 bits are the size.
        // Wire bytes: low word = 4096, high word = 0xFFFFFFFF.
        let buf = [0x00, 0x10, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut r = Reader::new(&buf);
        assert_eq!(r.file_size().unwrap(), 4096);
        assert!(r.is_empty(), "all 8 bytes are consumed even in the bug case");
    }

    #[test]
    fn file_size_keeps_genuine_large_values_intact() {
        // When the top byte is not 255 the value is a real uint64 and must be
        // read at full width — a legitimate >2 GiB file (3 GiB here) is not
        // truncated. Matches Nicotine+'s `else: unpack_uint64()` branch.
        let mut buf = Vec::new();
        put_u64(&mut buf, 3 * 1024 * 1024 * 1024);
        assert_eq!(buf[7], 0x00, "top byte is not the bug sentinel");
        let mut r = Reader::new(&buf);
        assert_eq!(r.file_size().unwrap(), 3 * 1024 * 1024 * 1024);
        assert!(r.is_empty());
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
