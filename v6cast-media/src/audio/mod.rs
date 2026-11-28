//! Conditional audio capture.
//!
//! The rule from the plan:
//!
//! * **Desktop** ([`CaptureSource::Monitor`]): the full system output, through
//!   a loopback of the default sink.
//! * **Window** ([`CaptureSource::Window`]): only the audio of that window's
//!   application. When the application plays nothing, or when the window
//!   cannot be tied to an application, the stream goes on **without audio,
//!   silently**: [`AudioCapture::start`] then returns a silent capture, not an
//!   error.
//!
//! [`plan_audio`] makes that decision from plain data (the window's identity
//! and the PipeWire audio nodes), so it is tested without hardware.
//!
//! Limit: the ScreenCast portal does not tell which process owns a shared
//! window. Unless the caller supplies a [`WindowIdentity`] (for example the UI
//! lets the user pick the application from [`list_audio_applications`]),
//! window capture is silent.
//!
//! Samples are 32-bit float, interleaved stereo at 48 kHz ([`SAMPLE_RATE`],
//! [`CHANNELS`]); PipeWire converts whatever the source produces.

#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod linux;

use std::time::Duration;

use thiserror::Error;

use crate::capture::{CaptureSource, MediaEpoch};

/// Sample rate of captured audio.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channels of captured audio (interleaved).
pub const CHANNELS: u32 = 2;

/// PipeWire `media.class` of an application's playback stream.
pub const CLASS_APP_OUTPUT: &str = "Stream/Output/Audio";
/// PipeWire `media.class` of a sink (speakers, headphones).
pub const CLASS_SINK: &str = "Audio/Sink";

/// One PipeWire audio node, as seen in the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioNode {
    pub id: u32,
    /// `object.serial`, used to target the node.
    pub serial: Option<u64>,
    pub media_class: String,
    pub node_name: Option<String>,
    pub app_name: Option<String>,
    pub app_id: Option<String>,
    pub process_id: Option<u32>,
    pub process_binary: Option<String>,
}

impl AudioNode {
    fn is_app_output(&self) -> bool {
        self.media_class == CLASS_APP_OUTPUT
    }
}

/// What is known about the shared window's application.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowIdentity {
    pub process_id: Option<u32>,
    /// Desktop application id (`org.mozilla.firefox`) or executable name.
    pub app_id: Option<String>,
}

/// Why no audio is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SilentReason {
    /// The window could not be tied to an application.
    UnknownWindowOwner,
    /// The application has no audio stream right now.
    ApplicationIsQuiet,
    /// No output device to loop back from.
    NoSink,
    /// Audio capture is not available in this build or on this platform.
    Unsupported,
    /// PipeWire could not be reached or refused the stream.
    PipeWire(String),
}

/// The decision of [`plan_audio`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPlan {
    /// Everything the default output device plays.
    SystemLoopback,
    /// These application streams (an app may have several), mixed together.
    Application {
        nodes: Vec<AudioNode>,
    },
    Silent(SilentReason),
}

fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// Last component of a reverse-DNS app id (`org.mozilla.firefox` -> `firefox`).
fn short_app_id(app_id: &str) -> String {
    normalize(app_id.rsplit('.').next().unwrap_or(app_id))
}

/// Whether `node` belongs to the window's application.
///
/// The process id is the strongest match. Names are a fallback because
/// browsers and Electron apps play sound from a helper process whose id
/// differs from the window's.
fn belongs_to(node: &AudioNode, window: &WindowIdentity) -> bool {
    if let (Some(pid), Some(node_pid)) = (window.process_id, node.process_id) {
        if pid == node_pid {
            return true;
        }
    }
    let Some(app) = window.app_id.as_deref() else {
        return false;
    };
    let (full, short) = (normalize(app), short_app_id(app));
    [&node.app_id, &node.process_binary, &node.app_name]
        .into_iter()
        .flatten()
        .map(|name| normalize(name))
        .any(|name| name == full || name == short || short_app_id(&name) == short)
}

/// Decides what audio to capture. See the module docs for the rules.
pub fn plan_audio(
    source: CaptureSource,
    window: Option<&WindowIdentity>,
    nodes: &[AudioNode],
) -> AudioPlan {
    match source {
        CaptureSource::Monitor => {
            if nodes.iter().any(|n| n.media_class == CLASS_SINK) {
                AudioPlan::SystemLoopback
            } else {
                AudioPlan::Silent(SilentReason::NoSink)
            }
        }
        CaptureSource::Window => {
            let Some(window) = window.filter(|w| w.process_id.is_some() || w.app_id.is_some())
            else {
                return AudioPlan::Silent(SilentReason::UnknownWindowOwner);
            };
            let mut streams: Vec<AudioNode> = nodes
                .iter()
                .filter(|n| n.is_app_output() && belongs_to(n, window))
                .cloned()
                .collect();
            if streams.is_empty() {
                return AudioPlan::Silent(SilentReason::ApplicationIsQuiet);
            }
            streams.sort_by_key(|n| n.id);
            AudioPlan::Application { nodes: streams }
        }
    }
}

