//! How the codec layer behaves around the edges of a live stream: periodic and
//! on-demand IDR, late joiners, lost frames, size changes, bad input and the
//! hardware -> software fallback. Every case runs on the software backend (CI)
//! and, when a VA-API device is present, on VA-API in every pairing.

mod common;

use common::*;
use v6cast_media::codec::{
    BackendPreference, CodecError, EncodedFrame, EncoderConfig, FallbackDecoder, FallbackEncoder,
};
use v6cast_media::{
    open_decoder, open_encoder, Backend, FrameRate, Preset, Resolution, VideoDecoder, VideoEncoder,
    VideoProfile, YuvFrame,
};

/// A frame of the synthetic screen already at `out` size.
fn frame(out: Resolution, t: u32) -> YuvFrame {
    capture_frame(out, out, t)
}

fn encode(enc: &mut dyn VideoEncoder, out: Resolution, t: u32) -> EncodedFrame {
    enc.encode(&frame(out, t), u64::from(t) * 16_666)
        .unwrap_or_else(|e| panic!("frame {t}: {e}"))
        .unwrap_or_else(|| panic!("frame {t}: the encoder skipped the frame"))
}

fn decode_good(dec: &mut dyn VideoDecoder, au: &EncodedFrame, input: &YuvFrame, what: &str) {
    let picture = dec
        .decode(&au.data)
        .unwrap_or_else(|e| panic!("{what}: decode failed: {e}"))
        .unwrap_or_else(|| panic!("{what}: no picture"));
    assert_eq!(picture.resolution(), input.resolution(), "{what}: size");
    let p = frame_psnr(&picture, input);
    assert!(
        p.y >= 30.0 && p.u >= 30.0 && p.v >= 30.0,
        "{what}: PSNR {:.1}/{:.1}/{:.1}",
        p.y,
        p.u,
        p.v
    );
}

fn keyframes(enc: &mut dyn VideoEncoder, out: Resolution, frames: u32) -> Vec<u32> {
    (0..frames)
        .filter(|&t| encode(enc, out, t).keyframe)
        .collect()
}

fn config(out: Resolution, preset: Preset, rate: FrameRate) -> EncoderConfig {
    EncoderConfig::new(out, VideoProfile::new(preset, rate))
}

fn open(which: Which, config: &EncoderConfig) -> Box<dyn VideoEncoder> {
    Box::new(open_encoder(config, which.preference()).expect("open encoder"))
}

// ---------------------------------------------------------------------------
// Cases, run once per backend pairing (see the macro at the bottom)
// ---------------------------------------------------------------------------

fn periodic_idr_follows_the_keyframe_interval(enc: Which, _dec: Which) {
    let out = r(320, 192);
    // Default interval: 2 s, i.e. 60 frames at 30 fps and 120 at 60 fps.
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps30));
    assert_eq!(keyframes(e.as_mut(), out, 130), vec![0, 60, 120], "30 fps");
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    assert_eq!(keyframes(e.as_mut(), out, 250), vec![0, 120, 240], "60 fps");
    // A shorter interval is honoured (18 frames: the shortest VA-API takes, see
    // `a_keyframe_interval_under_16_frames_does_not_break_the_vaapi_encoder`).
    let mut cfg = config(out, Preset::P720, FrameRate::Fps30);
    cfg.keyframe_interval = std::time::Duration::from_millis(600);
    let mut e = open(enc, &cfg);
    assert_eq!(
        keyframes(e.as_mut(), out, 60),
        vec![0, 18, 36, 54],
        "600 ms"
    );
}

