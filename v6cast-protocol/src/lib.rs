//! v6cast-protocol: the v6cast wire format.
//!
//! * [`media`]: header of the QUIC datagrams that carry media fragments.
//! * [`control`]: messages on the reliable control stream (hello/auth, preset and
//!   frame-rate negotiation with the effective width/height, keyframe requests,
//!   telemetry reports).
//! * [`fragment`]: splitting frames into IPv6-safe datagrams and bounded reassembly.
//!
//! The crate has no I/O and no async code. Every decoder works on untrusted bytes
//! and returns [`ProtocolError`] instead of panicking.

#![forbid(unsafe_code)]

mod codec;
pub mod control;
pub mod error;
pub mod fragment;
pub mod media;

pub use control::{
    Codec, ControlMessage, FrameRate, Hello, HelloAck, KeyframeRequest, Preset, PresetRequest,
    StreamConfig, TelemetryReport, CONTROL_VERSION,
};
pub use error::{ProtocolError, Result};
pub use fragment::{
    DropReason, Fragmenter, Frame, Push, Reassembler, ReassemblyConfig, ReassemblyStats,
    IPV6_SAFE_DATAGRAM,
};
pub use media::{decode_datagram, MediaFlags, MediaHeader, MediaKind, MEDIA_HEADER_LEN};