/// Applications that are playing audio right now, one entry per stream.
/// Meant for a UI that lets the user tie a shared window to its audio.
pub fn list_audio_applications() -> Result<Vec<AudioNode>, AudioError> {
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    {
        Ok(linux::audio_nodes()?
            .into_iter()
            .filter(AudioNode::is_app_output)
            .collect())
    }
    #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
    {
        Err(AudioError::Unsupported)
    }
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("audio capture is not available in this build or on this platform")]
    Unsupported,
    #[error("PipeWire error: {0}")]
    PipeWire(String),
}

/// Captured samples from one source stream.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    /// Index of the source in the plan (0 for the system loopback).
    pub source: usize,
    /// Interleaved stereo f32 samples.
    pub samples: Vec<f32>,
    /// Microseconds since the session's [`MediaEpoch`], monotonic.
    pub timestamp_us: u64,
}

/// Adds `src` into `dst` sample by sample, clamping to [-1, 1]. Use it to mix
/// the chunks of several application streams.
pub fn mix_into(dst: &mut [f32], src: &[f32]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = (*d + *s).clamp(-1.0, 1.0);
    }
}

/// Audio that goes with a capture. Silent captures yield no chunks.
pub struct AudioCapture {
    plan: AudioPlan,
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    session: Option<linux::AudioSession>,
}

impl AudioCapture {
    /// Captures what [`plan_audio`] decides for `source` and `window`. Never
    /// fails because audio is impossible: it returns a silent capture whose
    /// [`AudioCapture::plan`] says why.
    ///
    /// Timestamps count from an epoch taken now; use
    /// [`AudioCapture::start_with_epoch`] to share the video capture's epoch.
    pub fn start(source: CaptureSource, window: Option<&WindowIdentity>) -> Self {
        Self::start_with_epoch(source, window, MediaEpoch::now())
    }

    /// Like [`AudioCapture::start`], with timestamps counted from `epoch`. Give
    /// the video capture the same epoch (`CaptureRequest::epoch`) for lip-sync.
    pub fn start_with_epoch(
        source: CaptureSource,
        window: Option<&WindowIdentity>,
        epoch: MediaEpoch,
    ) -> Self {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            let nodes = match linux::audio_nodes() {
                Ok(nodes) => nodes,
                Err(e) => return Self::silent(SilentReason::PipeWire(e.to_string())),
            };
            let plan = plan_audio(source, window, &nodes);
            if let AudioPlan::Silent(_) = plan {
                return Self {
                    plan,
                    session: None,
                };
            }
            match linux::AudioSession::start(&plan, epoch) {
                Ok(session) => Self {
                    plan,
                    session: Some(session),
                },
                Err(e) => Self::silent(SilentReason::PipeWire(e.to_string())),
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = (source, window, epoch);
            Self::silent(SilentReason::Unsupported)
        }
    }

    fn silent(reason: SilentReason) -> Self {
        tracing::info!("sending without audio: {reason:?}");
        Self {
            plan: AudioPlan::Silent(reason),
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            session: None,
        }
    }

    pub fn plan(&self) -> &AudioPlan {
        &self.plan
    }

    pub fn is_silent(&self) -> bool {
        matches!(self.plan, AudioPlan::Silent(_))
    }

