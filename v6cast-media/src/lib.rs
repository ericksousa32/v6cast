//! v6cast-media: capture, scaling, H.264 coding and rendering.
//!
//! * [`scale`]: resolution presets (720p, 1080p, 1440p, Native, capped at 2.5K)
//!   and the proportional scaler.

#![forbid(unsafe_code)]

pub mod scale;

pub use scale::{fit, fit_preset, Alignment, FrameRate, Preset, Resolution, VideoProfile};
