//! Software H.264 backend: openh264, built from the bundled source.
//!
//! openh264 only produces Constrained Baseline, so [`H264Profile::Main`] falls
//! back to it here. Settings: screen-content real-time mode, bitrate rate
//! control, no frame skipping (the frame rate has priority over quality),
//! no scene-change IDRs (keyframes are periodic or on request only), low
//! complexity for latency. The IDR period counts from the last IDR, so a
//! requested IDR also moves the next periodic one.
//!
//! Frame skipping trade-off, measured on 720p30 screen-like content (static
//! desktop plus a scrolling window): without skipping the output stays close
//! to the target (1.11 Mbit/s for 1 Mbit/s) and no frame is dropped; with
//! skipping openh264 drops 11 to 27 of 90 frames. On worst-case content (full
//! frame noise) the bitrate overshoots far above target without skipping, so
//! the adaptive controller (Etapa 3) must lower bitrate or scale; it must not
//! rely on the encoder to hold the target.

use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig as OhConfig, FrameRate as OhFrameRate, FrameType,
    IntraFramePeriod, Profile, RateControlMode, UsageType,
};
use openh264::formats::{YUVSlices, YUVSource};
use openh264::OpenH264API;

use super::{
    Backend, BackendCaps, CodecError, EncodedFrame, EncoderConfig, H264Profile, VideoDecoder,
    VideoEncoder,
};
use crate::frame::YuvFrame;
use crate::scale::Resolution;

pub(crate) fn caps() -> BackendCaps {
    BackendCaps {
        backend: Backend::Software,
        encode: true,
        decode: true,
        max_resolution: None,
    }
}

fn err(e: impl std::fmt::Display) -> CodecError {
    CodecError::backend(Backend::Software, e)
}

fn build(config: &EncoderConfig) -> Result<Encoder, CodecError> {
    // openh264 has no Main profile support worth using; both map to Baseline.
    let profile = match config.profile {
        H264Profile::ConstrainedBaseline | H264Profile::Main => Profile::Baseline,
    };
    let oh = OhConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(config.bitrate_bps))
        .max_frame_rate(OhFrameRate::from_hz(f32::from(config.frame_rate.fps())))
        .profile(profile)
        .complexity(Complexity::Low)
        .skip_frames(false)
        .scene_change_detect(false)
        .background_detection(false)
        .adaptive_quantization(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(
            config.keyframe_interval_frames(),
        ));
    Encoder::with_api_config(OpenH264API::from_source(), oh).map_err(err)
}

pub struct SoftwareEncoder {
    config: EncoderConfig,
    encoder: Encoder,
    force_idr: bool,
}

impl SoftwareEncoder {
    pub fn new(config: &EncoderConfig) -> Result<Self, CodecError> {
        config.validate()?;
        Ok(Self {
            config: config.clone(),
            encoder: build(config)?,
            // The first frame of a fresh encoder is an IDR anyway; ask for it so
            // a replacement encoder (after a fallback) starts clean as well.
            force_idr: true,
        })
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn backend(&self) -> Backend {
        Backend::Software
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &YuvFrame,
        timestamp_us: u64,
    ) -> Result<Option<EncodedFrame>, CodecError> {
        if frame.resolution() != self.config.resolution {
            return Err(CodecError::ResolutionMismatch {
                expected: self.config.resolution,
                got: frame.resolution(),
            });
        }
        if std::mem::take(&mut self.force_idr) {
            self.encoder.force_intra_frame();
        }
        let r = frame.resolution();
        let (w, h) = (r.width as usize, r.height as usize);
        let source = YUVSlices::new((frame.y(), frame.u(), frame.v()), (w, h), (w, w / 2, w / 2));
        let bitstream = self.encoder.encode(&source).map_err(err)?;
        let frame_type = bitstream.frame_type();
        let data = bitstream.to_vec();
        if data.is_empty() || matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            data,
            keyframe: matches!(frame_type, FrameType::IDR),
            timestamp_us,
        }))
    }

    fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// openh264's bitrate can only be changed through its raw C API, so the
    /// encoder is rebuilt; the next frame is an IDR.
    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        if bps == 0 {
            return Err(CodecError::InvalidConfig("bitrate is zero"));
        }
        if bps == self.config.bitrate_bps {
            return Ok(());
        }
        let mut config = self.config.clone();
        config.bitrate_bps = bps;
        self.encoder = build(&config)?;
        self.config = config;
        self.force_idr = true;
        Ok(())
    }
}

pub struct SoftwareDecoder {
    decoder: Decoder,
}

impl SoftwareDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            decoder: Decoder::new().map_err(err)?,
        })
    }
}

