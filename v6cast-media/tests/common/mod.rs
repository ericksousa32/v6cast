//! Shared pieces of the local loopback harness: a synthetic screen, the
//! metrics used to judge it, and the encode -> decode loop.
//!
//! Everything here uses the public API of `v6cast-media` only.

#![allow(dead_code)] // each test binary uses a different part

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use v6cast_media::capture::{to_i420, PixelLayout, RawImage};
use v6cast_media::codec::{
    open_decoder, open_encoder, probe, BackendPreference, EncodedFrame, EncoderConfig,
};
use v6cast_media::{
    Alignment, Backend, FrameRate, Preset, Resolution, VideoDecoder, VideoEncoder, VideoProfile,
    YuvFrame,
};

pub const PRESETS: [Preset; 4] = [Preset::P720, Preset::P1080, Preset::P1440, Preset::Native];
pub const RATES: [FrameRate; 2] = [FrameRate::Fps30, FrameRate::Fps60];

pub fn r(w: u32, h: u32) -> Resolution {
    Resolution::new(w, h)
}

/// Writes straight to stderr: libtest does not capture it, so a skip is visible
/// in a normal `cargo test` run.
pub fn say(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
}

pub fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

// ---------------------------------------------------------------------------
// Synthetic screen
// ---------------------------------------------------------------------------

/// Side of the white marker square for a source of `size`: one third of the
/// short side, even so it is centered on whole pixels.
pub fn marker_side(size: Resolution) -> u32 {
    (size.width.min(size.height) / 3) & !1
}

/// A captured screen as the PipeWire buffer would deliver it: BGRx with
/// padded rows. A smooth gradient, a static white square in the middle (the
/// "stretch detector": it must stay square), a red block that moves with `t`
/// and a few thin lines so scaling has edges to get wrong.
pub fn synthetic_screen(size: Resolution, t: u32) -> (Vec<u8>, usize) {
    static BASES: OnceLock<Mutex<HashMap<Resolution, std::sync::Arc<Vec<u8>>>>> = OnceLock::new();
    let (w, h) = (size.width as usize, size.height as usize);
    let stride = w * 4 + 64;
    let base = {
        let mut bases = BASES.get_or_init(Default::default).lock().unwrap();
        bases
            .entry(size)
            .or_insert_with(|| std::sync::Arc::new(still_screen(size, stride)))
            .clone()
    };
    let mut data = base.as_ref().clone();
    // The moving block, painted over the still image (above the marker's rows).
    let block = (w.min(h) / 12).max(2);
    let travel = (w - block).max(1);
    let bx0 = (t as usize * 7 + t as usize * t as usize) % travel;
    let by0 = h / 12;
    for y in by0..(by0 + block).min(h) {
        for x in bx0..(bx0 + block).min(w) {
            let i = y * stride + x * 4;
            data[i..i + 3].copy_from_slice(&[0, 0, 255]); // BGR: red
        }
    }
    (data, stride)
}

/// The part of the screen that never changes.
fn still_screen(size: Resolution, stride: usize) -> Vec<u8> {
    let (w, h) = (size.width as usize, size.height as usize);
    let mut data = vec![0xAAu8; stride * h]; // padding is garbage on purpose
    let side = marker_side(size) as usize;
    let (mx0, my0) = ((w - side) / 2, (h - side) / 2);
    for y in 0..h {
        let green = (30 + 100 * y / h) as u8;
        for x in 0..w {
            let (mut red, mut green, mut blue) = ((30 + 100 * x / w) as u8, green, 60u8);
            if (x / 64 + y / 64) % 2 == 0 && (x % 64 == 0 || y % 64 == 0) {
                (red, green, blue) = (200, 200, 60); // thin grid lines
            }
            if x >= mx0 && x < mx0 + side && y >= my0 && y < my0 + side {
                (red, green, blue) = (255, 255, 255);
            }
            let i = y * stride + x * 4;
            data[i..i + 4].copy_from_slice(&[blue, green, red, 0xff]);
        }
    }
    data
}

/// Capture -> scaler: what `ScreenCapture` does for every frame. Results are
/// cached per `(source, target, t)`: the scaler is slow in debug builds and the
/// same frames are needed by several backends and frame rates.
pub fn capture_frame(source: Resolution, target: Resolution, t: u32) -> YuvFrame {
    type Key = (Resolution, Resolution, u32);
    static CACHE: OnceLock<Mutex<HashMap<Key, YuvFrame>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(frame) = cache.lock().unwrap().get(&(source, target, t)) {
        return frame.clone();
    }
    let frame = scale_capture(source, target, t);
    if t < 8 {
        // Long runs would otherwise keep every frame in memory.
        cache
            .lock()
            .unwrap()
            .insert((source, target, t), frame.clone());
    }
    frame
}

