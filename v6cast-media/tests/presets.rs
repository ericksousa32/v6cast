//! Presets, the 2.5K cap, the aspect fit and the pixel scaler, checked from
//! the outside with the public API.

mod common;

use common::*;
use v6cast_media::capture::{to_i420, OutputSize, PixelLayout, RawImage};
use v6cast_media::scale::{preset_box, MAX_RESOLUTION};
use v6cast_media::{fit_preset, Alignment, Preset, Resolution};

/// Window and monitor shapes seen in practice: 16:9, 16:10, 21:9, 32:9, 4:3,
/// 5:4, portrait, and sizes that are odd or not a multiple of anything.
const SOURCES: [(u32, u32); 26] = [
    (1280, 720),
    (1366, 768),
    (1600, 900),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (7680, 4320),
    (1280, 800),
    (1920, 1200),
    (2560, 1600),
    (2880, 1800),
    (2560, 1080),
    (3440, 1440),
    (3840, 1600),
    (5120, 1440),
    (1024, 768),
    (1280, 1024),
    (1080, 1920),
    (900, 2000),
    (800, 600),
    (1001, 777),
    (1337, 1001),
    (2561, 1441),
    (4001, 3001),
    (333, 777),
    (4096, 4096),
];

fn at(preset: Preset, w: u32, h: u32) -> Resolution {
    fit_preset(r(w, h), preset, Alignment::Two).unwrap()
}

#[test]
fn documented_sizes_for_the_common_shapes() {
    // (source, preset, expected). Worked out by hand from "scale by
    // min(1, Wmax/W, Hmax/H), then round down to even".
    let table = [
        ((1920, 1080), Preset::P720, (1280, 720)),
        ((1920, 1080), Preset::P1080, (1920, 1080)),
        ((1920, 1080), Preset::P1440, (1920, 1080)), // never upscaled
        ((1920, 1080), Preset::Native, (1920, 1080)),
        ((2560, 1440), Preset::P720, (1280, 720)),
        ((2560, 1440), Preset::P1080, (1920, 1080)),
        ((2560, 1440), Preset::P1440, (2560, 1440)),
        ((3840, 2160), Preset::P1440, (2560, 1440)),
        ((3840, 2160), Preset::Native, (2560, 1440)), // the 2.5K cap
        ((7680, 4320), Preset::Native, (2560, 1440)),
        ((2560, 1600), Preset::P1440, (2304, 1440)), // 16:10 is height-limited
        ((2560, 1600), Preset::P1080, (1728, 1080)),
        ((2560, 1600), Preset::P720, (1152, 720)),
        ((2560, 1600), Preset::Native, (2304, 1440)),
        ((2560, 1080), Preset::P1080, (1920, 810)), // 21:9 is width-limited
        ((2560, 1080), Preset::P720, (1280, 540)),
        ((3440, 1440), Preset::P1080, (1920, 803 & !1)),
        ((3840, 1600), Preset::P1440, (2560, 1066)),
        ((5120, 1440), Preset::P1080, (1920, 540)),
        ((1366, 768), Preset::P720, (1280, 718)),
        ((1080, 1920), Preset::P1080, (606, 1080)), // portrait window
        ((1080, 1920), Preset::P720, (404, 720)),
        ((2561, 1441), Preset::Native, (2558, 1440)),
        ((1001, 777), Preset::P1440, (1000, 776)), // odd window, only rounded
    ];
    for ((w, h), preset, (ow, oh)) in table {
        assert_eq!(at(preset, w, h), r(ow, oh), "{w}x{h} at {preset:?}");
    }
}

