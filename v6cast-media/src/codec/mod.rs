//! H.264 codec abstraction with automatic backend selection.
//!
//! [`open_encoder`] and [`open_decoder`] pick the best available backend for a
//! [`BackendPreference`]: hardware first (VA-API on Linux, when compiled in and
//! when the device reports the needed H.264 profile), then the software
//! backend (openh264). The returned [`FallbackEncoder`] and [`FallbackDecoder`]
//! also switch to software if a hardware backend fails while running.
//!
//! Low-latency settings on every backend: Constrained Baseline or Main, no
//! B-frames, one reference frame, IDR every [`EncoderConfig::keyframe_interval`]
//! (2 s by default) and on demand via [`VideoEncoder::request_keyframe`].
//! The bitstream is Annex B, one access unit per frame.

use std::time::Duration;

use thiserror::Error;

use crate::frame::{FrameError, YuvFrame};
use crate::scale::{FrameRate, Preset, Resolution, VideoProfile};

pub mod h264;
pub mod software;

/// Which implementation runs the codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// openh264, on the CPU.
    Software,
    /// VA-API (AMD/Intel on Linux).
    Vaapi,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Software => "software (openh264)",
            Backend::Vaapi => "VA-API",
        })
    }
}

/// What the caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendPreference {
    /// Hardware when it works, software otherwise.
    #[default]
    Auto,
    SoftwareOnly,
    /// Fail instead of using software.
    HardwareOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodecError {
    #[error("invalid encoder configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("frame is {got}, the encoder was configured for {expected}")]
    ResolutionMismatch {
        expected: Resolution,
        got: Resolution,
    },
    #[error("no {0} backend is available")]
    Unavailable(&'static str),
    #[error("{backend} backend failed: {message}")]
    Backend { backend: Backend, message: String },
    #[error("bad frame: {0}")]
    Frame(#[from] FrameError),
}

impl CodecError {
    pub(crate) fn backend(backend: Backend, err: impl std::fmt::Display) -> Self {
        CodecError::Backend {
            backend,
            message: err.to_string(),
        }
    }
}

/// H.264 profile. Both have no B-frames, so frames decode in send order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264Profile {
    #[default]
    ConstrainedBaseline,
    /// Main without B-frames. Backends that cannot do Main use Constrained Baseline.
    Main,
}

/// Bitrate bounds for a preset, in bits per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateRange {
    pub min_bps: u32,
    pub default_bps: u32,
    pub max_bps: u32,
}

impl BitrateRange {
    pub fn clamp(&self, bps: u32) -> u32 {
        bps.clamp(self.min_bps, self.max_bps)
    }
}

/// Bitrate bounds for screen content at `preset` and `rate`. The adaptive
/// controller (Etapa 3) moves inside this range.
pub fn bitrate_range(preset: Preset, rate: FrameRate) -> BitrateRange {
    const M: u32 = 1_000_000;
    let (min, default, max) = match (preset, rate) {
        (Preset::P720, FrameRate::Fps30) => (1, 3, 5),
        (Preset::P720, FrameRate::Fps60) => (2, 4, 8),
        (Preset::P1080, FrameRate::Fps30) => (2, 5, 10),
        (Preset::P1080, FrameRate::Fps60) => (3, 8, 15),
        (Preset::P1440 | Preset::Native, FrameRate::Fps30) => (4, 10, 18),
        (Preset::P1440 | Preset::Native, FrameRate::Fps60) => (6, 14, 25),
    };
    BitrateRange {
        min_bps: min * M,
        default_bps: default * M,
        max_bps: max * M,
    }
}

/// Encoder settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Encoded size, already scaled and aligned.
    pub resolution: Resolution,
    pub frame_rate: FrameRate,
    pub bitrate_bps: u32,
    pub keyframe_interval: Duration,
    pub profile: H264Profile,
}

