//! Control messages sent on the reliable QUIC stream.
//!
//! Framing: a `u32` big-endian body length, then the body. The body starts with a
//! one-byte message type followed by the message fields. Body length is limited to
//! [`MAX_CONTROL_FRAME`], so a peer cannot make the receiver buffer unbounded data.

use crate::codec::{Reader, Writer};
use crate::error::{invalid, ProtocolError, Result};

/// Version negotiated in [`Hello`]; a peer with another version is rejected.
pub const CONTROL_VERSION: u16 = 1;

/// Largest control frame body in bytes.
pub const MAX_CONTROL_FRAME: usize = 16 * 1024;

/// Length of the frame prefix.
pub const FRAME_PREFIX_LEN: usize = 4;

/// Largest group token in bytes.
pub const MAX_TOKEN_LEN: usize = 255;

/// Largest peer display name in bytes (UTF-8).
pub const MAX_PEER_NAME_LEN: usize = 64;

mod msg_type {
    pub const HELLO: u8 = 0x01;
    pub const HELLO_ACK: u8 = 0x02;
    pub const STREAM_CONFIG: u8 = 0x10;
    pub const PRESET_REQUEST: u8 = 0x11;
    pub const KEYFRAME_REQUEST: u8 = 0x12;
    pub const TELEMETRY_REPORT: u8 = 0x20;
}

/// Resolution preset. The effective size follows the source aspect ratio and
/// is announced in [`StreamConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    P720,
    P1080,
    P1440,
    /// Capture size, downscaled to fit 2560x1440.
    Native,
}

impl Preset {
    fn to_wire(self) -> u8 {
        match self {
            Preset::P720 => 1,
            Preset::P1080 => 2,
            Preset::P1440 => 3,
            Preset::Native => 4,
        }
    }

    fn from_wire(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Preset::P720,
            2 => Preset::P1080,
            3 => Preset::P1440,
            4 => Preset::Native,
            _ => return Err(invalid("preset", "unknown value")),
        })
    }
}

/// Target frame rate. Encoded as the number of frames per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameRate {
    Fps30,
    Fps60,
}

impl FrameRate {
    pub fn fps(self) -> u8 {
        match self {
            FrameRate::Fps30 => 30,
            FrameRate::Fps60 => 60,
        }
    }

    fn from_wire(v: u8) -> Result<Self> {
        match v {
            30 => Ok(FrameRate::Fps30),
            60 => Ok(FrameRate::Fps60),
            _ => Err(invalid("frame_rate", "must be 30 or 60")),
        }
    }
}

/// Video codec of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
}

impl Codec {
    fn to_wire(self) -> u8 {
        match self {
            Codec::H264 => 1,
        }
    }

    fn from_wire(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Codec::H264),
            _ => Err(invalid("codec", "unknown value")),
        }
    }
}

/// First message of a connection, from the connecting peer. Carries the group token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub protocol_version: u16,
    /// Group token. May be empty on the wire; the receiver then rejects the peer.
    pub token: Vec<u8>,
    pub peer_name: String,
}

/// Accepting peer's answer after the token checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloAck {
    pub protocol_version: u16,
    pub session_id: u64,
    pub peer_name: String,
}

/// Sender announces the stream it is about to send. Sent again whenever the
/// effective size changes (new preset, resized window, adaptive downscale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    pub preset: Preset,
    pub frame_rate: FrameRate,
    /// Effective encoded width in pixels, after aspect-preserving scaling.
    pub width: u16,
    /// Effective encoded height in pixels.
    pub height: u16,
    pub codec: Codec,
}

/// A viewer asks the sender for another preset or frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetRequest {
    pub preset: Preset,
    pub frame_rate: FrameRate,
}

/// A viewer lost frames and needs a keyframe (PLI/IDR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeRequest {
    /// Last frame the viewer reassembled completely, if any.
    pub last_complete_frame_id: Option<u32>,
}

/// Receiver-side network measurements, sent back to the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryReport {
    pub rtt_us: u32,
    pub jitter_us: u32,
    /// Datagram loss in parts per million (0..=1_000_000).
    pub loss_ppm: u32,
    /// Estimated receive bandwidth in bits per second.
    pub bandwidth_bps: u64,
}