    /// Next chunk, or `None` when silent or nothing arrived within `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<AudioChunk> {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        {
            self.session.as_ref()?.recv_timeout(timeout)
        }
        #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
        {
            let _ = timeout;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(id: u32) -> AudioNode {
        AudioNode {
            id,
            media_class: CLASS_SINK.into(),
            node_name: Some("alsa_output.pci".into()),
            ..Default::default()
        }
    }

    fn app(id: u32, pid: u32, binary: &str, name: &str) -> AudioNode {
        AudioNode {
            id,
            serial: Some(u64::from(id) + 1000),
            media_class: CLASS_APP_OUTPUT.into(),
            app_name: Some(name.into()),
            process_id: Some(pid),
            process_binary: Some(binary.into()),
            ..Default::default()
        }
    }

    fn nodes() -> Vec<AudioNode> {
        vec![
            sink(30),
            app(41, 1200, "firefox", "Firefox"),
            app(42, 1201, "firefox", "Firefox"),
            app(50, 2000, "spotify", "Spotify"),
            // A capture stream of another program, never a playback match.
            AudioNode {
                id: 60,
                media_class: "Stream/Input/Audio".into(),
                process_id: Some(3000),
                process_binary: Some("obs".into()),
                ..Default::default()
            },
        ]
    }

    fn ids(plan: &AudioPlan) -> Vec<u32> {
        match plan {
            AudioPlan::Application { nodes } => nodes.iter().map(|n| n.id).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn desktop_takes_the_system_loopback() {
        assert_eq!(
            plan_audio(CaptureSource::Monitor, None, &nodes()),
            AudioPlan::SystemLoopback
        );
        // Window identity does not matter for a monitor.
        let w = WindowIdentity {
            process_id: Some(2000),
            app_id: None,
        };
        assert_eq!(
            plan_audio(CaptureSource::Monitor, Some(&w), &nodes()),
            AudioPlan::SystemLoopback
        );
        assert_eq!(
            plan_audio(CaptureSource::Monitor, None, &[]),
            AudioPlan::Silent(SilentReason::NoSink)
        );
    }

    #[test]
    fn window_matches_its_process() {
        let w = WindowIdentity {
            process_id: Some(2000),
            app_id: None,
        };
        let plan = plan_audio(CaptureSource::Window, Some(&w), &nodes());
        assert_eq!(ids(&plan), vec![50]);
    }

    #[test]
    fn window_matches_helper_processes_by_name() {
        // The browser window's pid is not the audio process's pid.
        let w = WindowIdentity {
            process_id: Some(1100),
            app_id: Some("org.mozilla.firefox".into()),
        };
        let plan = plan_audio(CaptureSource::Window, Some(&w), &nodes());
        assert_eq!(ids(&plan), vec![41, 42]);
        let w = WindowIdentity {
            process_id: None,
            app_id: Some("Spotify".into()),
        };
        assert_eq!(
            ids(&plan_audio(CaptureSource::Window, Some(&w), &nodes())),
            vec![50]
        );
    }

    #[test]
    fn window_without_sound_or_owner_is_silent() {
        let quiet = WindowIdentity {
            process_id: Some(4242),
            app_id: Some("org.gnome.TextEditor".into()),
        };
        assert_eq!(
            plan_audio(CaptureSource::Window, Some(&quiet), &nodes()),
            AudioPlan::Silent(SilentReason::ApplicationIsQuiet)
        );
        assert_eq!(
            plan_audio(CaptureSource::Window, None, &nodes()),
            AudioPlan::Silent(SilentReason::UnknownWindowOwner)
        );
        assert_eq!(
            plan_audio(
                CaptureSource::Window,
                Some(&WindowIdentity::default()),
                &nodes()
            ),
            AudioPlan::Silent(SilentReason::UnknownWindowOwner)
        );
        // Recording programs are never treated as the window's audio.
        let obs = WindowIdentity {
            process_id: Some(3000),
            app_id: Some("obs".into()),
        };
        assert_eq!(
            plan_audio(CaptureSource::Window, Some(&obs), &nodes()),
            AudioPlan::Silent(SilentReason::ApplicationIsQuiet)
        );
    }

    #[test]
    fn names_do_not_match_by_substring() {
        let w = WindowIdentity {
            process_id: None,
            app_id: Some("fire".into()),
        };
        assert_eq!(
            plan_audio(CaptureSource::Window, Some(&w), &nodes()),
            AudioPlan::Silent(SilentReason::ApplicationIsQuiet)
        );
    }

    #[test]
    fn mixing_adds_and_clamps() {
        let mut a = vec![0.25, -0.5, 0.9, -0.9];
        mix_into(&mut a, &[0.25, -0.25, 0.5, -0.5, 7.0]);
        assert_eq!(a, vec![0.5, -0.75, 1.0, -1.0]);
    }

    #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
    #[test]
    fn builds_without_capture_send_silently() {
        let audio = AudioCapture::start(CaptureSource::Monitor, None);
        assert!(audio.is_silent());
        assert_eq!(audio.plan(), &AudioPlan::Silent(SilentReason::Unsupported));
        assert_eq!(audio.recv_timeout(Duration::from_millis(1)), None);
        assert!(matches!(
            list_audio_applications(),
            Err(AudioError::Unsupported)
        ));
    }
}