fn idr_on_demand(enc: Which, dec: Which) {
    let out = r(640, 368);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    let mut d = decoder_for(dec);
    let mut flags = Vec::new();
    for t in 0..14 {
        match t {
            4 => {
                // Asking twice in a row still gives one IDR.
                e.request_keyframe();
                e.request_keyframe();
            }
            8 | 9 => e.request_keyframe(), // two frames in a row
            _ => {}
        }
        let au = encode(e.as_mut(), out, t);
        assert_well_formed(&au, &format!("frame {t}"));
        flags.push(au.keyframe);
        decode_good(&mut d, &au, &frame(out, t), &format!("frame {t}"));
    }
    let idr: Vec<usize> = flags
        .iter()
        .enumerate()
        .filter(|(_, k)| **k)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(idr, vec![0, 4, 8, 9], "IDR exactly where it was asked for");
}

fn a_late_joiner_waits_for_an_idr_and_then_decodes(enc: Which, dec: Which) {
    let out = r(640, 368);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    for t in 0..5 {
        encode(e.as_mut(), out, t); // the viewer was not there
    }
    let mut d = decoder_for(dec);
    for t in 5..7 {
        // Delta frames without their reference: no panic, no hang, an error or
        // nothing is fine.
        let au = encode(e.as_mut(), out, t);
        let _ = d.decode(&au.data);
    }
    e.request_keyframe(); // what the viewer asks for when it joins
    let au = encode(e.as_mut(), out, 7);
    assert!(au.keyframe, "the requested frame is an IDR");
    decode_good(&mut d, &au, &frame(out, 7), "first IDR");
    for t in 8..11 {
        let au = encode(e.as_mut(), out, t);
        decode_good(&mut d, &au, &frame(out, t), &format!("frame {t}"));
    }
}

fn a_lost_frame_heals_at_the_next_idr(enc: Which, dec: Which) {
    let out = r(640, 368);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    let mut d = decoder_for(dec);
    for t in 0..3 {
        let au = encode(e.as_mut(), out, t);
        decode_good(&mut d, &au, &frame(out, t), &format!("frame {t}"));
    }
    let _lost = encode(e.as_mut(), out, 3); // never reaches the viewer
    for t in 4..6 {
        let au = encode(e.as_mut(), out, t);
        let _ = d.decode(&au.data); // damaged picture or an error, but no panic
    }
    e.request_keyframe();
    let au = encode(e.as_mut(), out, 6);
    assert!(au.keyframe);
    decode_good(&mut d, &au, &frame(out, 6), "IDR after the loss");
    for t in 7..9 {
        let au = encode(e.as_mut(), out, t);
        decode_good(&mut d, &au, &frame(out, t), &format!("frame {t}"));
    }
}

fn the_decoder_follows_size_changes_between_streams(enc: Which, dec: Which) {
    // The sender changes preset or the shared window is resized: a new encoder
    // starts a stream of another size and the same decoder must follow.
    let mut d = decoder_for(dec);
    let plan = [
        (r(1920, 1080), Preset::P720),
        (r(1920, 1080), Preset::P1080),
        (r(900, 2000), Preset::P1080),
        (r(1001, 777), Preset::P1440),
        (r(1920, 1080), Preset::P720),
    ];
    for (i, (source, preset)) in plan.into_iter().enumerate() {
        let out = v6cast_media::fit_preset(source, preset, alignment()).unwrap();
        let mut e = open(enc, &config(out, preset, FrameRate::Fps30));
        for t in 0..3 {
            let input = capture_frame(source, out, t);
            let au = e.encode(&input, u64::from(t)).unwrap().unwrap();
            assert_eq!(au.keyframe, t == 0);
            decode_good(
                &mut d,
                &au,
                &input,
                &format!("segment {i} ({out}) frame {t}"),
            );
        }
    }
}