/// Every control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    Hello(Hello),
    HelloAck(HelloAck),
    StreamConfig(StreamConfig),
    PresetRequest(PresetRequest),
    KeyframeRequest(KeyframeRequest),
    TelemetryReport(TelemetryReport),
}

fn check_name(name: &str) -> Result<()> {
    if name.len() > MAX_PEER_NAME_LEN {
        return Err(invalid("peer_name", "too long"));
    }
    Ok(())
}

fn check_dimension(field: &'static str, v: u16) -> Result<()> {
    if v == 0 {
        return Err(invalid(field, "must be non-zero"));
    }
    Ok(())
}

impl ControlMessage {
    /// Encodes the message as a complete frame (length prefix and body).
    ///
    /// Fails only for values that the decoder would reject, such as an
    /// oversized token or name, a zero dimension or a loss above 100 %.
    pub fn encode_frame(&self) -> Result<Vec<u8>> {
        let mut w = Writer::with_capacity(64);
        w.u32(0); // length placeholder
        match self {
            ControlMessage::Hello(m) => {
                if m.token.len() > MAX_TOKEN_LEN {
                    return Err(invalid("token", "too long"));
                }
                check_name(&m.peer_name)?;
                w.u8(msg_type::HELLO);
                w.u16(m.protocol_version);
                w.short_bytes(&m.token);
                w.short_bytes(m.peer_name.as_bytes());
            }
            ControlMessage::HelloAck(m) => {
                check_name(&m.peer_name)?;
                w.u8(msg_type::HELLO_ACK);
                w.u16(m.protocol_version);
                w.u64(m.session_id);
                w.short_bytes(m.peer_name.as_bytes());
            }
            ControlMessage::StreamConfig(m) => {
                check_dimension("width", m.width)?;
                check_dimension("height", m.height)?;
                w.u8(msg_type::STREAM_CONFIG);
                w.u8(m.preset.to_wire());
                w.u8(m.frame_rate.fps());
                w.u16(m.width);
                w.u16(m.height);
                w.u8(m.codec.to_wire());
            }
            ControlMessage::PresetRequest(m) => {
                w.u8(msg_type::PRESET_REQUEST);
                w.u8(m.preset.to_wire());
                w.u8(m.frame_rate.fps());
            }
            ControlMessage::KeyframeRequest(m) => {
                w.u8(msg_type::KEYFRAME_REQUEST);
                match m.last_complete_frame_id {
                    Some(id) => {
                        w.u8(1);
                        w.u32(id);
                    }
                    None => w.u8(0),
                }
            }
            ControlMessage::TelemetryReport(m) => {
                if m.loss_ppm > 1_000_000 {
                    return Err(invalid("loss_ppm", "above 1_000_000"));
                }
                w.u8(msg_type::TELEMETRY_REPORT);
                w.u32(m.rtt_us);
                w.u32(m.jitter_us);
                w.u32(m.loss_ppm);
                w.u64(m.bandwidth_bps);
            }
        }
        let body_len = w.buf.len() - FRAME_PREFIX_LEN;
        // Every message above is far below the limit; keep the check as a guard.
        if body_len > MAX_CONTROL_FRAME {
            return Err(ProtocolError::BadFrameLength {
                len: body_len,
                max: MAX_CONTROL_FRAME,
            });
        }
        w.buf[..FRAME_PREFIX_LEN].copy_from_slice(&(body_len as u32).to_be_bytes());
        Ok(w.buf)
    }

    /// Validates a frame length prefix before the body is read.
    pub fn body_len(prefix: [u8; FRAME_PREFIX_LEN]) -> Result<usize> {
        let len = u32::from_be_bytes(prefix) as usize;
        if len == 0 || len > MAX_CONTROL_FRAME {
            return Err(ProtocolError::BadFrameLength {
                len,
                max: MAX_CONTROL_FRAME,
            });
        }
        Ok(len)
    }

