//! Manual capture test for the owner's Wayland session (Linux, `pipewire` feature).
//!
//! Build it in the container and run it on the host:
//!
//! ```text
//! dev/build-manual-tests.sh
//! target/manual/capture_portal                     # a monitor, 5 s, 1080p60
//! target/manual/capture_portal --window --preset 720p --seconds 10
//! ```
//!
//! The portal dialog asks which monitor or window to share. The program then
//! captures, scales to the preset, encodes with the automatic backend choice
//! (VA-API when available, otherwise openh264) and writes into `--out`
//! (default `v6cast-capture/`):
//!
//! * `first-frame.y4m`: the first frame, viewable with `mpv first-frame.y4m`;
//! * `capture.h264`: the whole capture, playable with `mpv capture.h264`.
//!
//! It prints the granted source, frame sizes, measured frame rate, dropped
//! frames and whether timestamps were strictly increasing, and exits with an
//! error when no frame arrived.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use v6cast_media::capture::{
    CaptureError, CaptureRequest, CaptureSource, OutputSize, ScreenCapture,
};
use v6cast_media::codec::{open_encoder, BackendPreference, EncoderConfig, VideoEncoder};
use v6cast_media::{Alignment, FrameRate, Preset, Resolution, VideoProfile, YuvFrame};

struct Args {
    source: CaptureSource,
    preset: Preset,
    rate: FrameRate,
    seconds: u64,
    out: PathBuf,
    cursor: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        source: CaptureSource::Monitor,
        preset: Preset::P1080,
        rate: FrameRate::Fps60,
        seconds: 5,
        out: PathBuf::from("v6cast-capture"),
        cursor: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--window" => args.source = CaptureSource::Window,
            "--monitor" => args.source = CaptureSource::Monitor,
            "--no-cursor" => args.cursor = false,
            "--preset" => {
                args.preset = match value()?.as_str() {
                    "720p" => Preset::P720,
                    "1080p" => Preset::P1080,
                    "1440p" => Preset::P1440,
                    "native" => Preset::Native,
                    other => return Err(format!("unknown preset {other}")),
                }
            }
            "--fps" => {
                args.rate = match value()?.as_str() {
                    "30" => FrameRate::Fps30,
                    "60" => FrameRate::Fps60,
                    other => return Err(format!("fps must be 30 or 60, not {other}")),
                }
            }
            "--seconds" => {
                args.seconds = value()?
                    .parse()
                    .map_err(|_| "--seconds needs a number".to_string())?
            }
            "--out" => args.out = PathBuf::from(value()?),
            "-h" | "--help" => return Err(
                "usage: capture_portal [--monitor|--window] [--preset 720p|1080p|1440p|native] \
                     [--fps 30|60] [--seconds N] [--out DIR] [--no-cursor]"
                    .into(),
            ),
            other => return Err(format!("unknown argument {other} (try --help)")),
        }
    }
    Ok(args)
}

fn write_y4m(path: &PathBuf, frame: &YuvFrame, rate: FrameRate) -> std::io::Result<()> {
    let r = frame.resolution();
    let mut f = File::create(path)?;
    writeln!(
        f,
        "YUV4MPEG2 W{} H{} F{}:1 Ip A1:1 C420jpeg",
        r.width,
        r.height,
        rate.fps()
    )?;
    f.write_all(b"FRAME\n")?;
    f.write_all(frame.as_bytes())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&args.out) {
        eprintln!("cannot create {}: {e}", args.out.display());
        return ExitCode::FAILURE;
    }
    let mut request =
        CaptureRequest::new(args.source, OutputSize::Preset(args.preset, Alignment::Two));
    request.show_cursor = args.cursor;
    // One epoch for the session; audio capture would take the same one.
    request.epoch = Some(v6cast_media::capture::MediaEpoch::now());

    println!("Opening the screen sharing dialog ({:?})...", args.source);
    let capture = match ScreenCapture::start(request) {
        Ok(c) => c,
        Err(CaptureError::Cancelled) => {
            eprintln!("cancelled in the dialog");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("capture failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let info = capture.info().clone();
    println!(
        "granted: {:?}, PipeWire node {}, portal size {:?}",
        info.source, info.node_id, info.size
    );

    let h264_path = args.out.join("capture.h264");
    let mut h264 = match File::create(&h264_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot create {}: {e}", h264_path.display());
            return ExitCode::FAILURE;
        }
    };
    let profile = VideoProfile::new(args.preset, args.rate);
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut size: Option<Resolution> = None;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    let (mut frames, mut keyframes, mut bytes, mut size_changes) = (0u64, 0u64, 0u64, 0u64);
    let (mut last_ts, mut monotonic) = (None::<u64>, true);
    let mut deltas = Vec::new();

    while Instant::now() < deadline {
        let captured = match capture.recv_timeout(Duration::from_secs(2)) {
            Ok(c) => c,
            Err(CaptureError::Timeout) => {
                println!("no frame for 2 s (a static screen sends no frames on some desktops)");
                continue;
            }
            Err(e) => {
                eprintln!("capture stopped: {e}");
                break;
            }
        };
        let r = captured.frame.resolution();
        if size != Some(r) {
            println!("frame size {r} (source {})", captured.source_size);
            if size.is_some() {
                size_changes += 1;
            }
            size = Some(r);
            match open_encoder(&EncoderConfig::new(r, profile), BackendPreference::Auto) {
                Ok(e) => {
                    println!("encoder backend: {}", e.backend());
                    encoder = Some(Box::new(e));
                }
                Err(e) => {
                    eprintln!("no encoder for {r}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if frames == 0 {
            let path = args.out.join("first-frame.y4m");
            if let Err(e) = write_y4m(&path, &captured.frame, args.rate) {
                eprintln!("cannot write {}: {e}", path.display());
            }
        }
        if let Some(prev) = last_ts {
            if captured.timestamp_us <= prev {
                monotonic = false;
            }
            deltas.push(captured.timestamp_us.saturating_sub(prev));
        }
        last_ts = Some(captured.timestamp_us);
        frames += 1;
        if let Some(enc) = encoder.as_mut() {
            match enc.encode(&captured.frame, captured.timestamp_us) {
                Ok(Some(out)) => {
                    keyframes += u64::from(out.keyframe);
                    bytes += out.data.len() as u64;
                    if let Err(e) = h264.write_all(&out.data) {
                        eprintln!("cannot write {}: {e}", h264_path.display());
                        return ExitCode::FAILURE;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("encode failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    let secs = started.elapsed().as_secs_f64();
    println!();
    println!(
        "frames: {frames} in {secs:.1} s ({:.1} fps)",
        frames as f64 / secs
    );
    println!("dropped by the consumer: {}", capture.dropped_frames());
    println!("size changes: {size_changes}");
    if !deltas.is_empty() {
        deltas.sort_unstable();
        let avg = deltas.iter().sum::<u64>() / deltas.len() as u64;
        println!(
            "frame interval (us): min {} median {} max {} avg {avg}",
            deltas[0],
            deltas[deltas.len() / 2],
            deltas[deltas.len() - 1]
        );
    }
    println!(
        "timestamps strictly increasing: {}",
        if monotonic { "yes" } else { "NO" }
    );
    println!(
        "encoded: {bytes} bytes, {keyframes} keyframes, {:.2} Mbit/s",
        bytes as f64 * 8.0 / secs / 1e6
    );
    println!(
        "wrote {} and {}",
        args.out.join("first-frame.y4m").display(),
        h264_path.display()
    );
    if frames == 0 {
        eprintln!("FAILED: no frame captured");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
