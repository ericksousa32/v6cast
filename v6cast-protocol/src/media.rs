//! Media datagram header.
//!
//! Every QUIC datagram that carries media starts with a fixed 28-byte header,
//! followed by one fragment of an encoded frame:
//!
//! ```text
//!  0      1      2      3      4                8                12
//!  +------+------+------+------+----------------+----------------+
//!  | ver  | kind | flags| rsvd |   seq (u32)    | frame_id (u32) |
//!  +------+------+------+------+----------------+----------------+
//!  12             14             16              20
//!  +--------------+--------------+---------------+
//!  | frag_index   | frag_count   | frame_len u32 |
//!  +--------------+--------------+---------------+
//!  20                              28
//!  +-------------------------------+---------------------
//!  | timestamp_us (u64, monotonic) | fragment payload ...
//!  +-------------------------------+---------------------
//! ```
//!
//! All integers are big-endian. `seq` counts datagrams per source stream and
//! lets receivers measure loss; `frame_id` groups fragments of one frame.
//! The reserved byte is written as zero and ignored on read.

use crate::codec::{Reader, Writer};
use crate::error::{invalid, ProtocolError, Result};

/// Version byte of the media datagram format.
pub const MEDIA_VERSION: u8 = 1;

/// Size of [`MediaHeader`] on the wire.
pub const MEDIA_HEADER_LEN: usize = 28;

/// Largest encoded frame accepted (8 MiB, well above a 1440p H.264 keyframe).
pub const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024;

/// What a media datagram carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Video,
    Audio,
}

impl MediaKind {
    /// Number of kinds, for per-kind state arrays.
    pub const COUNT: usize = 2;

    pub fn index(self) -> usize {
        match self {
            MediaKind::Video => 0,
            MediaKind::Audio => 1,
        }
    }

    fn to_wire(self) -> u8 {
        match self {
            MediaKind::Video => 1,
            MediaKind::Audio => 2,
        }
    }

    fn from_wire(v: u8) -> Result<Self> {
        match v {
            1 => Ok(MediaKind::Video),
            2 => Ok(MediaKind::Audio),
            other => Err(ProtocolError::UnknownMediaKind(other)),
        }
    }
}

/// Per-frame flags. Unknown bits are kept so newer senders stay readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaFlags(pub u8);

impl MediaFlags {
    /// The frame can be decoded without earlier frames (H.264 IDR).
    pub const KEYFRAME: u8 = 0x01;

    pub fn keyframe() -> Self {
        Self(Self::KEYFRAME)
    }

    pub fn is_keyframe(self) -> bool {
        self.0 & Self::KEYFRAME != 0
    }
}

/// Header in front of every media fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub kind: MediaKind,
    pub flags: MediaFlags,
    /// Datagram sequence number of the source stream (wraps).
    pub seq: u32,
    /// Frame this fragment belongs to (wraps).
    pub frame_id: u32,
    pub fragment_index: u16,
    pub fragment_count: u16,
    /// Total length of the reassembled frame in bytes.
    pub frame_len: u32,
    /// Capture time in microseconds on the sender's monotonic clock.
    pub timestamp_us: u64,
}

impl MediaHeader {
    /// Checks the invariants that every valid header satisfies.
    pub fn validate(&self) -> Result<()> {
        if self.fragment_count == 0 {
            return Err(invalid("fragment_count", "must be at least 1"));
        }
        if self.fragment_index >= self.fragment_count {
            return Err(invalid("fragment_index", "must be below fragment_count"));
        }
        if self.frame_len == 0 || self.frame_len > MAX_FRAME_LEN {
            return Err(invalid("frame_len", "must be within 1..=MAX_FRAME_LEN"));
        }
        if u32::from(self.fragment_count) > self.frame_len {
            return Err(invalid("fragment_count", "more fragments than frame bytes"));
        }
        Ok(())
    }

    /// Appends the header to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let mut w = Writer {
            buf: std::mem::take(out),
        };
        w.u8(MEDIA_VERSION);
        w.u8(self.kind.to_wire());
        w.u8(self.flags.0);
        w.u8(0);
        w.u32(self.seq);
        w.u32(self.frame_id);
        w.u16(self.fragment_index);
        w.u16(self.fragment_count);
        w.u32(self.frame_len);
        w.u64(self.timestamp_us);
        *out = w.buf;
    }

    /// Builds a complete datagram: header followed by `payload`.
    pub fn encode_datagram(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(MEDIA_HEADER_LEN + payload.len());
        self.encode_into(&mut out);
        out.extend_from_slice(payload);
        out
    }
}

