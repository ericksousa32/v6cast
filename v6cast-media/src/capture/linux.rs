//! XDG Desktop Portal (ScreenCast) and PipeWire capture on Linux.
//!
//! One thread per session owns everything: a one-worker Tokio runtime for the
//! portal's D-Bus calls (it keeps the session alive while streaming) and the
//! PipeWire main loop. Frames leave through a bounded channel; when the
//! consumer is slow, new frames are dropped and counted rather than queued.
//!
//! Only shared-memory buffers in 4-byte RGB formats are negotiated, which every
//! portal backend (GNOME, KDE, wlroots) supports.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::video::{VideoFormat, VideoInfoRaw};

use super::{
    to_i420, CaptureError, CaptureInfo, CaptureRequest, CaptureSource, CapturedFrame, FrameClock,
    PixelLayout, RawImage,
};
use crate::scale::Resolution;

/// Frames buffered between the capture thread and the consumer.
const QUEUE: usize = 4;

pub(super) struct Session {
    info: CaptureInfo,
    frames: Receiver<Result<CapturedFrame, CaptureError>>,
    dropped: Arc<AtomicU64>,
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

type Ready = Result<(CaptureInfo, pw::channel::Sender<()>), CaptureError>;

impl Session {
    pub(super) fn start(request: CaptureRequest) -> Result<Self, CaptureError> {
        let (ready_tx, ready_rx) = mpsc::channel::<Ready>();
        let (frame_tx, frames) = mpsc::sync_channel(QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_in_thread = Arc::clone(&dropped);
        let thread = std::thread::Builder::new()
            .name("v6cast-capture".into())
            .spawn(move || capture_thread(request, ready_tx, frame_tx, dropped_in_thread))
            .map_err(|e| CaptureError::PipeWire(e.to_string()))?;
        let (info, stop) = ready_rx.recv().map_err(|_| CaptureError::Closed)??;
        Ok(Self {
            info,
            frames,
            dropped,
            stop,
            thread: Some(thread),
        })
    }

    pub(super) fn info(&self) -> &CaptureInfo {
        &self.info
    }

    pub(super) fn recv_timeout(&self, timeout: Duration) -> Result<CapturedFrame, CaptureError> {
        match self.frames.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(CaptureError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Closed),
        }
    }

    pub(super) fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn portal_error(e: ashpd::Error) -> CaptureError {
    match e {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => CaptureError::Cancelled,
        other => CaptureError::Portal(other.to_string()),
    }
}

fn capture_thread(
    request: CaptureRequest,
    ready: mpsc::Sender<Ready>,
    frames: SyncSender<Result<CapturedFrame, CaptureError>>,
    dropped: Arc<AtomicU64>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Portal(e.to_string())));
            return;
        }
    };
    let portal = runtime.block_on(async {
        let proxy = Screencast::new().await.map_err(portal_error)?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(portal_error)?;
        let sources = match request.source {
            CaptureSource::Monitor => SourceType::MONITOR,
            CaptureSource::Window => SourceType::WINDOW,
        };
        let cursor = if request.show_cursor {
            CursorMode::EMBEDDED
        } else {
            CursorMode::HIDDEN
        };
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(cursor)
                    .set_sources(sources)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(portal_error)?;
        let streams = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(portal_error)?
            .response()
            .map_err(portal_error)?;
        let stream = streams
            .streams()
            .first()
            .cloned()
            .ok_or(CaptureError::Portal("the portal returned no stream".into()))?;
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(portal_error)?;
        let source = match stream.source_type() {
            Some(t) if t.contains(SourceType::WINDOW) => CaptureSource::Window,
            Some(_) => CaptureSource::Monitor,
            None => request.source,
        };
        let size = stream.size().and_then(|(w, h)| {
            Some(Resolution::new(
                u32::try_from(w).ok()?,
                u32::try_from(h).ok()?,
            ))
        });
        let info = CaptureInfo {
            source,
            node_id: stream.pipe_wire_node_id(),
            size,
        };
        Ok::<_, CaptureError>((session, info, fd))
    });
    let (session, info, fd) = match portal {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if let Err(e) = stream_frames(request, info, fd, &ready, frames, dropped) {
        // Before `ready` was answered this reaches the caller; later errors end
        // the stream and the consumer sees `Closed`.
        let _ = ready.send(Err(e));
    }
    let _ = runtime.block_on(session.close());
}

struct StreamState {
    format: VideoInfoRaw,
    layout: Option<PixelLayout>,
    clock: FrameClock,
    request: CaptureRequest,
    frames: SyncSender<Result<CapturedFrame, CaptureError>>,
    dropped: Arc<AtomicU64>,
}