fn bad_input_does_not_kill_the_decoder(enc: Which, dec: Which) {
    let out = r(640, 368);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    let mut d = decoder_for(dec);
    let good = encode(e.as_mut(), out, 0);
    decode_good(&mut d, &good, &frame(out, 0), "before the garbage");
    let mut rng = Rng(0x5eed_1234_abcd);
    let mut garbage: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![0, 0, 0, 1],
        vec![0, 0, 0, 1, 0x80], // forbidden_zero_bit set
        vec![0, 0, 0, 1, 0x67],
        vec![0u8; 4096],
        vec![0xffu8; 4096],
        good.data[..good.data.len() / 2].to_vec(), // cut in the middle
        good.data[..9.min(good.data.len())].to_vec(),
        [good.data.clone(), good.data.clone()].concat(), // the same IDR twice
        std::iter::repeat_n([0u8, 0, 1], 500).flatten().collect(),
    ];
    for _ in 0..30 {
        let len = 1 + rng.next() as usize % 3000;
        garbage.push((0..len).map(|_| rng.next() as u8).collect());
    }
    // The slices of the IDR with bits flipped (headers kept).
    for i in 0..20 {
        let mut bad = good.data.clone();
        let at = 40 + rng.next() as usize % (bad.len() - 40);
        bad[at] ^= 1 << (i % 8);
        garbage.push(bad);
    }
    for bytes in &garbage {
        let _ = d.decode(bytes); // any result, never a panic or a hang
    }
    // A clean IDR from a fresh encoder is decoded as if nothing had happened.
    let mut e2 = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    let au = encode(e2.as_mut(), out, 2);
    assert!(au.keyframe);
    decode_good(&mut d, &au, &frame(out, 2), "after the garbage");
}

fn the_bitrate_can_change_in_the_middle_of_a_stream(enc: Which, dec: Which) {
    let out = r(640, 368);
    let range = v6cast_media::codec::bitrate_range(Preset::P720, FrameRate::Fps60);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    let mut d = decoder_for(dec);
    let mut t = 0;
    for bps in [
        range.default_bps,
        range.min_bps,
        range.max_bps,
        range.default_bps,
    ] {
        e.set_bitrate(bps).unwrap();
        assert_eq!(e.config().bitrate_bps, bps);
        for _ in 0..3 {
            let au = encode(e.as_mut(), out, t);
            decode_good(
                &mut d,
                &au,
                &frame(out, t),
                &format!("frame {t} at {bps} bps"),
            );
            t += 1;
        }
    }
    assert!(e.set_bitrate(0).is_err(), "zero bitrate is refused");
    assert_eq!(
        e.config().bitrate_bps,
        range.default_bps,
        "a refused change changes nothing"
    );
    let au = encode(e.as_mut(), out, t);
    decode_good(&mut d, &au, &frame(out, t), "after the refused change");
}

fn a_frame_of_the_wrong_size_is_refused_and_the_encoder_goes_on(enc: Which, dec: Which) {
    let out = r(640, 368);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    let mut d = decoder_for(dec);
    let au = encode(e.as_mut(), out, 0);
    decode_good(&mut d, &au, &frame(out, 0), "frame 0");
    for wrong in [r(642, 368), r(320, 184), r(1280, 720)] {
        let f = YuvFrame::black(wrong).unwrap();
        match e.encode(&f, 1) {
            Err(CodecError::ResolutionMismatch { expected, got }) => {
                assert_eq!((expected, got), (out, wrong));
            }
            other => panic!("{wrong}: expected ResolutionMismatch, got {other:?}"),
        }
    }
    let au = encode(e.as_mut(), out, 1);
    assert!(!au.keyframe, "a refused frame does not reset the stream");
    decode_good(&mut d, &au, &frame(out, 1), "frame 1");
}

fn the_requested_idr_survives_a_refused_frame(enc: Which, dec: Which) {
    let out = r(640, 368);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps60));
    let mut d = decoder_for(dec);
    let au = encode(e.as_mut(), out, 0);
    decode_good(&mut d, &au, &frame(out, 0), "frame 0");
    e.request_keyframe();
    assert!(e.encode(&YuvFrame::black(r(320, 184)).unwrap(), 1).is_err());
    let au = encode(e.as_mut(), out, 1);
    assert!(au.keyframe, "the IDR that was asked for is still due");
}