    /// Decodes a frame body (message type and fields, without the length prefix).
    pub fn decode_body(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let msg = match r.u8()? {
            msg_type::HELLO => {
                let protocol_version = r.u16()?;
                let token = r.short_bytes()?.to_vec();
                let peer_name = r.short_str("peer_name", MAX_PEER_NAME_LEN)?;
                ControlMessage::Hello(Hello {
                    protocol_version,
                    token,
                    peer_name,
                })
            }
            msg_type::HELLO_ACK => ControlMessage::HelloAck(HelloAck {
                protocol_version: r.u16()?,
                session_id: r.u64()?,
                peer_name: r.short_str("peer_name", MAX_PEER_NAME_LEN)?,
            }),
            msg_type::STREAM_CONFIG => {
                let m = StreamConfig {
                    preset: Preset::from_wire(r.u8()?)?,
                    frame_rate: FrameRate::from_wire(r.u8()?)?,
                    width: r.u16()?,
                    height: r.u16()?,
                    codec: Codec::from_wire(r.u8()?)?,
                };
                check_dimension("width", m.width)?;
                check_dimension("height", m.height)?;
                ControlMessage::StreamConfig(m)
            }
            msg_type::PRESET_REQUEST => ControlMessage::PresetRequest(PresetRequest {
                preset: Preset::from_wire(r.u8()?)?,
                frame_rate: FrameRate::from_wire(r.u8()?)?,
            }),
            msg_type::KEYFRAME_REQUEST => {
                let last_complete_frame_id = match r.u8()? {
                    0 => None,
                    1 => Some(r.u32()?),
                    _ => return Err(invalid("last_complete_frame_id", "bad presence flag")),
                };
                ControlMessage::KeyframeRequest(KeyframeRequest {
                    last_complete_frame_id,
                })
            }
            msg_type::TELEMETRY_REPORT => {
                let m = TelemetryReport {
                    rtt_us: r.u32()?,
                    jitter_us: r.u32()?,
                    loss_ppm: r.u32()?,
                    bandwidth_bps: r.u64()?,
                };
                if m.loss_ppm > 1_000_000 {
                    return Err(invalid("loss_ppm", "above 1_000_000"));
                }
                ControlMessage::TelemetryReport(m)
            }
            other => return Err(ProtocolError::UnknownMessageType(other)),
        };
        r.finish()?;
        Ok(msg)
    }

