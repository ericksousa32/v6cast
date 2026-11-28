//! v6cast-net: the network engine.
//!
//! * [`Server`] and [`Client`]: QUIC (quinn) over IPv6-only sockets. Media uses
//!   unreliable QUIC datagrams; the control channel is one reliable stream.
//! * Authentication: per-instance self-signed TLS 1.3 certificate and a
//!   pre-shared [`GroupToken`] checked in constant time on the first stream.
//!   Wrong or missing tokens close the connection at once.

#![forbid(unsafe_code)]

mod endpoint;
pub mod error;
mod session;
pub mod tls;
mod token;

pub use endpoint::{Client, ClientConfig, Server, ServerConfig, TransportSettings};
pub use error::{close_code, NetError, Result};
pub use session::Session;
pub use token::GroupToken;