fn noise_frame(out: Resolution, rng: &mut Rng) -> YuvFrame {
    let mut f = YuvFrame::black(out).unwrap();
    let (y, u, v) = f.planes_mut();
    for p in y.iter_mut() {
        *p = 16 + (rng.next() % 220) as u8;
    }
    for p in u.iter_mut().chain(v.iter_mut()) {
        *p = 16 + (rng.next() % 225) as u8;
    }
    f
}

/// Worst case for rate control: full-frame noise at the default bitrate. The
/// stream may overshoot (the adaptive controller handles that) but every frame
/// must still be encoded and shown: the frame rate has priority.
fn hard_content_never_makes_the_encoder_skip_a_frame(enc: Which, dec: Which) {
    let out = r(1280, 720);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps30));
    let mut d = decoder_for(dec);
    let mut rng = Rng(7);
    for t in 0..24u32 {
        let input = noise_frame(out, &mut rng);
        let au = e
            .encode(&input, u64::from(t) * 33_333)
            .unwrap_or_else(|e| panic!("frame {t}: {e}"))
            .unwrap_or_else(|| panic!("frame {t}: the encoder skipped a frame"));
        let picture = d
            .decode(&au.data)
            .unwrap_or_else(|e| panic!("frame {t}: decode: {e}"))
            .unwrap_or_else(|| panic!("frame {t}: no picture"));
        assert_eq!(picture.resolution(), out);
    }
}

/// A hard cut (a new window comes up, a game changes level) must not make the
/// encoder insert an IDR by itself: keyframes come from the interval and from
/// requests only, so the sender controls the keyframe cost.
fn a_scene_cut_does_not_cause_an_unrequested_idr(enc: Which, dec: Which) {
    let out = r(640, 368);
    let mut e = open(enc, &config(out, Preset::P720, FrameRate::Fps30));
    let mut d = decoder_for(dec);
    let mut idr = Vec::new();
    for t in 0..30u32 {
        // Frames 0..15 show one screen, 15..30 a completely different one.
        let scene = t / 15;
        let mut input = frame(out, t);
        if scene == 1 {
            let (y, u, v) = input.planes_mut();
            y.iter_mut().for_each(|p| *p = 251 - *p);
            u.iter_mut().for_each(|p| *p = 255 - *p);
            v.iter_mut().for_each(|p| *p = 255 - *p);
        }
        let au = e.encode(&input, u64::from(t)).unwrap().unwrap();
        if au.keyframe {
            idr.push(t);
        }
        let _ = d.decode(&au.data).unwrap();
    }
    assert_eq!(idr, vec![0], "only the first frame is an IDR");
}

macro_rules! cases {
    (
        all: [$($case:ident),* $(,)?],
        vaapi_encoder_bug: [$($vbug:ident),* $(,)?],
        vaapi_reason: $vreason:literal,
        software_encoder_bug: [$($sbug:ident),* $(,)?],
        software_reason: $sreason:literal $(,)?
    ) => {
        // Software encoder: the cases that are known to fail on it are ignored.
        mod software {
            use super::*;
            $( #[test] fn $case() { super::$case(Which::Software, Which::Software) } )*
            $( #[test] fn $vbug() { super::$vbug(Which::Software, Which::Software) } )*
            $( #[test] #[ignore = $sreason] fn $sbug() { super::$sbug(Which::Software, Which::Software) } )*
        }
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        mod vaapi {
            use super::*;
            $( #[test] fn $case() {
                if vaapi_available() { super::$case(Which::Vaapi, Which::Vaapi) }
            } )*
            $( #[test] #[ignore = $vreason] fn $vbug() {
                if vaapi_available() { super::$vbug(Which::Vaapi, Which::Vaapi) }
            } )*
            $( #[test] fn $sbug() {
                if vaapi_available() { super::$sbug(Which::Vaapi, Which::Vaapi) }
            } )*
        }
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        mod vaapi_sender_software_viewer {
            use super::*;
            $( #[test] fn $case() {
                if vaapi_available() { super::$case(Which::Vaapi, Which::Software) }
            } )*
            $( #[test] #[ignore = $vreason] fn $vbug() {
                if vaapi_available() { super::$vbug(Which::Vaapi, Which::Software) }
            } )*
            $( #[test] fn $sbug() {
                if vaapi_available() { super::$sbug(Which::Vaapi, Which::Software) }
            } )*
        }
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        mod software_sender_vaapi_viewer {
            use super::*;
            $( #[test] fn $case() {
                if vaapi_available() { super::$case(Which::Software, Which::Vaapi) }
            } )*
            $( #[test] fn $vbug() {
                if vaapi_available() { super::$vbug(Which::Software, Which::Vaapi) }
            } )*
            $( #[test] #[ignore = $sreason] fn $sbug() {
                if vaapi_available() { super::$sbug(Which::Software, Which::Vaapi) }
            } )*
        }
    };
}