#[test]
fn every_shape_and_preset_stays_in_the_box_even_and_undistorted() {
    for (w, h) in SOURCES {
        for preset in PRESETS {
            for align in [Alignment::Two, Alignment::Sixteen] {
                let src = r(w, h);
                let b = preset_box(preset);
                let out = fit_preset(src, preset, align).unwrap();
                let what = format!("{src} at {preset:?} ({align:?}) -> {out}");
                let a = align.get();
                assert!(
                    out.width <= b.width && out.height <= b.height,
                    "{what}: over the box"
                );
                assert!(out.width <= w && out.height <= h, "{what}: upscaled");
                assert_eq!((out.width % a, out.height % a), (0, 0), "{what}: alignment");
                // Aspect ratio: cross-multiplied error is at most one block per side.
                let lhs = u64::from(out.width) * u64::from(h);
                let rhs = u64::from(out.height) * u64::from(w);
                let slack = u64::from(a) * u64::from(w.max(h)) * 2;
                assert!(
                    lhs.abs_diff(rhs) <= slack,
                    "{what}: stretched ({lhs} vs {rhs})"
                );
                // Maximal: it only stops growing at the box or at the source.
                assert!(
                    out.width + a > w.min(b.width) || out.height + a > h.min(b.height),
                    "{what}: smaller than it could be"
                );
            }
        }
    }
}

#[test]
fn the_2_5k_cap_holds_for_every_preset_and_native_equals_1440p() {
    assert_eq!(MAX_RESOLUTION, r(2560, 1440));
    assert_eq!(preset_box(Preset::Native), r(2560, 1440));
    assert_eq!(preset_box(Preset::P1440), r(2560, 1440));
    for (w, h) in SOURCES {
        for preset in PRESETS {
            let out = at(preset, w, h);
            assert!(
                out.width <= 2560 && out.height <= 1440,
                "{w}x{h} at {preset:?} gave {out}: above 2.5K"
            );
        }
        assert_eq!(at(Preset::Native, w, h), at(Preset::P1440, w, h), "{w}x{h}");
        // A source inside the cap keeps its size (rounded down to even).
        if w <= 2560 && h <= 1440 {
            assert_eq!(
                at(Preset::Native, w, h),
                r(w & !1, h & !1),
                "{w}x{h} native"
            );
        }
    }
    // Presets only ever get smaller as the preset goes down.
    for (w, h) in SOURCES {
        let areas: Vec<u64> = PRESETS.iter().map(|p| at(*p, w, h).area()).collect();
        assert!(
            areas[0] <= areas[1] && areas[1] <= areas[2],
            "{w}x{h}: {areas:?}"
        );
    }
    // Just over the cap on one side: scaled down, not clipped.
    assert_eq!(at(Preset::Native, 2562, 1440), r(2560, 1438));
    assert_eq!(at(Preset::Native, 2560, 1442), r(2556, 1440));
}

#[test]
fn degenerate_sizes_are_errors_not_panics() {
    assert!(fit_preset(r(0, 0), Preset::P720, Alignment::Two).is_err());
    assert!(fit_preset(r(1, 1), Preset::Native, Alignment::Two).is_err());
    assert!(fit_preset(r(10, 10), Preset::Native, Alignment::Sixteen).is_err());
    assert!(fit_preset(r(100_000, 1), Preset::P1080, Alignment::Two).is_err());
    assert!(fit_preset(r(u32::MAX, u32::MAX), Preset::Native, Alignment::Two).is_ok());
}

#[test]
fn output_size_matches_the_preset_math() {
    for (w, h) in SOURCES {
        for preset in PRESETS {
            let by_request = OutputSize::Preset(preset, Alignment::Two)
                .for_source(r(w, h))
                .unwrap();
            assert_eq!(by_request, at(preset, w, h));
        }
        assert_eq!(
            OutputSize::Native(Alignment::Two)
                .for_source(r(w, h))
                .unwrap(),
            at(Preset::Native, w, h)
        );
    }
}

// ---------------------------------------------------------------------------
// The pixel scaler (BGRx/RGBx buffer -> scaled I420)
// ---------------------------------------------------------------------------

fn convert(
    data: &[u8],
    stride: usize,
    src: Resolution,
    layout: PixelLayout,
    out: Resolution,
) -> v6cast_media::YuvFrame {
    to_i420(
        &RawImage {
            data,
            stride,
            size: src,
            layout,
        },
        out,
    )
    .unwrap()
}

