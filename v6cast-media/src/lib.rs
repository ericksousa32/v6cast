//! v6cast-media: capture, scaling, H.264 coding and rendering.
//!
//! * [`scale`]: resolution presets (720p, 1080p, 1440p, Native, capped at 2.5K)
//!   and the proportional scaler.
//! * [`frame`]: I420 frames shared by scaler, codecs and renderer.
//! * [`codec`]: H.264 encoder/decoder traits, backend selection and the
//!   openh264 software fallback.

#![forbid(unsafe_code)]

pub mod codec;
pub mod frame;
pub mod scale;

pub use codec::{
    open_decoder, open_encoder, Backend, BackendPreference, CodecError, EncodedFrame,
    EncoderConfig, VideoDecoder, VideoEncoder,
};
pub use frame::YuvFrame;
pub use scale::{fit, fit_preset, Alignment, FrameRate, Preset, Resolution, VideoProfile};