impl EncoderConfig {
    /// Settings for `profile` at `resolution`, with the preset's default bitrate.
    pub fn new(resolution: Resolution, profile: VideoProfile) -> Self {
        Self {
            resolution,
            frame_rate: profile.frame_rate,
            bitrate_bps: bitrate_range(profile.preset, profile.frame_rate).default_bps,
            keyframe_interval: Duration::from_secs(2),
            profile: H264Profile::ConstrainedBaseline,
        }
    }

    /// Frames between two periodic IDRs (at least 1).
    pub fn keyframe_interval_frames(&self) -> u32 {
        let frames = self.keyframe_interval.as_millis() * u128::from(self.frame_rate.fps()) / 1000;
        u32::try_from(frames).unwrap_or(u32::MAX).max(1)
    }

    pub fn validate(&self) -> Result<(), CodecError> {
        let r = self.resolution;
        if r.width == 0
            || r.height == 0
            || !r.width.is_multiple_of(2)
            || !r.height.is_multiple_of(2)
        {
            return Err(CodecError::InvalidConfig(
                "resolution must be non-zero and even",
            ));
        }
        if r.width > 4096 || r.height > 4096 {
            return Err(CodecError::InvalidConfig("resolution above 4096 pixels"));
        }
        if self.bitrate_bps == 0 {
            return Err(CodecError::InvalidConfig("bitrate is zero"));
        }
        if self.keyframe_interval.is_zero() {
            return Err(CodecError::InvalidConfig("keyframe interval is zero"));
        }
        Ok(())
    }
}

/// One encoded access unit (Annex B, SPS/PPS in front of every IDR).
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Capture timestamp passed to [`VideoEncoder::encode`].
    pub timestamp_us: u64,
}

impl std::fmt::Debug for EncodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedFrame")
            .field("len", &self.data.len())
            .field("keyframe", &self.keyframe)
            .field("timestamp_us", &self.timestamp_us)
            .finish()
    }
}

pub trait VideoEncoder: Send {
    fn backend(&self) -> Backend;
    fn config(&self) -> &EncoderConfig;
    /// Encodes one frame. `Ok(None)` when the backend produced no output for it.
    fn encode(
        &mut self,
        frame: &YuvFrame,
        timestamp_us: u64,
    ) -> Result<Option<EncodedFrame>, CodecError>;
    /// The next encoded frame will be an IDR.
    fn request_keyframe(&mut self);
    /// Changes the target bitrate. The next frame may be an IDR.
    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError>;
}

pub trait VideoDecoder: Send {
    fn backend(&self) -> Backend;
    /// Decodes one access unit. `Ok(None)` when no picture is ready yet
    /// (for example before the first IDR).
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<YuvFrame>, CodecError>;
}

/// What a backend can do on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCaps {
    pub backend: Backend,
    pub encode: bool,
    pub decode: bool,
    /// Largest encode size, when the backend reports one.
    pub max_resolution: Option<Resolution>,
}

impl BackendCaps {
    fn can_encode(&self, r: Resolution) -> bool {
        self.encode
            && self
                .max_resolution
                .is_none_or(|m| r.width <= m.width && r.height <= m.height)
    }
}

/// Backends detected on this machine, hardware first.
pub fn probe() -> Vec<BackendCaps> {
    let mut caps = hardware_caps();
    caps.push(software::caps());
    caps
}

/// Hardware backends compiled into this build and present on this machine.
#[cfg(all(target_os = "linux", feature = "vaapi"))]
fn hardware_caps() -> Vec<BackendCaps> {
    crate::vaapi::probe().into_iter().collect()
}

/// Hardware backends compiled into this build: none without the `vaapi` feature.
#[cfg(not(all(target_os = "linux", feature = "vaapi")))]
fn hardware_caps() -> Vec<BackendCaps> {
    Vec::new()
}

fn open_hardware_encoder(
    backend: Backend,
    config: &EncoderConfig,
) -> Result<Box<dyn VideoEncoder>, CodecError> {
    match backend {
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        Backend::Vaapi => Ok(Box::new(crate::vaapi::VaapiEncoder::new(config)?)),
        _ => {
            let _ = config;
            Err(CodecError::Unavailable("hardware encoder"))
        }
    }
}

