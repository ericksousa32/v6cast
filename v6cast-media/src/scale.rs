//! Resolution presets and the proportional scaler.
//!
//! Every preset is a bounding box. The source is scaled by
//! `s = min(1, Wmax / Wsrc, Hmax / Hsrc)`: never up, never stretched, never
//! cropped or letterboxed. Width and height are then rounded **down** to the
//! encoder alignment, so the result never exceeds the box or the source.
//!
//! | preset  | box          |
//! |---------|--------------|
//! | 720p    | 1280 x 720   |
//! | 1080p   | 1920 x 1080  |
//! | 1440p   | 2560 x 1440  |
//! | Native  | 2560 x 1440 (the 2.5K ceiling) |
//!
//! Native and 1440p share the 2.5K ceiling: Native keeps the captured size and
//! only shrinks sources larger than 2560 x 1440. Examples: 2560x1600 (16:10) at
//! 1440p gives 2304x1440; 2560x1080 (21:9) at 1080p gives 1920x810.
//!
//! The math uses integers (no floating point), so results are exact and
//! reproducible on every platform.

use std::time::Duration;

use thiserror::Error;
pub use v6cast_protocol::{FrameRate, Preset};

/// Width and height in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Number of pixels.
    pub fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// The 2.5K ceiling shared by every preset.
pub const MAX_RESOLUTION: Resolution = Resolution::new(2560, 1440);

/// Bounding box of a preset.
pub fn preset_box(preset: Preset) -> Resolution {
    match preset {
        Preset::P720 => Resolution::new(1280, 720),
        Preset::P1080 => Resolution::new(1920, 1080),
        Preset::P1440 | Preset::Native => MAX_RESOLUTION,
    }
}

/// Time between two frames at `rate`.
pub fn frame_interval(rate: FrameRate) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(rate.fps()))
}

/// Multiple that encoded width and height must be rounded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Alignment {
    /// Even sizes: what 4:2:0 chroma needs. Default.
    #[default]
    Two,
    /// Whole 16x16 macroblocks, required by some hardware encoders.
    Sixteen,
}

impl Alignment {
    pub fn get(self) -> u32 {
        match self {
            Alignment::Two => 2,
            Alignment::Sixteen => 16,
        }
    }

    fn down(self, v: u32) -> u32 {
        v - v % self.get()
    }
}

/// Smallest encoded width and height: one H.264 macroblock. openh264 refuses
/// smaller sides with an opaque error, so the scaler stops here.
pub const MIN_ENCODED_SIDE: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ScaleError {
    #[error("source size {0} has a zero dimension")]
    EmptySource(Resolution),
    #[error("scaled size {scaled} has a side under {min} pixels, the H.264 encoder minimum")]
    TooSmall { scaled: Resolution, min: u32 },
}

/// Scales `source` into `bounds`, keeping the aspect ratio. See the module docs.
pub fn fit(
    source: Resolution,
    bounds: Resolution,
    alignment: Alignment,
) -> Result<Resolution, ScaleError> {
    if source.width == 0 || source.height == 0 {
        return Err(ScaleError::EmptySource(source));
    }
    let (sw, sh) = (u64::from(source.width), u64::from(source.height));
    let (bw, bh) = (u64::from(bounds.width), u64::from(bounds.height));
    // Compare Wmax/Wsrc with Hmax/Hsrc by cross-multiplying.
    let (w, h) = if sw <= bw && sh <= bh {
        (sw, sh) // s = 1: never upscale
    } else if bw * sh <= bh * sw {
        (bw, sh * bw / sw) // width is the limit
    } else {
        (sw * bh / sh, bh) // height is the limit
    };
    // w <= source and h <= source, so both fit in u32.
    let scaled = Resolution::new(alignment.down(w as u32), alignment.down(h as u32));
    if scaled.width < MIN_ENCODED_SIDE || scaled.height < MIN_ENCODED_SIDE {
        return Err(ScaleError::TooSmall {
            scaled,
            min: MIN_ENCODED_SIDE,
        });
    }
    Ok(scaled)
}

/// Scales `source` for `preset`.
pub fn fit_preset(
    source: Resolution,
    preset: Preset,
    alignment: Alignment,
) -> Result<Resolution, ScaleError> {
    fit(source, preset_box(preset), alignment)
}

/// A preset with its frame rate, as chosen by the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoProfile {
    pub preset: Preset,
    pub frame_rate: FrameRate,
}

impl VideoProfile {
    pub fn new(preset: Preset, frame_rate: FrameRate) -> Self {
        Self { preset, frame_rate }
    }