cases!(
    all: [
        periodic_idr_follows_the_keyframe_interval,
        the_decoder_follows_size_changes_between_streams,
        bad_input_does_not_kill_the_decoder,
        the_bitrate_can_change_in_the_middle_of_a_stream,
        a_frame_of_the_wrong_size_is_refused_and_the_encoder_goes_on,
        the_requested_idr_survives_a_refused_frame,
        hard_content_never_makes_the_encoder_skip_a_frame,
        idr_on_demand,
        a_late_joiner_waits_for_an_idr_and_then_decodes,
        a_lost_frame_heals_at_the_next_idr,
    ],
    vaapi_encoder_bug: [],
    vaapi_reason: "no known VA-API encoder bug",
    software_encoder_bug: [a_scene_cut_does_not_cause_an_unrequested_idr],
    software_reason: "known issue: openh264 in ScreenContentRealTime mode forces scene change \
detection on (it logs 'change bEnableSceneChangeDetect as true'), so a hard cut makes it insert an \
IDR nobody asked for, whatever .scene_change_detect(false) says. Run with --ignored to see it.",
);

/// Small windows stream on the software backend. (VA-API is kept out of this
/// on purpose: see `a_small_window_does_not_fault_the_gpu`.)
#[test]
fn small_windows_still_stream_on_the_software_backend() {
    for (w, h) in [
        (16, 16),
        (18, 18),
        (64, 64),
        (96, 54),
        (160, 90),
        (200, 100),
        (322, 18),
        (16, 640),
    ] {
        let out = r(w, h);
        let cfg = config(out, Preset::P720, FrameRate::Fps30);
        let mut enc = open_encoder(&cfg, BackendPreference::SoftwareOnly)
            .unwrap_or_else(|e| panic!("{out}: no encoder: {e}"));
        let mut dec = open_decoder(BackendPreference::SoftwareOnly).unwrap();
        for t in 0..3 {
            let input = frame(out, t);
            let au = enc
                .encode(&input, u64::from(t))
                .unwrap_or_else(|e| panic!("{out} frame {t}: {e}"))
                .unwrap_or_else(|| panic!("{out} frame {t}: skipped"));
            let picture = dec
                .decode(&au.data)
                .unwrap_or_else(|e| panic!("{out} frame {t}: decode: {e}"))
                .unwrap_or_else(|| panic!("{out} frame {t}: no picture"));
            assert_eq!(picture.resolution(), out, "{out} frame {t}");
        }
    }
}

