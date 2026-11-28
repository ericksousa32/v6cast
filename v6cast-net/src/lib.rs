//! v6cast-net: the network engine.
//!
//! * [`Server`] and [`Client`]: QUIC (quinn) over IPv6-only sockets. Media uses
//!   unreliable QUIC datagrams; the control channel is one reliable stream.
//! * Authentication: per-instance self-signed TLS 1.3 certificate and a
//!   pre-shared [`GroupToken`] checked in constant time on the first stream.
//!   Wrong or missing tokens close the connection at once.
//! * [`FanOut`]: one encoded stream copied to every viewer; a slow viewer
//!   skips frames instead of delaying the others.
//! * [`discovery`]: stable IPv6 global address of the active interface and a
//!   monitor that publishes the endpoint when the prefix or interface changes.

#![forbid(unsafe_code)]

pub mod discovery;
mod endpoint;
pub mod error;
pub mod fanout;
mod session;
pub mod tls;
mod token;

pub use discovery::{EndpointMonitor, PublicEndpoint};
pub use endpoint::{Client, ClientConfig, Server, ServerConfig, TransportSettings};
pub use error::{close_code, NetError, Result};
pub use fanout::{BroadcastReport, DatagramSink, FanOut, SubscriberId, SubscriberStats};
pub use session::Session;
pub use token::GroupToken;
