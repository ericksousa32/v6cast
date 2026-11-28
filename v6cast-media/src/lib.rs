//! v6cast-media: capture, scaling, H.264 coding and rendering.
//!
//! * [`scale`]: resolution presets (720p, 1080p, 1440p, Native, capped at 2.5K)
//!   and the proportional scaler.
//! * [`frame`]: I420 frames shared by scaler, codecs and renderer.
//! * [`codec`]: H.264 encoder/decoder traits, backend selection and the
//!   openh264 software fallback.
//! * `vaapi` (Linux, `vaapi` feature): VA-API hardware H.264 encode and decode.
//! * [`render`]: wgpu renderer with an aspect-ratio-preserving viewport.
//! * [`capture`]: monitor and window capture (Linux portal + PipeWire with the
//!   `pipewire` feature), delivering scaled I420 frames with monotonic timestamps.
//! * [`audio`]: conditional audio (system loopback for a monitor, the window's
//!   application only for a window, silence when that is not possible).

#![forbid(unsafe_code)]

pub mod audio;
pub mod capture;
pub mod codec;
pub mod frame;
pub mod render;
pub mod scale;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub mod vaapi;

pub use codec::{
    open_decoder, open_encoder, Backend, BackendPreference, CodecError, EncodedFrame,
    EncoderConfig, VideoDecoder, VideoEncoder,
};
pub use frame::YuvFrame;
pub use render::{fit_viewport, Renderer, Viewport};
pub use scale::{fit, fit_preset, Alignment, FrameRate, Preset, Resolution, VideoProfile};