/// openh264 cannot encode a side under 16 pixels (a thin panel or tooltip
/// window). That must come back as an error from `open_encoder` or `encode`,
/// never as a panic or a stream the decoder cannot read. (Software only, so no
/// GPU is involved.)
#[test]
fn sides_under_16_pixels_are_an_error_not_a_panic() {
    for (w, h) in [(2, 2), (8, 8), (640, 8), (8, 640), (2, 64)] {
        let out = r(w, h);
        let cfg = config(out, Preset::P720, FrameRate::Fps30);
        let outcome = open_encoder(&cfg, BackendPreference::SoftwareOnly)
            .and_then(|mut e| e.encode(&frame(out, 0), 0));
        if let Ok(Some(au)) = outcome {
            // If the backend ever supports it, the picture must be right.
            let mut dec = open_decoder(BackendPreference::SoftwareOnly).unwrap();
            let picture = dec.decode(&au.data).unwrap().expect("a picture");
            assert_eq!(picture.resolution(), out);
        }
    }
}

/// A 200x100 stream through the VA-API encoder made the AMD VCN ring fault (the
/// kernel reset the ring) and Mesa aborted the process, so no fallback could
/// catch it. Streams below `VAAPI_MIN_ENCODE_RESOLUTION` now never reach the
/// VA-API encoder: automatic selection picks openh264, and asking for hardware
/// only is refused before any VA object is created. This test is safe by
/// construction: it never opens a VA-API encoder below the minimum.
#[cfg(all(target_os = "linux", feature = "vaapi"))]
#[test]
fn a_small_window_does_not_fault_the_gpu() {
    let out = r(200, 100);
    let cfg = config(out, Preset::P720, FrameRate::Fps30);
    assert!(
        out.width < v6cast_media::codec::VAAPI_MIN_ENCODE_RESOLUTION.width,
        "the case must stay below the VA-API minimum"
    );
    // Hardware only: refused up front, whether or not a GPU is present.
    assert!(open_encoder(&cfg, BackendPreference::HardwareOnly).is_err());
    assert!(v6cast_media::vaapi::VaapiEncoder::new(&cfg).is_err());
    // Automatic: software, and the stream works.
    let mut enc = open_encoder(&cfg, BackendPreference::Auto).unwrap();
    assert_eq!(enc.backend(), Backend::Software);
    let mut dec = open_decoder(BackendPreference::SoftwareOnly).unwrap();
    for t in 0..3 {
        let input = frame(out, t);
        let au = enc.encode(&input, u64::from(t)).unwrap().unwrap();
        decode_good(&mut dec, &au, &input, &format!("200x100 frame {t}"));
    }
}

/// A keyframe interval under 16 frames is accepted by `EncoderConfig::validate`
/// but panics inside cros-codecs (`SpsBuilder::max_frame_num`: `ilog2() - 4`).
/// The worker thread dies and the encoder reports "worker thread stopped".
#[cfg(all(target_os = "linux", feature = "vaapi"))]
#[test]
#[ignore = "known bug: VA-API encoder worker panics for keyframe intervals under 16 frames"]
fn a_keyframe_interval_under_16_frames_does_not_break_the_vaapi_encoder() {
    if !vaapi_available() {
        return;
    }
    let out = r(320, 192);
    let mut cfg = config(out, Preset::P720, FrameRate::Fps30);
    cfg.keyframe_interval = std::time::Duration::from_millis(500); // 15 frames
    assert!(
        cfg.validate().is_ok(),
        "the config is valid as far as validate() knows"
    );
    let mut e = open(Which::Vaapi, &cfg);
    assert_eq!(keyframes(e.as_mut(), out, 50), vec![0, 15, 30, 45]);
}

// ---------------------------------------------------------------------------
// Fallback (the hardware backends are stood in for by wrappers that fail)
// ---------------------------------------------------------------------------

/// Passes frames to openh264 but reports itself as VA-API and fails from frame
/// `fail_at` on, the way a GPU that hangs does.
struct DyingHardwareEncoder {
    inner: Box<dyn VideoEncoder>,
    seen: u32,
    fail_at: u32,
}