    pub fn frame_interval(&self) -> Duration {
        frame_interval(self.frame_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(w: u32, h: u32) -> Resolution {
        Resolution::new(w, h)
    }

    fn at(preset: Preset, w: u32, h: u32) -> Resolution {
        fit_preset(r(w, h), preset, Alignment::Two).unwrap()
    }

    /// Aspect ratio error introduced by rounding, relative.
    fn aspect_error(src: Resolution, out: Resolution) -> f64 {
        let a = f64::from(src.width) / f64::from(src.height);
        let b = f64::from(out.width) / f64::from(out.height);
        (a - b).abs() / a
    }

    #[test]
    fn sixteen_by_nine_monitors() {
        assert_eq!(at(Preset::P720, 1920, 1080), r(1280, 720));
        assert_eq!(at(Preset::P1080, 1920, 1080), r(1920, 1080));
        assert_eq!(at(Preset::P1440, 2560, 1440), r(2560, 1440));
        assert_eq!(at(Preset::P1080, 3840, 2160), r(1920, 1080));
        assert_eq!(at(Preset::Native, 3840, 2160), r(2560, 1440));
    }

    #[test]
    fn sixteen_by_ten_is_height_limited() {
        assert_eq!(at(Preset::P1440, 2560, 1600), r(2304, 1440));
        assert_eq!(at(Preset::P1080, 1920, 1200), r(1728, 1080));
        assert_eq!(at(Preset::P720, 2560, 1600), r(1152, 720));
    }

    #[test]
    fn ultrawide_is_width_limited() {
        assert_eq!(at(Preset::P1440, 2560, 1080), r(2560, 1080));
        assert_eq!(at(Preset::P1080, 2560, 1080), r(1920, 810));
        assert_eq!(at(Preset::P720, 3440, 1440), r(1280, 534));
        assert_eq!(at(Preset::Native, 5120, 1440), r(2560, 720));
    }

    #[test]
    fn never_upscales() {
        assert_eq!(at(Preset::P1440, 1280, 720), r(1280, 720));
        assert_eq!(at(Preset::Native, 800, 600), r(800, 600));
        assert_eq!(at(Preset::P1080, 640, 1000), r(640, 1000).min_fit(1080));
    }

    #[test]
    fn windows_of_any_shape() {
        // Tall window wider than nothing, taller than the box.
        assert_eq!(at(Preset::P1080, 900, 2000), r(486, 1080));
        // Arbitrary window, odd sizes.
        assert_eq!(at(Preset::P1440, 1001, 777), r(1000, 776));
        assert_eq!(at(Preset::P720, 1337, 1001), r(960, 720));
    }

    #[test]
    fn rounding_follows_the_alignment() {
        let src = r(1365, 767);
        assert_eq!(
            fit_preset(src, Preset::P1080, Alignment::Two).unwrap(),
            r(1364, 766)
        );
        assert_eq!(
            fit_preset(src, Preset::P1080, Alignment::Sixteen).unwrap(),
            r(1360, 752)
        );
        let out = fit_preset(r(2560, 1600), Preset::P1440, Alignment::Sixteen).unwrap();
        assert_eq!(out, r(2304, 1440));
        assert_eq!(out.width % 16 + out.height % 16, 0);
    }

    #[test]
    fn result_stays_within_box_source_and_aspect() {
        let presets = [Preset::P720, Preset::P1080, Preset::P1440, Preset::Native];
        for preset in presets {
            let b = preset_box(preset);
            for w in (16..4000).step_by(97) {
                for h in (16..3000).step_by(89) {
                    for align in [Alignment::Two, Alignment::Sixteen] {
                        let src = r(w, h);
                        let Ok(out) = fit_preset(src, preset, align) else {
                            continue;
                        };
                        assert!(out.width <= b.width && out.height <= b.height);
                        assert!(out.width <= w && out.height <= h);
                        assert_eq!(out.width % align.get(), 0);
                        assert_eq!(out.height % align.get(), 0);
                        // At least one side touches the limit (box or source) within one block.
                        let a = align.get();
                        let lw = w.min(b.width);
                        let lh = h.min(b.height);
                        assert!(out.width + a > lw || out.height + a > lh, "{src} -> {out}");
                        // Rounding distorts by at most a block per side.
                        let max_err = f64::from(a) / f64::from(out.width.min(out.height));
                        assert!(aspect_error(src, out) <= 2.0 * max_err, "{src} -> {out}");
                    }
                }
            }
        }
    }

    #[test]
    fn zero_and_tiny_sizes_are_errors_not_panics() {
        assert_eq!(
            fit_preset(r(0, 1080), Preset::P1080, Alignment::Two),
            Err(ScaleError::EmptySource(r(0, 1080)))
        );
        assert!(fit_preset(r(1920, 0), Preset::P1080, Alignment::Two).is_err());
        assert!(fit_preset(r(1, 1), Preset::P720, Alignment::Two).is_err());
        assert!(fit_preset(r(15, 15), Preset::P720, Alignment::Sixteen).is_err());
        // Sides under one macroblock get a clear error naming the minimum.
        assert_eq!(
            fit_preset(r(2, 2), Preset::P720, Alignment::Two),
            Err(ScaleError::TooSmall {
                scaled: r(2, 2),
                min: MIN_ENCODED_SIDE
            })
        );
        assert!(fit_preset(r(640, 14), Preset::P720, Alignment::Two).is_err());
        assert_eq!(at(Preset::P720, 16, 16), r(16, 16));
        // A thin strip shrinks below 16 rows once it is scaled into the box.
        assert!(fit_preset(r(10_000, 100), Preset::P720, Alignment::Two).is_err());
        // A 1-pixel-high strip collapses to zero height after scaling.
        assert!(fit_preset(r(100_000, 1), Preset::P720, Alignment::Two).is_err());
        // Extreme sizes do not overflow.
        assert_eq!(at(Preset::P720, u32::MAX, u32::MAX), r(720, 720));
    }

    #[test]
    fn frame_intervals() {
        assert_eq!(
            frame_interval(FrameRate::Fps30),
            Duration::from_nanos(33_333_333)
        );
        assert_eq!(
            frame_interval(FrameRate::Fps60),
            Duration::from_nanos(16_666_666)
        );
        let p = VideoProfile::new(Preset::P1080, FrameRate::Fps60);
        assert_eq!(p.frame_interval(), frame_interval(FrameRate::Fps60));
    }

    impl Resolution {
        /// Test helper: the expected size when only the height bound applies.
        fn min_fit(self, max_h: u32) -> Resolution {
            if self.height <= max_h {
                return self;
            }
            let w = u64::from(self.width) * u64::from(max_h) / u64::from(self.height);
            Resolution::new(w as u32 - w as u32 % 2, max_h)
        }
    }
}