    /// Decodes the first frame in `buf`, if it is complete.
    ///
    /// Returns `Ok(None)` when more bytes are needed and `Ok(Some((msg, used)))`
    /// with the number of bytes consumed otherwise.
    pub fn decode_frame(buf: &[u8]) -> Result<Option<(Self, usize)>> {
        let Some(prefix) = buf.get(..FRAME_PREFIX_LEN) else {
            return Ok(None);
        };
        let mut p = [0u8; FRAME_PREFIX_LEN];
        p.copy_from_slice(prefix);
        let len = Self::body_len(p)?;
        let Some(body) = buf.get(FRAME_PREFIX_LEN..FRAME_PREFIX_LEN + len) else {
            return Ok(None);
        };
        Ok(Some((Self::decode_body(body)?, FRAME_PREFIX_LEN + len)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<ControlMessage> {
        vec![
            ControlMessage::Hello(Hello {
                protocol_version: CONTROL_VERSION,
                token: b"group-secret".to_vec(),
                peer_name: "fedora-desktop".into(),
            }),
            ControlMessage::Hello(Hello {
                protocol_version: CONTROL_VERSION,
                token: Vec::new(),
                peer_name: String::new(),
            }),
            ControlMessage::HelloAck(HelloAck {
                protocol_version: CONTROL_VERSION,
                session_id: u64::MAX,
                peer_name: "Ünïcode 名前".into(),
            }),
            ControlMessage::StreamConfig(StreamConfig {
                preset: Preset::P1440,
                frame_rate: FrameRate::Fps60,
                width: 2304,
                height: 1440,
                codec: Codec::H264,
            }),
            ControlMessage::PresetRequest(PresetRequest {
                preset: Preset::Native,
                frame_rate: FrameRate::Fps30,
            }),
            ControlMessage::KeyframeRequest(KeyframeRequest {
                last_complete_frame_id: Some(41),
            }),
            ControlMessage::KeyframeRequest(KeyframeRequest {
                last_complete_frame_id: None,
            }),
            ControlMessage::TelemetryReport(TelemetryReport {
                rtt_us: 12_500,
                jitter_us: 800,
                loss_ppm: 1_000_000,
                bandwidth_bps: 25_000_000,
            }),
        ]
    }

    #[test]
    fn every_message_roundtrips() {
        for msg in samples() {
            let frame = msg.encode_frame().unwrap();
            let (decoded, used) = ControlMessage::decode_frame(&frame).unwrap().unwrap();
            assert_eq!(decoded, msg);
            assert_eq!(used, frame.len());
        }
    }

    #[test]
    fn partial_frames_ask_for_more_bytes() {
        let frame = samples()[0].encode_frame().unwrap();
        for len in 0..frame.len() {
            assert_eq!(
                ControlMessage::decode_frame(&frame[..len]),
                Ok(None),
                "len {len}"
            );
        }
    }

    #[test]
    fn back_to_back_frames_decode_in_order() {
        let mut buf = Vec::new();
        for msg in samples() {
            buf.extend(msg.encode_frame().unwrap());
        }
        let mut decoded = Vec::new();
        let mut rest = &buf[..];
        while let Some((msg, used)) = ControlMessage::decode_frame(rest).unwrap() {
            decoded.push(msg);
            rest = &rest[used..];
        }
        assert!(rest.is_empty());
        assert_eq!(decoded, samples());
    }

    #[test]
    fn truncated_bodies_are_errors() {
        for msg in samples() {
            let frame = msg.encode_frame().unwrap();
            let body = &frame[FRAME_PREFIX_LEN..];
            for len in 0..body.len() {
                assert!(ControlMessage::decode_body(&body[..len]).is_err());
            }
        }
    }

    #[test]
    fn frame_length_limits() {
        assert!(ControlMessage::body_len(0u32.to_be_bytes()).is_err());
        assert!(ControlMessage::body_len((MAX_CONTROL_FRAME as u32 + 1).to_be_bytes()).is_err());
        assert_eq!(
            ControlMessage::body_len((MAX_CONTROL_FRAME as u32).to_be_bytes()),
            Ok(MAX_CONTROL_FRAME)
        );
        // A hostile prefix is rejected before any body is buffered.
        assert!(ControlMessage::decode_frame(&u32::MAX.to_be_bytes()).is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        let bodies: [&[u8]; 7] = [
            &[0x7f],                                          // unknown type
            &[msg_type::STREAM_CONFIG, 9, 60, 0, 1, 0, 1, 1], // bad preset
            &[msg_type::STREAM_CONFIG, 1, 25, 0, 1, 0, 1, 1], // bad fps
            &[msg_type::STREAM_CONFIG, 1, 60, 0, 0, 0, 1, 1], // zero width
            &[msg_type::STREAM_CONFIG, 1, 60, 0, 1, 0, 1, 7], // bad codec
            &[msg_type::KEYFRAME_REQUEST, 2],                 // bad option flag
            &[msg_type::PRESET_REQUEST, 1, 60, 0],            // trailing byte
        ];
        for body in bodies {
            assert!(ControlMessage::decode_body(body).is_err(), "{body:?}");
        }
        let mut loss = vec![msg_type::TELEMETRY_REPORT];
        loss.extend([0; 8]);
        loss.extend(1_000_001u32.to_be_bytes());
        loss.extend([0; 8]);
        assert!(ControlMessage::decode_body(&loss).is_err());
    }

    #[test]
    fn encoder_refuses_values_the_decoder_rejects() {
        let long_token = ControlMessage::Hello(Hello {
            protocol_version: 1,
            token: vec![0; MAX_TOKEN_LEN + 1],
            peer_name: String::new(),
        });
        assert!(long_token.encode_frame().is_err());
        let long_name = ControlMessage::HelloAck(HelloAck {
            protocol_version: 1,
            session_id: 1,
            peer_name: "x".repeat(MAX_PEER_NAME_LEN + 1),
        });
        assert!(long_name.encode_frame().is_err());
        let zero = ControlMessage::StreamConfig(StreamConfig {
            preset: Preset::P720,
            frame_rate: FrameRate::Fps30,
            width: 0,
            height: 720,
            codec: Codec::H264,
        });
        assert!(zero.encode_frame().is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        // Small deterministic generator; the decoder must return, not panic.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 48) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = ControlMessage::decode_body(&bytes);
            let _ = ControlMessage::decode_frame(&bytes);
            let _ = crate::media::decode_datagram(&bytes);
        }
    }
}
