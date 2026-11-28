//! Minimal big-endian reader and writer used by every message in the crate.

use crate::error::{invalid, ProtocolError, Result};

/// Bounds-checked cursor over an untrusted byte slice.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(ProtocolError::Truncated {
                needed: n - self.buf.len(),
                available: self.buf.len(),
            });
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// A byte string prefixed by its `u8` length.
    pub(crate) fn short_bytes(&mut self) -> Result<&'a [u8]> {
        let len = usize::from(self.u8()?);
        self.take(len)
    }

    /// A UTF-8 string prefixed by its `u8` length, at most `max` bytes.
    pub(crate) fn short_str(&mut self, field: &'static str, max: usize) -> Result<String> {
        let raw = self.short_bytes()?;
        if raw.len() > max {
            return Err(invalid(field, "too long"));
        }
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| invalid(field, "not valid UTF-8"))
    }

    pub(crate) fn remaining(&self) -> &'a [u8] {
        self.buf
    }

    /// Fails when bytes are left over, so a message cannot smuggle extra data.
    pub(crate) fn finish(self) -> Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::TrailingBytes(self.buf.len()))
        }
    }
}

/// Append-only big-endian writer.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Writes a `u8` length prefix and the bytes. Callers validate the length first.
    pub(crate) fn short_bytes(&mut self, v: &[u8]) {
        debug_assert!(v.len() <= usize::from(u8::MAX));
        self.u8(v.len() as u8);
        self.buf.extend_from_slice(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_reads_report_missing_bytes() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(
            r.u32(),
            Err(ProtocolError::Truncated {
                needed: 1,
                available: 3
            })
        );
    }

    #[test]
    fn short_str_rejects_invalid_utf8_and_overlong() {
        assert!(Reader::new(&[2, 0xff, 0xfe]).short_str("name", 8).is_err());
        assert_eq!(
            Reader::new(&[3, b'a', b'b', b'c']).short_str("name", 2),
            Err(invalid("name", "too long"))
        );
    }

    #[test]
    fn finish_rejects_trailing_bytes() {
        let mut r = Reader::new(&[0, 1, 2]);
        r.u8().unwrap();
        assert_eq!(r.finish(), Err(ProtocolError::TrailingBytes(2)));
    }
}
