//! Errors of the network engine.

use thiserror::Error;
use v6cast_protocol::ProtocolError;

/// QUIC application close codes used by v6cast.
pub mod close_code {
    /// Orderly close.
    pub const NORMAL: u32 = 0x00;
    /// Wrong or empty group token.
    pub const AUTH_FAILED: u32 = 0x01;
    /// No Hello before the authentication timeout.
    pub const AUTH_TIMEOUT: u32 = 0x02;
    /// Malformed or unexpected control traffic.
    pub const PROTOCOL_VIOLATION: u32 = 0x03;
    /// The peer speaks another control protocol version.
    pub const VERSION_MISMATCH: u32 = 0x04;
}

#[derive(Debug, Error)]
pub enum NetError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TLS setup failed: {0}")]
    Tls(#[from] rustls::Error),

    #[error("certificate generation failed: {0}")]
    Certificate(#[from] rcgen::Error),

    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),

    #[error("cannot start connection: {0}")]
    Connect(#[from] quinn::ConnectError),

    #[error("connection lost: {0}")]
    Connection(#[from] quinn::ConnectionError),

    #[error("cannot send datagram: {0}")]
    SendDatagram(#[from] quinn::SendDatagramError),

    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("the peer rejected the group token")]
    AuthRejected,

    #[error("the peer closed the connection with code {code}")]
    ClosedByPeer { code: u64 },

    #[error("authentication timed out")]
    AuthTimeout,

    #[error("unexpected control message: {0}")]
    UnexpectedMessage(&'static str),

    #[error("the control stream was closed")]
    ControlClosed,
}

pub type Result<T> = std::result::Result<T, NetError>;

impl NetError {
    /// Maps a connection error, turning v6cast application close codes into
    /// specific variants.
    pub(crate) fn from_connection(err: quinn::ConnectionError) -> Self {
        match &err {
            quinn::ConnectionError::ApplicationClosed(close) => {
                let code = close.error_code.into_inner();
                if code == u64::from(close_code::AUTH_FAILED) {
                    NetError::AuthRejected
                } else {
                    NetError::ClosedByPeer { code }
                }
            }
            _ => NetError::Connection(err),
        }
    }
}