fn layout_for(format: VideoFormat) -> Option<PixelLayout> {
    match format {
        VideoFormat::BGRx => Some(PixelLayout::Bgrx),
        VideoFormat::BGRA => Some(PixelLayout::Bgra),
        VideoFormat::RGBx => Some(PixelLayout::Rgbx),
        VideoFormat::RGBA => Some(PixelLayout::Rgba),
        _ => None,
    }
}

fn stream_frames(
    request: CaptureRequest,
    info: CaptureInfo,
    fd: OwnedFd,
    ready: &mpsc::Sender<Ready>,
    frames: SyncSender<Result<CapturedFrame, CaptureError>>,
    dropped: Arc<AtomicU64>,
) -> Result<(), CaptureError> {
    let pw_err = |e: pw::Error| CaptureError::PipeWire(e.to_string());
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_err)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_err)?;
    let core = context.connect_fd_rc(fd, None).map_err(pw_err)?;
    let stream = pw::stream::StreamRc::new(
        core,
        "v6cast-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(pw_err)?;

    let state = StreamState {
        format: VideoInfoRaw::default(),
        layout: None,
        clock: request
            .epoch
            .map(FrameClock::with_epoch)
            .unwrap_or_default(),
        request,
        frames,
        dropped,
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if state.format.parse(param).is_err() {
                state.layout = None;
                return;
            }
            state.layout = layout_for(state.format.format());
            if state.layout.is_none() {
                tracing::warn!("unsupported capture format {:?}", state.format.format());
            }
        })
        .process(|stream, state| {
            let Some(layout) = state.layout else { return };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let now = Instant::now();
            let size = state.format.size();
            let source_size = Resolution::new(size.width, size.height);
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let (offset, len, stride) = {
                let chunk = data.chunk();
                (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride(),
                )
            };
            let stride = usize::try_from(stride)
                .ok()
                .filter(|s| *s > 0)
                .unwrap_or(source_size.width as usize * 4);
            let Some(bytes) = data.data() else { return };
            let Some(bytes) = bytes.get(offset..offset.saturating_add(len)) else {
                return;
            };
            let result = state
                .request
                .output
                .for_source(source_size)
                .map_err(CaptureError::from)
                .and_then(|target| {
                    to_i420(
                        &RawImage {
                            data: bytes,
                            stride,
                            size: source_size,
                            layout,
                        },
                        target,
                    )
                })
                .map(|frame| CapturedFrame {
                    frame,
                    source_size,
                    timestamp_us: state.clock.stamp(now),
                });
            match state.frames.try_send(result) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    state.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        })
        .register()
        .map_err(pw_err)?;

    let format = enum_format_pod()?;
    let mut params = [spa::pod::Pod::from_bytes(&format)
        .ok_or(CaptureError::PipeWire("bad format pod".into()))?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(info.node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(pw_err)?;

    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let quit = mainloop.clone();
    let _stop = stop_rx.attach(mainloop.loop_(), move |()| quit.quit());
    ready
        .send(Ok((info, stop_tx)))
        .map_err(|_| CaptureError::Closed)?;
    mainloop.run();
    Ok(())
}

/// EnumFormat parameter: raw video in BGRx, BGRA, RGBx or RGBA, any size up
/// to 8K, any frame rate.
fn enum_format_pod() -> Result<Vec<u8>, CaptureError> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 2,
                height: 2
            },
            spa::utils::Rectangle {
                width: 7680,
                height: 4320
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|e| CaptureError::PipeWire(format!("cannot build format pod: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_formats_map_to_layouts() {
        assert_eq!(layout_for(VideoFormat::BGRx), Some(PixelLayout::Bgrx));
        assert_eq!(layout_for(VideoFormat::RGBA), Some(PixelLayout::Rgba));
        assert_eq!(layout_for(VideoFormat::I420), None);
        assert_eq!(layout_for(VideoFormat::NV12), None);
    }

    #[test]
    fn format_pod_serializes() {
        let pod = enum_format_pod().unwrap();
        assert!(spa::pod::Pod::from_bytes(&pod).is_some());
    }

    /// Needs a Wayland session with xdg-desktop-portal; the owner runs the
    /// `capture_portal` example on the host instead.
    #[test]
    #[ignore = "needs the host Wayland session and a person to answer the portal dialog"]
    fn portal_capture_delivers_frames() {
        use crate::scale::Alignment;
        let capture = super::super::ScreenCapture::start(CaptureRequest::new(
            CaptureSource::Monitor,
            super::super::OutputSize::Native(Alignment::Two),
        ))
        .unwrap();
        let frame = capture.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(frame.frame.resolution().width > 0);
    }
}