/// BT.601 limited range, in floating point (the scaler uses integers).
fn yuv(rgb: [u8; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(f64::from);
    [
        16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0,
        128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0,
        128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0,
    ]
}

#[test]
fn flat_colors_survive_any_scale_stride_and_byte_order() {
    let colors = [
        [255, 255, 255],
        [0, 0, 0],
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [200, 100, 50],
    ];
    let layouts = [
        (PixelLayout::Bgrx, [2, 1, 0]),
        (PixelLayout::Bgra, [2, 1, 0]),
        (PixelLayout::Rgbx, [0, 1, 2]),
        (PixelLayout::Rgba, [0, 1, 2]),
    ];
    // Small sources keep this fast in debug builds; the scaling logic does not
    // depend on the size.
    for (sw, sh, tw, th) in [
        (640, 360, 426, 240),
        (500, 388, 250, 194),
        (333, 777, 166, 388),
        (1001, 777, 1000, 776),
        (96, 54, 96, 54),
    ] {
        let src = r(sw, sh);
        let out = r(tw, th);
        for rgb in colors {
            let want = yuv(rgb);
            for (layout, order) in layouts {
                let stride = sw as usize * 4 + 36;
                let mut data = vec![0x77u8; stride * sh as usize];
                for y in 0..sh as usize {
                    for x in 0..sw as usize {
                        let i = y * stride + x * 4;
                        for c in 0..3 {
                            data[i + order[c]] = rgb[c];
                        }
                        data[i + 3] = 0xff;
                    }
                }
                let f = convert(&data, stride, src, layout, out);
                let what = format!("{src}->{out} {layout:?} {rgb:?}");
                for (plane, expect, name) in [
                    (f.y(), want[0], "Y"),
                    (f.u(), want[1], "U"),
                    (f.v(), want[2], "V"),
                ] {
                    let worst = plane
                        .iter()
                        .map(|p| (f64::from(*p) - expect).abs())
                        .fold(0.0, f64::max);
                    assert!(
                        worst <= 1.6,
                        "{what}: {name} off by {worst} (want {expect:.1})"
                    );
                }
            }
        }
    }
}

#[test]
fn primary_colors_have_the_exact_integer_bt601_values() {
    // Limited-range BT.601 as the scaler computes it, in integers. A wrong
    // coefficient moves at least one of these by a level.
    let cases = [
        ([255, 255, 255], [235, 128, 128]),
        ([0, 0, 0], [16, 128, 128]),
        ([128, 128, 128], [126, 128, 128]),
        ([255, 0, 0], [82, 90, 240]),
        ([0, 255, 0], [144, 54, 34]),
        ([0, 0, 255], [41, 240, 110]),
        ([255, 255, 0], [210, 16, 146]),
        ([0, 255, 255], [169, 166, 16]),
        ([255, 0, 255], [107, 202, 222]),
    ];
    for (rgb, want) in cases {
        let data = flat_screen(r(8, 8), rgb, 8 * 4);
        let f = convert(&data, 8 * 4, r(8, 8), PixelLayout::Bgrx, r(8, 8));
        assert_eq!([f.y()[0], f.u()[0], f.v()[0]], want, "{rgb:?}");
    }
}

/// Reference box filter written from the documentation: every target pixel is
/// the average of the source pixels in `[t*S/T, (t+1)*S/T)` (at least one).
fn box_average(src: &[u8], sw: usize, sh: usize, tw: usize, th: usize) -> Vec<f64> {
    let span = |t: usize, s: usize, n: usize| {
        let start = t * s / n;
        (start, ((t + 1) * s / n).max(start + 1).min(s))
    };
    let mut out = Vec::new();
    for ty in 0..th {
        let (y0, y1) = span(ty, sh, th);
        for tx in 0..tw {
            let (x0, x1) = span(tx, sw, tw);
            let mut sum = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += u64::from(src[y * sw + x]);
                }
            }
            out.push(sum as f64 / ((y1 - y0) * (x1 - x0)) as f64);
        }
    }
    out
}

