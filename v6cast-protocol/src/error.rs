//! Errors returned when decoding untrusted bytes.

use thiserror::Error;

/// Every decoding failure. Decoders never panic on malformed input; they return one of these.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtocolError {
    #[error("input truncated: need {needed} more bytes, {available} available")]
    Truncated { needed: usize, available: usize },

    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),

    #[error("unknown media kind {0}")]
    UnknownMediaKind(u8),

    #[error("unknown control message type 0x{0:02x}")]
    UnknownMessageType(u8),

    #[error("invalid {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },

    #[error("control frame of {len} bytes is outside the allowed range 1..={max}")]
    BadFrameLength { len: usize, max: usize },

    #[error("{0} unexpected trailing bytes")]
    TrailingBytes(usize),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

pub(crate) fn invalid(field: &'static str, reason: &'static str) -> ProtocolError {
    ProtocolError::InvalidField { field, reason }
}
