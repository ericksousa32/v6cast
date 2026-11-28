//! Screen and window capture.
//!
//! On Linux with the `pipewire` feature, [`ScreenCapture::start`] asks the XDG
//! Desktop Portal (ScreenCast) for a monitor or a window, which shows the
//! desktop's own picker, then reads frames from the PipeWire stream the portal
//! hands over. Frames arrive as [`CapturedFrame`]s: I420, already scaled to the
//! requested [`OutputSize`], with a monotonic timestamp.
//!
//! Everywhere else `start` returns [`CaptureError::Unsupported`]; the pure
//! parts (pixel conversion, scaling, timestamps) are available on every
//! platform. Windows capture is Etapa 2B.

mod convert;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod linux;

use std::time::{Duration, Instant};

use thiserror::Error;

pub use convert::{to_i420, PixelLayout, RawImage};

use crate::frame::{FrameError, YuvFrame};
use crate::scale::{fit_preset, Alignment, Preset, Resolution, ScaleError};

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureSource {
    /// A whole monitor (the "Desktop Inteiro" mode, with system audio).
    Monitor,
    /// A single window (application audio only, when possible).
    Window,
}

/// Size of the delivered frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSize {
    /// Capture size, shrunk to the 2.5K ceiling if larger.
    Native(Alignment),
    /// Scaled into a preset's box, aspect ratio kept.
    Preset(Preset, Alignment),
}

impl OutputSize {
    /// Encoded size for a capture of `source` size.
    pub fn for_source(self, source: Resolution) -> Result<Resolution, ScaleError> {
        match self {
            OutputSize::Native(a) => fit_preset(source, Preset::Native, a),
            OutputSize::Preset(p, a) => fit_preset(source, p, a),
        }
    }
}

/// Parameters of a capture session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRequest {
    pub source: CaptureSource,
    /// Draw the mouse cursor into the frames.
    pub show_cursor: bool,
    pub output: OutputSize,
}

impl CaptureRequest {
    pub fn new(source: CaptureSource, output: OutputSize) -> Self {
        Self {
            source,
            show_cursor: true,
            output,
        }
    }
}

/// What the portal granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInfo {
    /// Source type the user picked (the portal may offer both kinds).
    pub source: CaptureSource,
    /// PipeWire node of the stream.
    pub node_id: u32,
    /// Size reported by the portal, if any. The stream's own format wins.
    pub size: Option<Resolution>,
}

/// One captured frame.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub frame: YuvFrame,
    /// Size of the captured image before scaling.
    pub source_size: Resolution,
    /// Microseconds since the first frame of the session, from a monotonic
    /// clock; strictly increasing.
    pub timestamp_us: u64,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("screen capture is not available in this build or on this platform")]
    Unsupported,
    #[error("the user cancelled the screen sharing dialog")]
    Cancelled,
    #[error("XDG Desktop Portal error: {0}")]
    Portal(String),
    #[error("PipeWire error: {0}")]
    PipeWire(String),
    #[error("bad captured frame: {0}")]
    BadFrame(&'static str),
    #[error("bad frame size: {0}")]
    Frame(#[from] FrameError),
    #[error("cannot scale the captured frame: {0}")]
    Scale(#[from] ScaleError),
    #[error("no frame within the timeout")]
    Timeout,
    #[error("the capture stream ended")]
    Closed,
}

/// Monotonic, strictly increasing frame timestamps in microseconds.
#[derive(Debug, Clone, Default)]
pub struct FrameClock {
    start: Option<Instant>,
    last_us: Option<u64>,
}

impl FrameClock {
    /// Timestamp for a frame captured at `now`. The first frame is 0; a frame
    /// that arrives at the same microsecond as the previous one gets +1.
    pub fn stamp(&mut self, now: Instant) -> u64 {
        let start = *self.start.get_or_insert(now);
        let us = now.saturating_duration_since(start).as_micros();
        let us = u64::try_from(us).unwrap_or(u64::MAX);
        let us = match self.last_us {
            Some(last) if us <= last => last.saturating_add(1),
            _ => us,
        };
        self.last_us = Some(us);
        us
    }
}

/// A running capture session. Dropping it stops the stream and closes the
/// portal session.
pub struct ScreenCapture {
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    inner: linux::Session,
}

impl ScreenCapture {
    /// Opens the portal dialog and starts streaming. Blocks until the user
    /// picks a source (or cancels).
    pub fn start(request: CaptureRequest) -> Result<Self, CaptureError> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            Ok(Self {
                inner: linux::Session::start(request)?,
            })
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = request;
            Err(CaptureError::Unsupported)
        }
    }

    pub fn info(&self) -> &CaptureInfo {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.inner.info()
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            unreachable!("ScreenCapture cannot be constructed without capture support")
        }
    }

    /// Waits up to `timeout` for the next frame.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<CapturedFrame, CaptureError> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.inner.recv_timeout(timeout)
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = timeout;
            Err(CaptureError::Unsupported)
        }
    }

    /// Frames dropped because the consumer was too slow.
    pub fn dropped_frames(&self) -> u64 {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.inner.dropped_frames()
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_start_at_zero_and_always_increase() {
        let mut clock = FrameClock::default();
        let t0 = Instant::now();
        assert_eq!(clock.stamp(t0), 0);
        assert_eq!(clock.stamp(t0 + Duration::from_micros(16_667)), 16_667);
        // Same instant twice, or a timestamp from the past: still increasing.
        assert_eq!(clock.stamp(t0 + Duration::from_micros(16_667)), 16_668);
        assert_eq!(clock.stamp(t0), 16_669);
        assert_eq!(clock.stamp(t0 + Duration::from_millis(50)), 50_000);
    }

    #[test]
    fn output_size_follows_the_preset() {
        let src = Resolution::new(2560, 1600);
        assert_eq!(
            OutputSize::Preset(Preset::P1440, Alignment::Two).for_source(src),
            Ok(Resolution::new(2304, 1440))
        );
        assert_eq!(
            OutputSize::Native(Alignment::Two).for_source(Resolution::new(3840, 2160)),
            Ok(Resolution::new(2560, 1440))
        );
        assert!(OutputSize::Native(Alignment::Two)
            .for_source(Resolution::new(0, 10))
            .is_err());
    }

    #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
    #[test]
    fn unsupported_builds_say_so() {
        let request =
            CaptureRequest::new(CaptureSource::Monitor, OutputSize::Native(Alignment::Two));
        assert!(matches!(
            ScreenCapture::start(request),
            Err(CaptureError::Unsupported)
        ));
    }
}