fn scale_capture(source: Resolution, target: Resolution, t: u32) -> YuvFrame {
    let (data, stride) = synthetic_screen(source, t);
    to_i420(
        &RawImage {
            data: &data,
            stride,
            size: source,
            layout: PixelLayout::Bgrx,
        },
        target,
    )
    .expect("scaling a valid capture")
}

/// Same image but with every pixel a flat color, for exact scaler checks.
pub fn flat_screen(size: Resolution, rgb: [u8; 3], stride: usize) -> Vec<u8> {
    let mut data = vec![0x55u8; stride * size.height as usize];
    for y in 0..size.height as usize {
        for x in 0..size.width as usize {
            let i = y * stride + x * 4;
            data[i..i + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0xff]);
        }
    }
    data
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Peak signal-to-noise ratio of two equally long 8-bit planes, in dB.
/// Identical planes give infinity.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "planes differ in size");
    let sse: u64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            let d = u64::from(x.abs_diff(*y));
            d * d
        })
        .sum();
    if sse == 0 {
        return f64::INFINITY;
    }
    let mse = sse as f64 / a.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

pub struct PlanePsnr {
    pub y: f64,
    pub u: f64,
    pub v: f64,
}

pub fn frame_psnr(a: &YuvFrame, b: &YuvFrame) -> PlanePsnr {
    assert_eq!(a.resolution(), b.resolution());
    PlanePsnr {
        y: psnr(a.y(), b.y()),
        u: psnr(a.u(), b.u()),
        v: psnr(a.v(), b.v()),
    }
}