#[test]
fn the_scaler_is_a_box_filter_over_every_source_pixel() {
    // Noise makes every misplaced footprint show: moving a footprint by one
    // pixel, dropping a pixel from it or skipping one changes the average by
    // tens of levels.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for (sw, sh, tw, th) in [
        (97usize, 61usize, 48usize, 30usize),
        (640, 360, 426, 240),
        (100, 100, 98, 98),
        (55, 33, 20, 12),
        (333, 777, 166, 388),
        (1001, 777, 1000, 776),
        (64, 64, 64, 64),
        (50, 40, 100, 80), // upscaling repeats pixels
    ] {
        let gray: Vec<u8> = (0..sw * sh).map(|_| rng.next() as u8).collect();
        let stride = sw * 4 + 8;
        let mut data = vec![0u8; stride * sh];
        for y in 0..sh {
            for x in 0..sw {
                let g = gray[y * sw + x];
                data[y * stride + x * 4..][..4].copy_from_slice(&[g, g, g, 255]);
            }
        }
        let f = convert(
            &data,
            stride,
            r(sw as u32, sh as u32),
            PixelLayout::Bgrx,
            r(tw as u32, th as u32),
        );
        let want = box_average(&gray, sw, sh, tw, th);
        for (i, (got, avg)) in f.y().iter().zip(&want).enumerate() {
            let expect = 16.0 + 219.0 * avg / 255.0;
            assert!(
                (f64::from(*got) - expect).abs() <= 1.6,
                "{sw}x{sh}->{tw}x{th} pixel ({}, {}): {got}, box average says {expect:.1}",
                i % tw,
                i / tw
            );
        }
    }
}

#[test]
fn a_horizontal_ramp_scales_without_shifting_or_flipping() {
    // Gray ramp, one value per source column. After scaling, output column
    // tx must hold the ramp's value at the center of the footprint it covers.
    for (sw, sh, tw, th) in [
        (1920u32, 1080u32, 1280u32, 720u32),
        (2560, 1440, 1920, 1080),
        (1001, 777, 1000, 776),
        (1366, 768, 1280, 718),
        (640, 480, 640, 480),
    ] {
        let stride = sw as usize * 4;
        let mut data = vec![0u8; stride * sh as usize];
        for y in 0..sh as usize {
            for x in 0..sw as usize {
                let g = (255 * x / (sw as usize - 1)) as u8;
                data[y * stride + x * 4..][..4].copy_from_slice(&[g, g, g, 255]);
            }
        }
        let f = convert(&data, stride, r(sw, sh), PixelLayout::Bgrx, r(tw, th));
        for ty in [0usize, th as usize / 2, th as usize - 1] {
            for tx in 0..tw as usize {
                let centre = (tx as f64 + 0.5) * f64::from(sw) / f64::from(tw) - 0.5;
                let g = 255.0 * centre / f64::from(sw - 1);
                let expect = 16.0 + 219.0 * g / 255.0;
                let got = f64::from(f.y()[ty * tw as usize + tx]);
                assert!(
                    (got - expect).abs() <= 2.5,
                    "{sw}x{sh}->{tw}x{th} col {tx} row {ty}: {got}, expected {expect:.1}"
                );
            }
        }
    }
}

