//! Local loopback of the whole media path on the software backend (runs in CI):
//! synthetic screen -> capture scaler -> encode -> decode.
//!
//! The VA-API variants of the same matrix are in `loopback_vaapi.rs`.

mod common;

use std::time::{Duration, Instant};

use common::*;
use v6cast_media::capture::FrameClock;
use v6cast_media::{FrameRate, Preset};

const FRAMES: u32 = 4;

#[test]
fn every_preset_and_rate_on_every_screen_shape() {
    let limits = Limits::for_tests();
    for (w, h) in MATRIX_SOURCES {
        for preset in PRESETS {
            for rate in RATES {
                let line = check_stream(
                    Which::Software,
                    Which::Software,
                    r(w, h),
                    preset,
                    rate,
                    FRAMES,
                    limits,
                );
                say(&line);
            }
        }
    }
}

#[test]
fn native_1440p_at_60_fps() {
    // The heaviest case: 2560x1440 at 60 fps, every frame decoded.
    let line = check_stream(
        Which::Software,
        Which::Software,
        r(2560, 1440),
        Preset::Native,
        FrameRate::Fps60,
        30,
        Limits::for_tests(),
    );
    say(&line);
}

#[test]
fn a_4k_monitor_is_delivered_at_2_5k() {
    let line = check_stream(
        Which::Software,
        Which::Software,
        r(3840, 2160),
        Preset::Native,
        FrameRate::Fps30,
        3,
        Limits::for_tests(),
    );
    assert!(line.contains("2560x1440"), "{line}");
    say(&line);
}

#[test]
fn nothing_is_dropped_over_a_sustained_run() {
    // 10 seconds of 30 fps at a small size: one access unit and one picture per
    // frame, no gap in the timestamps, and the quality does not decay.
    let (src, out) = (r(640, 360), r(640, 360));
    let mut enc = encoder_for(Which::Software, out, Preset::P720, FrameRate::Fps30);
    let mut dec = decoder_for(Which::Software);
    let n = 300;
    let steps = run_loopback(
        &mut enc,
        &mut dec,
        src,
        out,
        FrameRate::Fps30,
        n,
        "sustained",
    );
    assert_eq!(steps.len(), n as usize);
    let first: f64 = steps[..30].iter().map(|s| s.psnr.y).sum::<f64>() / 30.0;
    let last: f64 = steps[steps.len() - 30..]
        .iter()
        .map(|s| s.psnr.y)
        .sum::<f64>()
        / 30.0;
    assert!(
        last > first - 3.0,
        "quality decays: {first:.1} dB -> {last:.1} dB"
    );
    let keyframes: Vec<u32> = steps
        .iter()
        .filter(|s| s.encoded.keyframe)
        .map(|s| s.t)
        .collect();
    assert_eq!(keyframes, vec![0, 60, 120, 180, 240], "2 s at 30 fps");
    for s in &steps {
        assert_well_formed(&s.encoded, &format!("frame {}", s.t));
    }
}

#[test]
fn per_frame_latency_stays_under_the_frame_interval_budget_for_small_streams() {
    // 720p60 on the software path: encode + decode must fit in the frame time
    // of a 30 fps stream (33 ms) in a release build. Debug builds only check
    // for a sane bound.
    let (src, out) = (r(1280, 720), r(1280, 720));
    let mut enc = encoder_for(Which::Software, out, Preset::P720, FrameRate::Fps60);
    let mut dec = decoder_for(Which::Software);
    let steps = run_loopback(
        &mut enc,
        &mut dec,
        src,
        out,
        FrameRate::Fps60,
        40,
        "latency",
    );
    let mut times: Vec<Duration> = steps[1..]
        .iter()
        .map(|s| s.encode_time + s.decode_time)
        .collect();
    times.sort();
    let median = times[times.len() / 2];
    let worst = *times.last().unwrap();
    say(&format!(
        "software 720p60 encode+decode: median {median:?}, worst {worst:?}"
    ));
    if cfg!(debug_assertions) {
        assert!(worst < Duration::from_secs(2), "worst frame {worst:?}");
    } else {
        assert!(median < Duration::from_millis(33), "median {median:?}");
        assert!(worst < Duration::from_millis(100), "worst {worst:?}");
    }
}

// ---------------------------------------------------------------------------
// Capture timestamps
// ---------------------------------------------------------------------------

#[test]
fn capture_timestamps_follow_the_clock_and_never_repeat() {
    let t0 = Instant::now();
    let mut clock = FrameClock::default();
    // 60 fps pacing: the stamps are the elapsed microseconds.
    let stamps: Vec<u64> = (0..120)
        .map(|i| clock.stamp(t0 + Duration::from_micros(i * 16_667)))
        .collect();
    assert_eq!(stamps[0], 0, "the first frame is time zero");
    assert_eq!(stamps[60], 60 * 16_667);
    assert!(stamps.windows(2).all(|w| w[1] > w[0]));
}

#[test]
fn capture_timestamps_survive_equal_and_backwards_instants() {
    let t0 = Instant::now();
    let mut clock = FrameClock::default();
    let first = clock.stamp(t0 + Duration::from_millis(5));
    assert_eq!(first, 0);
    // Same instant, then an earlier one (a callback that ran late): still up.
    let mut last = first;
    for now in [
        t0 + Duration::from_millis(5),
        t0 + Duration::from_millis(5),
        t0,
        t0 + Duration::from_millis(5),
        t0 + Duration::from_millis(6),
        t0 + Duration::from_millis(6),
    ] {
        let stamp = clock.stamp(now);
        assert!(stamp > last, "{stamp} after {last}");
        last = stamp;
    }
    // After the burst the clock catches up with real time again.
    let later = clock.stamp(t0 + Duration::from_millis(50));
    assert_eq!(later, 45_000);
}