fn open_hardware_decoder(backend: Backend) -> Result<Box<dyn VideoDecoder>, CodecError> {
    match backend {
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        Backend::Vaapi => Ok(Box::new(crate::vaapi::VaapiDecoder::new()?)),
        _ => Err(CodecError::Unavailable("hardware decoder")),
    }
}

fn software_encoder(config: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, CodecError> {
    Ok(Box::new(software::SoftwareEncoder::new(config)?))
}

fn software_decoder() -> Result<Box<dyn VideoDecoder>, CodecError> {
    Ok(Box::new(software::SoftwareDecoder::new()?))
}

/// Opens an encoder for `config`. See the module docs for the selection order.
pub fn open_encoder(
    config: &EncoderConfig,
    preference: BackendPreference,
) -> Result<FallbackEncoder, CodecError> {
    config.validate()?;
    if preference != BackendPreference::SoftwareOnly {
        for caps in hardware_caps() {
            if !caps.can_encode(config.resolution) {
                continue;
            }
            match open_hardware_encoder(caps.backend, config) {
                Ok(enc) => {
                    let allow = preference == BackendPreference::Auto;
                    return Ok(FallbackEncoder::new(enc, allow));
                }
                Err(e) => tracing::warn!("{} encoder unavailable: {e}", caps.backend),
            }
        }
        if preference == BackendPreference::HardwareOnly {
            return Err(CodecError::Unavailable("hardware encoder"));
        }
    }
    Ok(FallbackEncoder::new(software_encoder(config)?, false))
}

/// Opens a decoder. See the module docs for the selection order.
pub fn open_decoder(preference: BackendPreference) -> Result<FallbackDecoder, CodecError> {
    if preference != BackendPreference::SoftwareOnly {
        for caps in hardware_caps().into_iter().filter(|c| c.decode) {
            match open_hardware_decoder(caps.backend) {
                Ok(dec) => {
                    let allow = preference == BackendPreference::Auto;
                    return Ok(FallbackDecoder::new(dec, allow));
                }
                Err(e) => tracing::warn!("{} decoder unavailable: {e}", caps.backend),
            }
        }
        if preference == BackendPreference::HardwareOnly {
            return Err(CodecError::Unavailable("hardware decoder"));
        }
    }
    Ok(FallbackDecoder::new(software_decoder()?, false))
}

/// An encoder that replaces a failing hardware backend with openh264.
///
/// The failing frame is re-encoded by the software encoder, which starts with
/// an IDR, so the stream keeps going. Only one switch happens; software errors
/// are returned as they are.
pub struct FallbackEncoder {
    inner: Box<dyn VideoEncoder>,
    can_fall_back: bool,
    fell_back: bool,
}

impl FallbackEncoder {
    pub fn new(inner: Box<dyn VideoEncoder>, can_fall_back: bool) -> Self {
        Self {
            inner,
            can_fall_back,
            fell_back: false,
        }
    }

    /// True once a hardware failure made the encoder switch to software.
    pub fn fell_back(&self) -> bool {
        self.fell_back
    }
}

impl VideoEncoder for FallbackEncoder {
    fn backend(&self) -> Backend {
        self.inner.backend()
    }

    fn config(&self) -> &EncoderConfig {
        self.inner.config()
    }

    fn encode(
        &mut self,
        frame: &YuvFrame,
        timestamp_us: u64,
    ) -> Result<Option<EncodedFrame>, CodecError> {
        match self.inner.encode(frame, timestamp_us) {
            Err(CodecError::Backend { backend, message })
                if self.can_fall_back && backend != Backend::Software =>
            {
                tracing::warn!("{backend} encoder failed ({message}); switching to software");
                let config = self.inner.config().clone();
                self.inner = software_encoder(&config)?;
                self.can_fall_back = false;
                self.fell_back = true;
                self.inner.encode(frame, timestamp_us)
            }
            other => other,
        }
    }

    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        self.inner.set_bitrate(bps)
    }
}