#[test]
fn a_vertical_ramp_scales_without_shifting_or_flipping() {
    for (sw, sh, tw, th) in [
        (1920u32, 1080u32, 1280u32, 720u32),
        (1001, 777, 1000, 776),
        (1080, 1920, 606, 1080),
    ] {
        let stride = sw as usize * 4;
        let mut data = vec![0u8; stride * sh as usize];
        for y in 0..sh as usize {
            let g = (255 * y / (sh as usize - 1)) as u8;
            for x in 0..sw as usize {
                data[y * stride + x * 4..][..4].copy_from_slice(&[g, g, g, 255]);
            }
        }
        let f = convert(&data, stride, r(sw, sh), PixelLayout::Bgrx, r(tw, th));
        for ty in 0..th as usize {
            let centre = (ty as f64 + 0.5) * f64::from(sh) / f64::from(th) - 0.5;
            let expect = 16.0 + 219.0 * centre / f64::from(sh - 1);
            for tx in [0usize, tw as usize - 1] {
                let got = f64::from(f.y()[ty * tw as usize + tx]);
                assert!(
                    (got - expect).abs() <= 2.5,
                    "{sw}x{sh}->{tw}x{th} row {ty}: {got}, expected {expect:.1}"
                );
            }
        }
    }
}

#[test]
fn chroma_stays_aligned_with_luma() {
    // Left half red, right half blue. The U/V planes must switch at the same
    // place as the luma does (half the width), not a pixel earlier or later.
    let (sw, sh) = (1920u32, 1080u32);
    let stride = sw as usize * 4;
    let mut data = vec![0u8; stride * sh as usize];
    for y in 0..sh as usize {
        for x in 0..sw as usize {
            let rgb = if x < sw as usize / 2 {
                [255, 0, 0]
            } else {
                [0, 0, 255]
            };
            data[y * stride + x * 4..][..4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    for (tw, th) in [(1280u32, 720u32), (1920, 1080), (1000, 562)] {
        let f = convert(&data, stride, r(sw, sh), PixelLayout::Bgrx, r(tw, th));
        let row = th as usize / 2;
        let luma_edge = (0..tw as usize)
            .find(|&x| f.y()[row * tw as usize + x] < 60)
            .unwrap();
        let cw = tw as usize / 2;
        let chroma_edge = (0..cw).find(|&x| f.v()[(row / 2) * cw + x] < 128).unwrap();
        assert!(
            (luma_edge as i64 - 2 * chroma_edge as i64).abs() <= 2,
            "{tw}x{th}: luma edge at {luma_edge}, chroma edge at {}",
            2 * chroma_edge
        );
        assert_eq!(luma_edge, tw as usize / 2, "{tw}x{th}: the edge moved");
    }
}

#[test]
fn the_synthetic_screen_keeps_its_marker_square_through_the_scaler() {
    for (w, h) in [
        (1920, 1080),
        (2560, 1600),
        (3440, 1440),
        (1001, 777),
        (900, 2000),
    ] {
        for preset in [Preset::P720, Preset::P1080, Preset::Native] {
            let src = r(w, h);
            let out = at(preset, w, h);
            let f = capture_frame(src, out, 0);
            assert_marker_is_square(f.y(), out, src, 3.0, &format!("{src} at {preset:?}"));
        }
    }
}

#[test]
fn scaler_rejects_odd_targets_and_short_buffers_without_panicking() {
    let src = r(64, 64);
    let data = vec![0u8; 64 * 64 * 4];
    let raw = RawImage {
        data: &data,
        stride: 64 * 4,
        size: src,
        layout: PixelLayout::Bgrx,
    };
    assert!(to_i420(&raw, r(33, 32)).is_err());
    assert!(to_i420(&raw, r(0, 0)).is_err());
    assert!(to_i420(
        &RawImage {
            data: &data[..100],
            ..raw
        },
        r(32, 32)
    )
    .is_err());
    assert!(to_i420(&RawImage { stride: 10, ..raw }, r(32, 32)).is_err());
    assert!(to_i420(
        &RawImage {
            size: r(0, 64),
            ..raw
        },
        r(32, 32)
    )
    .is_err());
    // Strides may be larger than the row, and the last row needs no padding.
    let tight = 64 * 63 * 4 + 64 * 4;
    assert!(to_i420(
        &RawImage {
            data: &data[..tight],
            ..raw
        },
        r(32, 32)
    )
    .is_ok());
}
