//! The loopback matrix on VA-API hardware, in every pairing of the two
//! backends. Runs with `--features v6cast-media/vaapi` in the dev container
//! (`dev/run.sh`, which passes /dev/dri through). Without a VA-API device each
//! test prints `SKIPPED (VA-API): ...` and passes; with `V6CAST_REQUIRE_VAAPI=1`
//! a missing device fails instead.

#![cfg(all(target_os = "linux", feature = "vaapi"))]

mod common;

use common::*;
use v6cast_media::{FrameRate, Preset};

const PAIRS: [(Which, Which); 3] = [
    (Which::Vaapi, Which::Vaapi),
    (Which::Vaapi, Which::Software), // what a viewer without a GPU sees
    (Which::Software, Which::Vaapi), // a GPU viewer of a software sender
];

#[test]
fn every_preset_and_rate_on_every_screen_shape() {
    if !vaapi_available() {
        return;
    }
    let limits = Limits::for_tests();
    for (enc, dec) in PAIRS {
        for (w, h) in MATRIX_SOURCES {
            for preset in PRESETS {
                for rate in RATES {
                    say(&check_stream(enc, dec, r(w, h), preset, rate, 4, limits));
                }
            }
        }
    }
}

#[test]
fn native_1440p_at_60_fps() {
    if !vaapi_available() {
        return;
    }
    for (enc, dec) in PAIRS {
        let mut limits = Limits::for_tests();
        if enc == Which::Vaapi && dec == Which::Vaapi && !cfg!(debug_assertions) {
            // 12 ms encode + 9 ms decode measured on the RX 9060 XT.
            limits.max_frame_time = std::time::Duration::from_millis(60);
        }
        say(&check_stream(
            enc,
            dec,
            r(2560, 1440),
            Preset::Native,
            FrameRate::Fps60,
            30,
            limits,
        ));
    }
}

#[test]
fn a_4k_monitor_is_delivered_at_2_5k() {
    if !vaapi_available() {
        return;
    }
    for (enc, dec) in PAIRS {
        say(&check_stream(
            enc,
            dec,
            r(3840, 2160),
            Preset::Native,
            FrameRate::Fps60,
            3,
            Limits::for_tests(),
        ));
    }
}