/// A decoder that replaces a failing hardware backend with openh264.
///
/// After the switch, pictures resume at the next IDR; the caller should send a
/// keyframe request, which [`FallbackDecoder::take_keyframe_request`] reports.
pub struct FallbackDecoder {
    inner: Box<dyn VideoDecoder>,
    can_fall_back: bool,
    fell_back: bool,
    wants_keyframe: bool,
}

impl FallbackDecoder {
    pub fn new(inner: Box<dyn VideoDecoder>, can_fall_back: bool) -> Self {
        Self {
            inner,
            can_fall_back,
            fell_back: false,
            wants_keyframe: false,
        }
    }

    pub fn fell_back(&self) -> bool {
        self.fell_back
    }

    /// True once after a switch: the sender should be asked for an IDR.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.wants_keyframe)
    }
}

impl VideoDecoder for FallbackDecoder {
    fn backend(&self) -> Backend {
        self.inner.backend()
    }

    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        match self.inner.decode(access_unit) {
            Err(CodecError::Backend { backend, message })
                if self.can_fall_back && backend != Backend::Software =>
            {
                tracing::warn!("{backend} decoder failed ({message}); switching to software");
                self.inner = software_decoder()?;
                self.can_fall_back = false;
                self.fell_back = true;
                self.wants_keyframe = true;
                // Works if this access unit is an IDR; otherwise waits for one.
                self.inner.decode(access_unit).or(Ok(None))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(w: u32, h: u32) -> EncoderConfig {
        EncoderConfig::new(
            Resolution::new(w, h),
            VideoProfile::new(Preset::P1080, FrameRate::Fps60),
        )
    }

    #[test]
    fn bitrate_ranges_grow_with_size_and_rate() {
        let presets = [Preset::P720, Preset::P1080, Preset::P1440];
        for rate in [FrameRate::Fps30, FrameRate::Fps60] {
            let ranges: Vec<_> = presets.iter().map(|p| bitrate_range(*p, rate)).collect();
            for r in &ranges {
                assert!(r.min_bps < r.default_bps && r.default_bps < r.max_bps);
            }
            assert!(ranges
                .windows(2)
                .all(|w| w[0].default_bps < w[1].default_bps));
        }
        for p in presets {
            let (a, b) = (
                bitrate_range(p, FrameRate::Fps30),
                bitrate_range(p, FrameRate::Fps60),
            );
            assert!(a.default_bps < b.default_bps);
        }
        assert_eq!(
            bitrate_range(Preset::Native, FrameRate::Fps60),
            bitrate_range(Preset::P1440, FrameRate::Fps60)
        );
        let r = bitrate_range(Preset::P720, FrameRate::Fps30);
        assert_eq!(r.clamp(0), r.min_bps);
        assert_eq!(r.clamp(u32::MAX), r.max_bps);
    }

    #[test]
    fn keyframe_interval_in_frames() {
        let c = config(1920, 1080);
        assert_eq!(c.keyframe_interval_frames(), 120);
        let c = EncoderConfig {
            keyframe_interval: Duration::from_millis(1),
            frame_rate: FrameRate::Fps30,
            ..c
        };
        assert_eq!(c.keyframe_interval_frames(), 1);
    }

    #[test]
    fn config_validation() {
        assert!(config(1920, 1080).validate().is_ok());
        assert!(config(0, 1080).validate().is_err());
        assert!(config(1921, 1080).validate().is_err());
        assert!(config(8192, 1080).validate().is_err());
        let c = EncoderConfig {
            bitrate_bps: 0,
            ..config(64, 64)
        };
        assert!(c.validate().is_err());
    }

    /// A "hardware" encoder that fails after `ok_frames` frames.
    struct FlakyEncoder {
        config: EncoderConfig,
        ok_frames: u32,
    }

    impl VideoEncoder for FlakyEncoder {
        fn backend(&self) -> Backend {
            Backend::Vaapi
        }
        fn config(&self) -> &EncoderConfig {
            &self.config
        }
        fn encode(
            &mut self,
            _frame: &YuvFrame,
            timestamp_us: u64,
        ) -> Result<Option<EncodedFrame>, CodecError> {
            if self.ok_frames == 0 {
                return Err(CodecError::backend(Backend::Vaapi, "GPU hang"));
            }
            self.ok_frames -= 1;
            Ok(Some(EncodedFrame {
                data: vec![0, 0, 0, 1, 0x65],
                keyframe: true,
                timestamp_us,
            }))
        }
        fn request_keyframe(&mut self) {}
        fn set_bitrate(&mut self, _bps: u32) -> Result<(), CodecError> {
            Ok(())
        }
    }

    #[test]
    fn failing_hardware_encoder_falls_back_to_software() {
        let cfg = config(64, 64);
        let flaky = FlakyEncoder {
            config: cfg.clone(),
            ok_frames: 1,
        };
        let mut enc = FallbackEncoder::new(Box::new(flaky), true);
        let frame = YuvFrame::black(cfg.resolution).unwrap();
        assert_eq!(enc.backend(), Backend::Vaapi);
        enc.encode(&frame, 0).unwrap().unwrap();
        // The second frame fails on "hardware" and is re-encoded in software.
        let out = enc.encode(&frame, 1).unwrap().unwrap();
        assert!(enc.fell_back());
        assert_eq!(enc.backend(), Backend::Software);
        assert!(out.keyframe, "software restarts with an IDR");
        assert_eq!(out.timestamp_us, 1);
        let mut dec = open_decoder(BackendPreference::SoftwareOnly).unwrap();
        assert!(dec.decode(&out.data).unwrap().is_some());
    }

    #[test]
    fn hardware_errors_surface_when_fallback_is_off() {
        let cfg = config(64, 64);
        let flaky = FlakyEncoder {
            config: cfg.clone(),
            ok_frames: 0,
        };
        let mut enc = FallbackEncoder::new(Box::new(flaky), false);
        let frame = YuvFrame::black(cfg.resolution).unwrap();
        assert!(matches!(
            enc.encode(&frame, 0),
            Err(CodecError::Backend {
                backend: Backend::Vaapi,
                ..
            })
        ));
    }

    struct BrokenDecoder;

    impl VideoDecoder for BrokenDecoder {
        fn backend(&self) -> Backend {
            Backend::Vaapi
        }
        fn decode(&mut self, _au: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
            Err(CodecError::backend(Backend::Vaapi, "context lost"))
        }
    }

    #[test]
    fn failing_hardware_decoder_falls_back_and_asks_for_a_keyframe() {
        let cfg = config(64, 64);
        let mut enc = open_encoder(&cfg, BackendPreference::SoftwareOnly).unwrap();
        let idr = enc
            .encode(&YuvFrame::black(cfg.resolution).unwrap(), 0)
            .unwrap()
            .unwrap();
        let mut dec = FallbackDecoder::new(Box::new(BrokenDecoder), true);
        let picture = dec.decode(&idr.data).unwrap();
        assert!(picture.is_some());
        assert_eq!(dec.backend(), Backend::Software);
        assert!(dec.take_keyframe_request());
        assert!(!dec.take_keyframe_request());
    }

    #[test]
    fn probe_always_lists_software_last() {
        let caps = probe();
        let last = caps.last().unwrap();
        assert_eq!(last.backend, Backend::Software);
        assert!(last.encode && last.decode);
    }

    #[test]
    fn auto_selection_always_returns_a_working_encoder() {
        let cfg = config(320, 180);
        let mut enc = open_encoder(&cfg, BackendPreference::Auto).unwrap();
        let out = enc
            .encode(&YuvFrame::black(cfg.resolution).unwrap(), 7)
            .unwrap()
            .unwrap();
        assert!(out.keyframe);
        assert!(open_encoder(&config(0, 0), BackendPreference::Auto).is_err());
    }
}