impl VideoDecoder for SoftwareDecoder {
    fn backend(&self) -> Backend {
        Backend::Software
    }

    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        let Some(picture) = self.decoder.decode(access_unit).map_err(err)? else {
            return Ok(None);
        };
        let (w, h) = picture.dimensions();
        let (sy, su, sv) = picture.strides();
        let resolution = Resolution::new(w as u32, h as u32);
        let frame = YuvFrame::from_strided(
            resolution,
            [(picture.y(), sy), (picture.u(), su), (picture.v(), sv)],
        )?;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scale::{fit_preset, Alignment, FrameRate, Preset, VideoProfile};

    /// A frame with a moving gradient so the encoder has real work.
    fn pattern(r: Resolution, t: u32) -> YuvFrame {
        let mut f = YuvFrame::black(r).unwrap();
        let w = r.width as usize;
        let (y, u, v) = f.planes_mut();
        for (i, px) in y.iter_mut().enumerate() {
            let (x, row) = (i % w, i / w);
            *px = 16 + ((x + row + t as usize * 4) % 200) as u8;
        }
        u.fill(100);
        v.fill(150);
        f
    }

    fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
        let sum: u64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        sum as f64 / a.len() as f64
    }

    fn roundtrip(resolution: Resolution, profile: VideoProfile, frames: u32) {
        let config = EncoderConfig::new(resolution, profile);
        let mut enc = SoftwareEncoder::new(&config).unwrap();
        let mut dec = SoftwareDecoder::new().unwrap();
        let mut decoded = 0;
        for t in 0..frames {
            let input = pattern(resolution, t);
            let Some(out) = enc.encode(&input, u64::from(t)).unwrap() else {
                continue;
            };
            assert_eq!(out.keyframe, t == 0, "frame {t}");
            if let Some(picture) = dec.decode(&out.data).unwrap() {
                assert_eq!(picture.resolution(), resolution);
                let diff = mean_abs_diff(picture.y(), input.y());
                assert!(diff < 6.0, "{resolution} frame {t}: luma error {diff}");
                decoded += 1;
            }
        }
        assert!(
            decoded >= frames - 1,
            "{resolution}: decoded {decoded}/{frames}"
        );
    }

    #[test]
    fn every_preset_roundtrips_up_to_2560x1440() {
        let sources = [
            Resolution::new(1920, 1080),
            Resolution::new(2560, 1440),
            Resolution::new(2560, 1600),
            Resolution::new(3440, 1440),
        ];
        for preset in [Preset::P720, Preset::P1080, Preset::P1440, Preset::Native] {
            for rate in [FrameRate::Fps30, FrameRate::Fps60] {
                for src in sources {
                    let r = fit_preset(src, preset, Alignment::Two).unwrap();
                    roundtrip(r, VideoProfile::new(preset, rate), 3);
                }
            }
        }
    }

    #[test]
    fn periodic_and_requested_idrs() {
        let r = Resolution::new(320, 180);
        let mut config = EncoderConfig::new(r, VideoProfile::new(Preset::P720, FrameRate::Fps30));
        config.keyframe_interval = std::time::Duration::from_millis(200); // 6 frames
        let mut enc = SoftwareEncoder::new(&config).unwrap();
        let mut keyframes = Vec::new();
        for t in 0..17 {
            if t == 9 {
                enc.request_keyframe();
            }
            let out = enc.encode(&pattern(r, t), 0).unwrap().unwrap();
            if out.keyframe {
                keyframes.push(t);
            }
        }
        // The period restarts after a requested IDR: 9 + 6 = 15.
        assert_eq!(keyframes, vec![0, 6, 9, 15]);
    }

    #[test]
    fn bitrate_change_restarts_with_an_idr() {
        let r = Resolution::new(320, 180);
        let config = EncoderConfig::new(r, VideoProfile::new(Preset::P720, FrameRate::Fps60));
        let mut enc = SoftwareEncoder::new(&config).unwrap();
        enc.encode(&pattern(r, 0), 0).unwrap();
        assert!(!enc.encode(&pattern(r, 1), 1).unwrap().unwrap().keyframe);
        enc.set_bitrate(500_000).unwrap();
        assert_eq!(enc.config().bitrate_bps, 500_000);
        assert!(enc.encode(&pattern(r, 2), 2).unwrap().unwrap().keyframe);
        assert!(enc.set_bitrate(0).is_err());
    }

    #[test]
    fn wrong_frame_size_and_garbage_input_are_errors() {
        let r = Resolution::new(320, 180);
        let config = EncoderConfig::new(r, VideoProfile::new(Preset::P720, FrameRate::Fps60));
        let mut enc = SoftwareEncoder::new(&config).unwrap();
        let other = YuvFrame::black(Resolution::new(160, 90 + 2)).unwrap();
        assert!(matches!(
            enc.encode(&other, 0),
            Err(CodecError::ResolutionMismatch { .. })
        ));
        let mut dec = SoftwareDecoder::new().unwrap();
        for junk in [&[][..], &[0, 0, 0, 1, 0xff, 0xff][..], &[7u8; 300][..]] {
            // Must return, never panic; garbage may be an error or no picture.
            let _ = dec.decode(junk);
        }
    }
}
