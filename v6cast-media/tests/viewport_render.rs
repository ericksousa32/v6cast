//! The last stage of the loopback: where the decoded picture lands in a
//! window. The viewport math runs everywhere; the GPU part draws the decoded
//! frame with the real `Renderer` into an offscreen window and checks the
//! pixels, and is skipped (loudly) when there is no GPU adapter. Set
//! `V6CAST_REQUIRE_GPU=1` to make a missing adapter a failure.

mod common;

use common::*;
use v6cast_media::{fit_preset, fit_viewport, Preset, Renderer, Resolution};

const WINDOWS: [(u32, u32); 10] = [
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (1280, 720),
    (1024, 768),
    (800, 800),
    (640, 1000),
    (3440, 1440),
    (333, 777),
    (1, 1),
];

#[test]
fn every_decoded_size_fits_every_window_without_distortion() {
    for (sw, sh) in MATRIX_SOURCES {
        for preset in PRESETS {
            let source = r(sw, sh);
            let frame = fit_preset(source, preset, alignment()).unwrap();
            for (ww, wh) in WINDOWS {
                let window = r(ww, wh);
                let v = fit_viewport(frame, window).unwrap();
                let what = format!("{source} -> {frame} in {window}: {v:?}");
                // Inside the window, touching it on at least one axis.
                assert!(
                    v.x + v.width <= ww && v.y + v.height <= wh,
                    "{what}: outside"
                );
                assert!(
                    v.width == ww || v.height == wh,
                    "{what}: not as large as possible"
                );
                // Centered.
                assert!(
                    v.x.abs_diff(ww - v.x - v.width) <= 1 && v.y.abs_diff(wh - v.y - v.height) <= 1,
                    "{what}: not centered"
                );
                // The aspect of what is drawn is the aspect of what was captured,
                // up to the encoder's rounding and the viewport's pixel rounding
                // (meaningless for a window a few pixels wide).
                if v.width.min(v.height) < 16 {
                    continue;
                }
                let (a, b) = (
                    f64::from(sw) / f64::from(sh),
                    f64::from(v.width) / f64::from(v.height),
                );
                let slack = 2.0 / f64::from(frame.width.min(frame.height))
                    + 1.0 / f64::from(v.width.min(v.height));
                assert!(
                    (a - b).abs() / a <= slack + 1e-9,
                    "{what}: drawn at {b:.4}, captured at {a:.4} (slack {slack:.4})"
                );
            }
        }
    }
}

#[test]
fn a_window_with_the_frames_aspect_is_filled_exactly() {
    for (fw, fh) in [
        (1280u32, 720u32),
        (1920, 1080),
        (2304, 1440),
        (486, 1080),
        (926, 720),
    ] {
        for k in [1u32, 2, 3] {
            let window = r(fw / 2 * k, fh / 2 * k);
            let v = fit_viewport(r(fw, fh), window).unwrap();
            assert_eq!((v.x, v.y), (0, 0), "{fw}x{fh} in {window}");
            assert_eq!(
                (v.width, v.height),
                (window.width, window.height),
                "{fw}x{fh} in {window}"
            );
        }
    }
}

#[test]
fn degenerate_sizes_give_none_not_a_panic() {
    assert_eq!(fit_viewport(r(0, 0), r(10, 10)), None);
    assert_eq!(fit_viewport(r(10, 10), r(0, 5)), None);
    assert!(fit_viewport(r(u32::MAX, 1), r(u32::MAX, u32::MAX)).is_some());
}

// ---------------------------------------------------------------------------
// The renderer
// ---------------------------------------------------------------------------

fn gpu() -> Option<Renderer> {
    match Renderer::headless(wgpu_format()) {
        Ok(renderer) => {
            if let Some(info) = renderer.adapter_info() {
                say(&format!(
                    "renderer adapter: {} ({:?})",
                    info.name, info.backend
                ));
            }
            Some(renderer)
        }
        Err(e) => {
            assert!(
                !env_flag("V6CAST_REQUIRE_GPU"),
                "V6CAST_REQUIRE_GPU is set but there is no GPU adapter: {e}"
            );
            say(&format!("SKIPPED (renderer): no GPU adapter: {e}"));
            None
        }
    }
}

fn wgpu_format() -> wgpu::TextureFormat {
    wgpu::TextureFormat::Rgba8Unorm
}

fn rgba(img: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
}