/// Parses a media datagram into its header and fragment payload.
///
/// Rejects unknown versions or kinds, inconsistent fragment fields and empty payloads.
pub fn decode_datagram(datagram: &[u8]) -> Result<(MediaHeader, &[u8])> {
    let mut r = Reader::new(datagram);
    let version = r.u8()?;
    if version != MEDIA_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let kind = MediaKind::from_wire(r.u8()?)?;
    let flags = MediaFlags(r.u8()?);
    let _reserved = r.u8()?;
    let header = MediaHeader {
        kind,
        flags,
        seq: r.u32()?,
        frame_id: r.u32()?,
        fragment_index: r.u16()?,
        fragment_count: r.u16()?,
        frame_len: r.u32()?,
        timestamp_us: r.u64()?,
    };
    header.validate()?;
    let payload = r.remaining();
    if payload.is_empty() {
        return Err(invalid("payload", "empty fragment"));
    }
    if payload.len() > header.frame_len as usize {
        return Err(invalid("payload", "larger than the whole frame"));
    }
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> MediaHeader {
        MediaHeader {
            kind: MediaKind::Video,
            flags: MediaFlags::keyframe(),
            seq: 0xdead_beef,
            frame_id: 7,
            fragment_index: 1,
            fragment_count: 3,
            frame_len: 3000,
            timestamp_us: 1_234_567_890,
        }
    }

    #[test]
    fn roundtrip_keeps_every_field() {
        let h = header();
        let datagram = h.encode_datagram(b"payload");
        assert_eq!(datagram.len(), MEDIA_HEADER_LEN + 7);
        let (decoded, payload) = decode_datagram(&datagram).unwrap();
        assert_eq!(decoded, h);
        assert_eq!(payload, b"payload");
        assert!(decoded.flags.is_keyframe());
    }

    #[test]
    fn header_layout_is_fixed() {
        let mut out = Vec::new();
        header().encode_into(&mut out);
        assert_eq!(out.len(), MEDIA_HEADER_LEN);
        assert_eq!(&out[..4], &[MEDIA_VERSION, 1, MediaFlags::KEYFRAME, 0]);
        assert_eq!(&out[4..8], &0xdead_beef_u32.to_be_bytes());
    }

    #[test]
    fn every_truncation_is_an_error() {
        let datagram = header().encode_datagram(b"x");
        for len in 0..datagram.len() {
            assert!(decode_datagram(&datagram[..len]).is_err(), "len {len}");
        }
    }

    #[test]
    fn rejects_bad_version_and_kind() {
        let mut d = header().encode_datagram(b"x");
        d[0] = 9;
        assert_eq!(
            decode_datagram(&d),
            Err(ProtocolError::UnsupportedVersion(9))
        );
        let mut d = header().encode_datagram(b"x");
        d[1] = 0;
        assert_eq!(decode_datagram(&d), Err(ProtocolError::UnknownMediaKind(0)));
    }

    #[test]
    fn rejects_inconsistent_fragment_fields() {
        let cases = [
            MediaHeader {
                fragment_count: 0,
                fragment_index: 0,
                ..header()
            },
            MediaHeader {
                fragment_index: 3,
                ..header()
            },
            MediaHeader {
                frame_len: 0,
                ..header()
            },
            MediaHeader {
                frame_len: MAX_FRAME_LEN + 1,
                ..header()
            },
            MediaHeader {
                frame_len: 2,
                ..header()
            },
        ];
        for h in cases {
            assert!(decode_datagram(&h.encode_datagram(b"x")).is_err(), "{h:?}");
        }
    }

    #[test]
    fn rejects_empty_and_oversized_payload() {
        assert!(decode_datagram(&header().encode_datagram(b"")).is_err());
        let h = MediaHeader {
            frame_len: 4,
            fragment_count: 1,
            fragment_index: 0,
            ..header()
        };
        assert!(decode_datagram(&h.encode_datagram(b"12345")).is_err());
    }

    #[test]
    fn unknown_flag_bits_survive() {
        let h = MediaHeader {
            flags: MediaFlags(0x81),
            ..header()
        };
        let (decoded, _) = decode_datagram(&h.encode_datagram(b"x")).unwrap();
        assert_eq!(decoded.flags, MediaFlags(0x81));
    }
}