impl VideoEncoder for DyingHardwareEncoder {
    fn backend(&self) -> Backend {
        Backend::Vaapi
    }
    fn config(&self) -> &EncoderConfig {
        self.inner.config()
    }
    fn encode(&mut self, f: &YuvFrame, ts: u64) -> Result<Option<EncodedFrame>, CodecError> {
        self.seen += 1;
        if self.seen > self.fail_at {
            return Err(CodecError::Backend {
                backend: Backend::Vaapi,
                message: "GPU hang".into(),
            });
        }
        self.inner.encode(f, ts)
    }
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }
    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        self.inner.set_bitrate(bps)
    }
}

struct DyingHardwareDecoder {
    inner: Box<dyn VideoDecoder>,
    seen: u32,
    fail_at: u32,
}

impl VideoDecoder for DyingHardwareDecoder {
    fn backend(&self) -> Backend {
        Backend::Vaapi
    }
    fn decode(&mut self, au: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        self.seen += 1;
        if self.seen > self.fail_at {
            return Err(CodecError::Backend {
                backend: Backend::Vaapi,
                message: "context lost".into(),
            });
        }
        self.inner.decode(au)
    }
}

#[test]
fn a_gpu_dying_mid_stream_costs_one_idr_and_no_frame() {
    let out = r(1280, 720);
    let cfg = config(out, Preset::P720, FrameRate::Fps60);
    let inner = Box::new(open_encoder(&cfg, BackendPreference::SoftwareOnly).unwrap());
    let mut enc = FallbackEncoder::new(
        Box::new(DyingHardwareEncoder {
            inner,
            seen: 0,
            fail_at: 4,
        }),
        true,
    );
    let mut dec = open_decoder(BackendPreference::SoftwareOnly).unwrap();
    let mut idr = Vec::new();
    for t in 0..10 {
        let input = frame(out, t);
        let au = enc
            .encode(&input, u64::from(t))
            .unwrap_or_else(|e| panic!("frame {t}: {e}"))
            .unwrap_or_else(|| panic!("frame {t}: skipped"));
        assert_eq!(au.timestamp_us, u64::from(t));
        if au.keyframe {
            idr.push(t);
        }
        decode_good(&mut dec, &au, &input, &format!("frame {t}"));
        assert_eq!(
            enc.backend(),
            if t < 4 {
                Backend::Vaapi
            } else {
                Backend::Software
            },
            "frame {t}"
        );
    }
    assert!(enc.fell_back());
    assert_eq!(idr, vec![0, 4], "an IDR right at the switch");
    // Keyframe requests and bitrate changes reach the new encoder.
    enc.set_bitrate(1_500_000).unwrap();
    assert_eq!(enc.config().bitrate_bps, 1_500_000);
}

#[test]
fn a_gpu_decoder_dying_mid_stream_resumes_at_the_next_idr() {
    let out = r(1280, 720);
    let cfg = config(out, Preset::P720, FrameRate::Fps60);
    let mut enc = open_encoder(&cfg, BackendPreference::SoftwareOnly).unwrap();
    let inner = Box::new(open_decoder(BackendPreference::SoftwareOnly).unwrap());
    let mut dec = FallbackDecoder::new(
        Box::new(DyingHardwareDecoder {
            inner,
            seen: 0,
            fail_at: 3,
        }),
        true,
    );
    for t in 0..3 {
        let au = encode(&mut enc, out, t);
        decode_good(&mut dec, &au, &frame(out, t), &format!("frame {t}"));
    }
    // Frame 3 is a delta frame: the new decoder cannot show it, and says so.
    let au = encode(&mut enc, out, 3);
    assert!(
        dec.decode(&au.data).unwrap().is_none(),
        "no picture without a reference"
    );
    assert!(dec.fell_back());
    assert!(
        dec.take_keyframe_request(),
        "the viewer must ask for an IDR"
    );
    assert!(!dec.take_keyframe_request(), "only once");
    enc.request_keyframe();
    let au = encode(&mut enc, out, 4);
    assert!(au.keyframe);
    decode_good(&mut dec, &au, &frame(out, 4), "IDR after the switch");
    assert_eq!(dec.backend(), Backend::Software);
}