/// capture -> scale -> encode -> decode -> upload -> draw into `window`.
fn through_the_whole_pipe(
    renderer: &mut Renderer,
    source: Resolution,
    preset: Preset,
    window: Resolution,
) {
    let frame = fit_preset(source, preset, alignment()).unwrap();
    let what = format!("{source} at {preset:?} in a {window} window");
    let mut enc = encoder_for(
        Which::Software,
        frame,
        preset,
        v6cast_media::FrameRate::Fps30,
    );
    let mut dec = decoder_for(Which::Software);
    let steps = run_loopback(
        &mut enc,
        &mut dec,
        source,
        frame,
        v6cast_media::FrameRate::Fps30,
        2,
        &what,
    );
    let decoded = &steps.last().unwrap().decoded;

    renderer.upload(decoded);
    let img = renderer.render_to_rgba(window).expect("render");
    assert_eq!(img.len(), (window.width * window.height * 4) as usize);
    let v = fit_viewport(frame, window).unwrap();

    // 1. The bars are black. A 2 px guard band around the picture is not
    //    checked (the texture is filtered at its edge).
    for y in 0..window.height {
        for x in 0..window.width {
            let inside =
                x + 2 > v.x && x < v.x + v.width + 2 && y + 2 > v.y && y < v.y + v.height + 2;
            if !inside {
                let p = rgba(&img, window.width, x, y);
                assert_eq!(p, [0, 0, 0, 255], "{what}: ({x},{y}) is not black");
            }
        }
    }

    // 2. The picture is there: the corners of the viewport are not black.
    let nonblack = |x: u32, y: u32| rgba(&img, window.width, x, y)[..3].iter().any(|c| *c > 20);
    assert!(
        nonblack(v.x + 2, v.y + 2) || nonblack(v.x + v.width - 3, v.y + 2),
        "{what}: empty picture"
    );

    // 3. The white marker square is still a square, scaled by the viewport
    //    size over the *source* size, and sits in the middle of the window.
    let side = f64::from(marker_side(source));
    let (kx, ky) = (
        f64::from(v.width) / f64::from(source.width),
        f64::from(v.height) / f64::from(source.height),
    );
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..window.height {
        for x in 0..window.width {
            if rgba(&img, window.width, x, y)[..3]
                .iter()
                .all(|c| *c >= 200)
            {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
    }
    assert!(x0 != u32::MAX, "{what}: the marker is not on screen");
    let (bw, bh) = (f64::from(x1 - x0 + 1), f64::from(y1 - y0 + 1));
    // Coding blur, chroma and the viewport rounding cost a few pixels at most.
    let tol = 4.0 + 2.0 * kx.max(ky);
    assert!(
        (bw - side * kx).abs() <= tol && (bh - side * ky).abs() <= tol,
        "{what}: marker {bw}x{bh}, expected {:.1}x{:.1}",
        side * kx,
        side * ky
    );
    assert!(
        (bw - bh).abs() <= tol * 1.5,
        "{what}: marker {bw}x{bh} is not square: stretched on screen"
    );
    let (cx, cy) = (
        f64::from(x0 + x1 + 1) / 2.0 - f64::from(window.width) / 2.0,
        f64::from(y0 + y1 + 1) / 2.0 - f64::from(window.height) / 2.0,
    );
    assert!(
        cx.abs() <= tol && cy.abs() <= tol,
        "{what}: marker off-center by ({cx:.1}, {cy:.1})"
    );
}

#[test]
fn the_decoded_picture_is_drawn_centered_undistorted_and_on_black() {
    let Some(mut renderer) = gpu() else { return };
    for (sw, sh) in [
        (1920, 1080),
        (2560, 1600),
        (3440, 1440),
        (900, 2000),
        (1001, 777),
    ] {
        for (ww, wh) in [
            (1280, 720),
            (800, 800),
            (1024, 768),
            (640, 1000),
            (1600, 600),
            (333, 777),
        ] {
            through_the_whole_pipe(&mut renderer, r(sw, sh), Preset::P720, r(ww, wh));
        }
    }
}

#[test]
fn switching_stream_size_redraws_with_the_new_aspect() {
    // The sender changes preset (and a window is resized): the textures are
    // rebuilt and the new picture is centered with its own aspect.
    let Some(mut renderer) = gpu() else { return };
    let window = r(1000, 700);
    for (source, preset) in [
        (r(1920, 1080), Preset::P720),
        (r(900, 2000), Preset::P720),
        (r(3440, 1440), Preset::P720),
        (r(1920, 1080), Preset::P1080),
    ] {
        through_the_whole_pipe(&mut renderer, source, preset, window);
    }
}