/// Bounding box `(x0, y0, x1, y1)` (inclusive) of the pixels of `plane`
/// (`width` wide) at or above `threshold`.
pub fn bright_bbox(
    plane: &[u8],
    width: usize,
    threshold: u8,
) -> Option<(usize, usize, usize, usize)> {
    let mut found: Option<(usize, usize, usize, usize)> = None;
    for (i, &p) in plane.iter().enumerate() {
        if p >= threshold {
            let (x, y) = (i % width, i / width);
            found = Some(match found {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
    }
    found
}

/// Checks that the marker square of a `source` capture is still a centered
/// square in `plane` (luma of a frame of size `out`), within `tol` pixels.
/// This is the "no stretch" property: a uniform scale keeps `width == height`.
pub fn assert_marker_is_square(
    plane: &[u8],
    out: Resolution,
    source: Resolution,
    tol: f64,
    what: &str,
) {
    let side = f64::from(marker_side(source));
    let (sx, sy) = (
        f64::from(out.width) / f64::from(source.width),
        f64::from(out.height) / f64::from(source.height),
    );
    let (x0, y0, x1, y1) = bright_bbox(plane, out.width as usize, 200)
        .unwrap_or_else(|| panic!("{what}: the marker is gone"));
    let (bw, bh) = ((x1 - x0 + 1) as f64, (y1 - y0 + 1) as f64);
    assert!(
        (bw - side * sx).abs() <= tol && (bh - side * sy).abs() <= tol,
        "{what}: marker {bw}x{bh}, expected {:.1}x{:.1} (source {source}, output {out})",
        side * sx,
        side * sy
    );
    assert!(
        (bw - bh).abs() <= tol * 2.0,
        "{what}: marker is {bw}x{bh}, not square: the picture was stretched"
    );
    let (cx, cy) = (
        (x0 + x1 + 1) as f64 / 2.0 - f64::from(out.width) / 2.0,
        (y0 + y1 + 1) as f64 / 2.0 - f64::from(out.height) / 2.0,
    );
    assert!(
        cx.abs() <= tol && cy.abs() <= tol,
        "{what}: marker off-center by ({cx:.1}, {cy:.1}) px"
    );
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Software,
    Vaapi,
}

impl Which {
    pub fn preference(self) -> BackendPreference {
        match self {
            Which::Software => BackendPreference::SoftwareOnly,
            Which::Vaapi => BackendPreference::HardwareOnly,
        }
    }

    pub fn backend(self) -> Backend {
        match self {
            Which::Software => Backend::Software,
            Which::Vaapi => Backend::Vaapi,
        }
    }
}

/// True when this build and machine have a VA-API device that encodes and
/// decodes H.264. Says why not on stderr; with `V6CAST_REQUIRE_VAAPI=1` a
/// missing device is a failure, so a container run cannot pass by skipping.
pub fn vaapi_available() -> bool {
    let caps = probe();
    let ok = caps
        .iter()
        .any(|c| c.backend == Backend::Vaapi && c.encode && c.decode);
    if !ok {
        let why = if cfg!(all(target_os = "linux", feature = "vaapi")) {
            "no VA-API device with H.264 encode and decode (is /dev/dri available?)"
        } else {
            "built without the `vaapi` feature"
        };
        assert!(
            !env_flag("V6CAST_REQUIRE_VAAPI"),
            "V6CAST_REQUIRE_VAAPI is set but: {why}"
        );
        say(&format!("SKIPPED (VA-API): {why}"));
    }
    ok
}

pub fn encoder_for(
    which: Which,
    out: Resolution,
    preset: Preset,
    rate: FrameRate,
) -> impl VideoEncoder {
    let config = EncoderConfig::new(out, VideoProfile::new(preset, rate));
    let enc = open_encoder(&config, which.preference()).expect("open encoder");
    assert_eq!(enc.backend(), which.backend(), "wrong encoder backend");
    enc
}

pub fn decoder_for(which: Which) -> impl VideoDecoder {
    let dec = open_decoder(which.preference()).expect("open decoder");
    assert_eq!(dec.backend(), which.backend(), "wrong decoder backend");
    dec
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// What one frame went through.
pub struct Step {
    pub t: u32,
    pub input: YuvFrame,
    pub encoded: EncodedFrame,
    pub decoded: YuvFrame,
    pub encode_time: Duration,
    pub decode_time: Duration,
    pub psnr: PlanePsnr,
}

/// Drives `frames` frames of the synthetic screen through
/// capture -> scaler -> encoder -> decoder, one after the other, the way a
/// real-time pipeline does, and returns every step.
///
/// Asserts the properties that hold for every frame, whatever the backend:
/// one access unit per frame (no frame is skipped), a picture back for every
/// access unit (no decode delay), timestamps passed through unchanged, and
/// the decoded size equal to the encoded size.
pub fn run_loopback(
    enc: &mut dyn VideoEncoder,
    dec: &mut dyn VideoDecoder,
    source: Resolution,
    out: Resolution,
    rate: FrameRate,
    frames: u32,
    what: &str,
) -> Vec<Step> {
    let interval_us = 1_000_000 / u64::from(rate.fps());
    let mut steps = Vec::new();
    for t in 0..frames {
        let input = capture_frame(source, out, t);
        assert_eq!(input.resolution(), out, "{what}: scaler output size");
        let timestamp = u64::from(t) * interval_us;
        let a = Instant::now();
        let encoded = enc
            .encode(&input, timestamp)
            .unwrap_or_else(|e| panic!("{what} frame {t}: encode failed: {e}"))
            .unwrap_or_else(|| panic!("{what} frame {t}: the encoder skipped the frame"));
        let encode_time = a.elapsed();
        assert_eq!(
            encoded.timestamp_us, timestamp,
            "{what} frame {t}: timestamp"
        );
        assert!(
            !encoded.data.is_empty(),
            "{what} frame {t}: empty access unit"
        );
        let b = Instant::now();
        let decoded = dec
            .decode(&encoded.data)
            .unwrap_or_else(|e| panic!("{what} frame {t}: decode failed: {e}"))
            .unwrap_or_else(|| panic!("{what} frame {t}: no picture (decoder delay)"));
        let decode_time = b.elapsed();
        assert_eq!(decoded.resolution(), out, "{what} frame {t}: decoded size");
        let psnr = frame_psnr(&input, &decoded);
        steps.push(Step {
            t,
            input,
            encoded,
            decoded,
            encode_time,
            decode_time,
            psnr,
        });
    }
    steps
}

pub fn alignment() -> Alignment {
    Alignment::Two
}

/// Limits for one stream check.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Lowest luma PSNR (dB) of any decoded frame against the encoder input.
    pub min_psnr_y: f64,
    /// Lowest chroma PSNR (dB).
    pub min_psnr_uv: f64,
    /// Longest encode + decode of one frame.
    pub max_frame_time: Duration,
}

impl Limits {
    /// Loose limits that any working backend meets, in a debug or a release
    /// build: they catch hangs, garbage and wrong pictures, not slowness.
    pub fn for_tests() -> Self {
        Self {
            min_psnr_y: 30.0,
            min_psnr_uv: 30.0,
            max_frame_time: if cfg!(debug_assertions) {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(250)
            },
        }
    }
}

/// NAL unit types of an access unit, in order.
pub fn nal_types(au: &[u8]) -> Vec<u8> {
    v6cast_media::codec::h264::nal_unit_types(au)
}

/// What every encoded access unit must look like, whatever made it.
pub fn assert_well_formed(au: &EncodedFrame, what: &str) {
    // Annex B: zero bytes, then 00 00 01. VA-API output has one zero byte more
    // than the usual 4-byte start code on delta frames; decoders accept that.
    let zeros = au.data.iter().take_while(|b| **b == 0).count();
    assert!(
        zeros >= 2 && au.data.get(zeros) == Some(&1),
        "{what}: not Annex B (no start code)"
    );
    let types = nal_types(&au.data);
    assert!(!types.is_empty(), "{what}: no NAL units");
    assert!(
        types.iter().all(|t| (1..=12).contains(t)),
        "{what}: unexpected NAL types {types:?}"
    );
    let idr = types.contains(&5);
    assert_eq!(
        au.keyframe, idr,
        "{what}: keyframe flag disagrees with the bitstream ({types:?})"
    );
    if idr {
        let pos = |t: u8| types.iter().position(|x| *x == t);
        let (sps, pps, slice) = (pos(7), pos(8), pos(5));
        assert!(
            sps.is_some() && pps.is_some() && sps < pps && pps < slice,
            "{what}: an IDR must carry SPS and PPS in front of the slice, got {types:?}"
        );
    } else {
        assert!(
            types.contains(&1),
            "{what}: a delta frame must have a non-IDR slice, got {types:?}"
        );
    }
}

/// One full stream check: `frames` frames of `source` scaled to `out`, encoded
/// by `enc` and decoded by `dec`. Returns a one-line summary for the log.
pub fn check_stream(
    enc: Which,
    dec: Which,
    source: Resolution,
    preset: Preset,
    rate: FrameRate,
    frames: u32,
    limits: Limits,
) -> String {
    let out = v6cast_media::fit_preset(source, preset, alignment()).unwrap();
    let what = format!("{enc:?}->{dec:?} {source} {preset:?} {}fps", rate.fps());
    let mut e = encoder_for(enc, out, preset, rate);
    let mut d = decoder_for(dec);
    let steps = run_loopback(&mut e, &mut d, source, out, rate, frames, &what);
    let mut worst_y = f64::INFINITY;
    let mut worst_uv = f64::INFINITY;
    let (mut bytes, mut slowest) = (0usize, Duration::ZERO);
    for s in &steps {
        let tag = format!("{what} frame {}", s.t);
        assert_well_formed(&s.encoded, &tag);
        assert_eq!(
            s.encoded.keyframe,
            s.t == 0,
            "{tag}: only the first frame is an IDR here"
        );
        assert!(
            s.psnr.y >= limits.min_psnr_y,
            "{tag}: luma PSNR {:.1} dB < {}",
            s.psnr.y,
            limits.min_psnr_y
        );
        assert!(
            s.psnr.u >= limits.min_psnr_uv && s.psnr.v >= limits.min_psnr_uv,
            "{tag}: chroma PSNR {:.1}/{:.1} dB < {}",
            s.psnr.u,
            s.psnr.v,
            limits.min_psnr_uv
        );
        let took = s.encode_time + s.decode_time;
        assert!(
            took <= limits.max_frame_time,
            "{tag}: encode+decode took {took:?} (limit {:?})",
            limits.max_frame_time
        );
        assert_marker_is_square(s.decoded.y(), out, source, 4.0, &tag);
        worst_y = worst_y.min(s.psnr.y);
        worst_uv = worst_uv.min(s.psnr.u.min(s.psnr.v));
        bytes += s.encoded.data.len();
        slowest = slowest.max(took);
    }
    // Timestamps go through untouched and strictly increase.
    assert!(
        steps
            .windows(2)
            .all(|w| w[1].encoded.timestamp_us > w[0].encoded.timestamp_us),
        "{what}: timestamps do not increase"
    );
    format!(
        "{what}: {out}, min PSNR Y {worst_y:.1} dB UV {worst_uv:.1} dB, {} KiB/frame, slowest frame {} ms",
        bytes / steps.len() / 1024,
        slowest.as_millis()
    )
}

/// Shapes the loopback matrices run: 16:9, 16:10, 21:9, an odd window and a
/// portrait window.
pub const MATRIX_SOURCES: [(u32, u32); 5] = [
    (1920, 1080),
    (2560, 1600),
    (3440, 1440),
    (1001, 777),
    (900, 2000),
];

/// Deterministic xorshift generator: failures reproduce.
pub struct Rng(pub u64);

impl Rng {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
