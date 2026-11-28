//! VA-API H.264 encode and decode (Linux, `vaapi` feature).
//!
//! Built on `cros-codecs` (ChromeOS), a maintained, safe Rust stack over
//! `cros-libva`: it writes SPS/PPS/slice headers itself for encode and has a
//! full H.264 parser and DPB for stateless decode, so this module contains no
//! unsafe code. Frames are GBM buffer objects on the first DRM render node,
//! shared with VA-API as DMA-BUFs.
//!
//! Desktop Mesa GBM cannot allocate NV12 buffers (ChromeOS minigbm can, which
//! is what cros-codecs expects). [`nv12_frame`] allocates one linear R8 buffer
//! tall enough for both planes and describes it to VA-API as NV12.
//!
//! cros-codecs uses `Rc` throughout, so its objects cannot cross threads. Each
//! [`VaapiEncoder`] and [`VaapiDecoder`] therefore owns one worker thread that
//! creates and keeps every VA object; the handle only holds channels and is
//! `Send`. Calls are synchronous: send a job, wait for its answer. A worker
//! that dies (driver crash, panic inside the library) surfaces as a
//! [`CodecError::Backend`], which [`crate::codec::FallbackEncoder`] turns into
//! a switch to software.
//!
//! Desktop Mesa needs some fixes on top of cros-codecs, which targets ChromeOS:
//! the vendored copy (see `vendor/README.md`) creates its first decoder context
//! at a size AMD accepts and outputs low-delay pictures at once, and
//! [`repair_vaapi_stream`] fixes the slice NAL headers and SPS constraint flags
//! of the encoder output.
//!
//! Measured on an AMD Radeon RX 9060 XT (radeonsi, Mesa 26.2) in a release
//! build: 12 ms to encode and 9 ms to decode (with readback) a 2560x1440 frame.

use std::fs::File;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use gbm::{BufferObjectFlags, Format as GbmFormat};

use cros_codecs::backend::vaapi::decoder::VaapiBackend as VaDecodeBackend;
use cros_codecs::bitstream_utils::NalIterator;
use cros_codecs::codec::h264::parser::{Level as CrosLevel, Nalu, Profile as CrosProfile};
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::{DecodeError, StatelessDecoder, StatelessVideoDecoder};
use cros_codecs::decoder::{DecodedHandle, DecoderEvent, StreamInfo};
use cros_codecs::encoder::h264::EncoderConfig as CrosConfig;
use cros_codecs::encoder::stateless::h264::StatelessEncoder;
use cros_codecs::encoder::{
    FrameMetadata, PredictionStructure, RateControl, Tunings, VideoEncoder as CrosEncoder,
};
use cros_codecs::image_processing::nv12_to_i420;
use cros_codecs::libva::{
    Display, Image, UsageHint, VAEntrypoint, VAImageFormat, VAProfile, VA_FOURCC_NV12,
    VA_RT_FORMAT_YUV420,
};
use cros_codecs::video_frame::generic_dma_video_frame::GenericDmaVideoFrame;
use cros_codecs::video_frame::{VideoFrame, UV_PLANE, Y_PLANE};
use cros_codecs::{BlockingMode, Fourcc, FrameLayout, PlaneLayout, Resolution as CrosResolution};

use crate::codec::h264::{is_idr, level_for, repair_vaapi_stream};
use crate::codec::{
    Backend, BackendCaps, CodecError, EncodedFrame, EncoderConfig, H264Profile, VideoDecoder,
    VideoEncoder, VAAPI_MIN_ENCODE_RESOLUTION,
};
use crate::frame::YuvFrame;
use crate::scale::Resolution;

fn err(e: impl std::fmt::Debug) -> CodecError {
    CodecError::backend(Backend::Vaapi, format!("{e:?}"))
}

fn worker_gone() -> CodecError {
    CodecError::backend(Backend::Vaapi, "worker thread stopped")
}

/// First DRM render node (`/dev/dri/renderD128` on single-GPU machines).
fn render_node() -> Option<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

type GbmDevice = gbm::Device<File>;

fn open_gbm(node: &std::path::Path) -> Result<GbmDevice, CodecError> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(node)
        .map_err(err)?;
    GbmDevice::new(file).map_err(err)
}

/// A linear NV12 frame of `coded` size: one R8 buffer of `coded.height * 3 / 2`
/// rows, the UV plane right after the Y plane, both with the buffer's stride.
fn nv12_frame(gbm: &GbmDevice, coded: CrosResolution) -> Result<GenericDmaVideoFrame, CodecError> {
    let rows = coded.height + coded.height / 2;
    let bo = gbm
        .create_buffer_object::<()>(coded.width, rows, GbmFormat::R8, BufferObjectFlags::LINEAR)
        .map_err(err)?;
    let stride = bo.stride().map_err(err)? as usize;
    // The DMA-BUF keeps the memory alive after `bo` is dropped.
    let fd = bo.fd().map_err(err)?;
    let layout = FrameLayout {
        format: (Fourcc::from(b"NV12"), 0), // DRM_FORMAT_MOD_LINEAR
        size: coded,
        planes: vec![
            PlaneLayout {
                buffer_index: 0,
                offset: 0,
                stride,
            },
            PlaneLayout {
                buffer_index: 0,
                offset: stride * coded.height as usize,
                stride,
            },
        ],
    };
    GenericDmaVideoFrame::new(vec![File::from(fd)], layout).map_err(err)
}

fn open_display() -> Result<Rc<Display>, CodecError> {
    let node = render_node().ok_or(CodecError::Unavailable("DRM render node"))?;
    Display::open_drm_display(&node).map_err(err)
}

fn supports(
    display: &Display,
    profile: VAProfile::Type,
    entrypoints: &[VAEntrypoint::Type],
) -> bool {
    display
        .query_config_entrypoints(profile)
        .is_ok_and(|eps| eps.iter().any(|e| entrypoints.contains(e)))
}

/// What the VA driver offers for H.264, or `None` without a usable device.
pub(crate) fn probe() -> Option<BackendCaps> {
    let display = open_display().ok()?;
    let profiles = display.query_config_profiles().ok()?;
    let enc_eps = [
        VAEntrypoint::VAEntrypointEncSlice,
        VAEntrypoint::VAEntrypointEncSliceLP,
    ];
    let cb = VAProfile::VAProfileH264ConstrainedBaseline;
    let encode = profiles.contains(&cb) && supports(&display, cb, &enc_eps);
    let decode = [
        cb,
        VAProfile::VAProfileH264Main,
        VAProfile::VAProfileH264High,
    ]
    .into_iter()
    .any(|p| profiles.contains(&p) && supports(&display, p, &[VAEntrypoint::VAEntrypointVLD]));
    (encode || decode).then_some(BackendCaps {
        backend: Backend::Vaapi,
        encode,
        decode,
        // radeonsi and iHD accept 4096x4096, above the 2.5K ceiling we use.
        max_resolution: Some(Resolution::new(4096, 4096)),
        min_resolution: Some(VAAPI_MIN_ENCODE_RESOLUTION),
    })
}

/// Starts `body` on a named worker thread and waits until it reports that its
/// setup succeeded.
/// Job sender, reply receiver and thread of a worker.
type Worker<J, R> = (Sender<J>, Receiver<R>, JoinHandle<()>);

fn spawn_worker<J: Send + 'static, R: Send + 'static>(
    name: &str,
    body: impl FnOnce(Sender<Result<(), CodecError>>, Receiver<J>, Sender<R>) + Send + 'static,
) -> Result<Worker<J, R>, CodecError> {
    let (job_tx, job_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || body(ready_tx, job_rx, reply_tx))
        .map_err(err)?;
    ready_rx.recv().map_err(|_| worker_gone())??;
    Ok((job_tx, reply_rx, thread))
}

fn coded_size(r: Resolution) -> CrosResolution {
    CrosResolution {
        width: r.width.div_ceil(16) * 16,
        height: r.height.div_ceil(16) * 16,
    }
}

fn cros_resolution(r: Resolution) -> CrosResolution {
    CrosResolution {
        width: r.width,
        height: r.height,
    }
}

fn tunings(config: &EncoderConfig) -> Tunings {
    Tunings {
        rate_control: RateControl::ConstantBitrate(u64::from(config.bitrate_bps)),
        framerate: u32::from(config.frame_rate.fps()),
        ..Tunings::default()
    }
}

enum EncodeJob {
    Encode {
        frame: YuvFrame,
        timestamp_us: u64,
        keyframe: bool,
    },
    Tune(Tunings),
}

enum EncodeReply {
    Encoded(Result<Option<EncodedFrame>, CodecError>),
    Tuned(Result<(), CodecError>),
}

/// H.264 encoder on the VA-API device. See the module docs.
pub struct VaapiEncoder {
    config: EncoderConfig,
    jobs: Sender<EncodeJob>,
    replies: Receiver<EncodeReply>,
    force_keyframe: bool,
    _thread: JoinHandle<()>,
}

impl VaapiEncoder {
    pub fn new(config: &EncoderConfig) -> Result<Self, CodecError> {
        config.validate()?;
        // Refused before any VA object exists: small streams can fault the GPU
        // and abort the process (see VAAPI_MIN_ENCODE_RESOLUTION).
        let r = config.resolution;
        let min = VAAPI_MIN_ENCODE_RESOLUTION;
        if r.width < min.width || r.height < min.height {
            return Err(CodecError::Unavailable(
                "VA-API encoder for streams below 320x192",
            ));
        }
        let cfg = config.clone();
        let (jobs, replies, thread) =
            spawn_worker("v6cast-vaapi-enc", move |ready, jobs, replies| {
                encoder_worker(cfg, ready, jobs, replies)
            })?;
        Ok(Self {
            config: config.clone(),
            jobs,
            replies,
            force_keyframe: true,
            _thread: thread,
        })
    }
}

type VaEncoder = StatelessEncoder<
    GenericDmaVideoFrame,
    cros_codecs::backend::vaapi::encoder::VaapiBackend<
        <GenericDmaVideoFrame as VideoFrame>::MemDescriptor,
        cros_codecs::libva::Surface<<GenericDmaVideoFrame as VideoFrame>::MemDescriptor>,
    >,
>;

fn encoder_worker(
    config: EncoderConfig,
    ready: Sender<Result<(), CodecError>>,
    jobs: Receiver<EncodeJob>,
    replies: Sender<EncodeReply>,
) {
    let setup = || -> Result<(VaEncoder, GbmDevice, Readback), CodecError> {
        let node = render_node().ok_or(CodecError::Unavailable("DRM render node"))?;
        let display = Display::open_drm_display(&node).map_err(err)?;
        let gbm = open_gbm(&node)?;
        let images = image_access(&display)?;
        let (profile, va_profile) = match config.profile {
            H264Profile::ConstrainedBaseline => (
                CrosProfile::Baseline,
                VAProfile::VAProfileH264ConstrainedBaseline,
            ),
            H264Profile::Main => (CrosProfile::Main, VAProfile::VAProfileH264Main),
        };
        let low_power = !supports(&display, va_profile, &[VAEntrypoint::VAEntrypointEncSlice])
            && supports(
                &display,
                va_profile,
                &[VAEntrypoint::VAEntrypointEncSliceLP],
            );
        let level =
            CrosLevel::try_from(level_for(config.resolution, u32::from(config.frame_rate.fps())).0)
                .map_err(err)?;
        let cros_config = CrosConfig {
            resolution: cros_resolution(config.resolution),
            profile,
            level,
            pred_structure: PredictionStructure::LowDelay {
                limit: u16::try_from(config.keyframe_interval_frames()).unwrap_or(u16::MAX),
            },
            initial_tunings: tunings(&config),
        };
        let encoder = VaEncoder::new_vaapi(
            display,
            cros_config,
            Fourcc::from(b"NV12"),
            coded_size(config.resolution),
            low_power,
            BlockingMode::Blocking,
        )
        .map_err(err)?;
        Ok((encoder, gbm, images))
    };
    let (mut encoder, gbm, images) = match setup() {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    for job in jobs {
        let reply = match job {
            EncodeJob::Encode {
                frame,
                timestamp_us,
                keyframe,
            } => EncodeReply::Encoded(encode_one(
                &mut encoder,
                &gbm,
                &images,
                &config,
                &frame,
                timestamp_us,
                keyframe,
            )),
            EncodeJob::Tune(t) => EncodeReply::Tuned(encoder.tune(t).map_err(err)),
        };
        if replies.send(reply).is_err() {
            break;
        }
    }
}

/// Uploads one I420 frame into a GBM NV12 buffer and encodes it.
fn encode_one(
    encoder: &mut VaEncoder,
    gbm: &GbmDevice,
    images: &Readback,
    config: &EncoderConfig,
    frame: &YuvFrame,
    timestamp_us: u64,
    keyframe: bool,
) -> Result<Option<EncodedFrame>, CodecError> {
    let r = frame.resolution();
    let surface = nv12_frame(gbm, coded_size(r))?;
    let pitches = surface.get_plane_pitch();
    let offset_uv = pitches[Y_PLANE] * coded_size(r).height as usize;
    upload_nv12(images, &surface, frame)?;
    let layout = FrameLayout {
        format: (Fourcc::from(b"NV12"), 0),
        size: coded_size(r),
        planes: vec![
            PlaneLayout {
                buffer_index: 0,
                offset: 0,
                stride: pitches[Y_PLANE],
            },
            PlaneLayout {
                buffer_index: 0,
                offset: offset_uv,
                stride: pitches[UV_PLANE],
            },
        ],
    };
    let meta = FrameMetadata {
        timestamp: timestamp_us,
        layout,
        force_keyframe: keyframe,
    };
    encoder.encode(meta, surface).map_err(err)?;
    let mut data = Vec::new();
    while let Some(coded) = encoder.poll().map_err(err)? {
        data.extend_from_slice(&coded.bitstream);
    }
    if data.is_empty() {
        return Ok(None);
    }
    repair_vaapi_stream(&mut data);
    let _ = config;
    Ok(Some(EncodedFrame {
        keyframe: is_idr(&data),
        data,
        timestamp_us,
    }))
}

impl VideoEncoder for VaapiEncoder {
    fn backend(&self) -> Backend {
        Backend::Vaapi
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &YuvFrame,
        timestamp_us: u64,
    ) -> Result<Option<EncodedFrame>, CodecError> {
        if frame.resolution() != self.config.resolution {
            return Err(CodecError::ResolutionMismatch {
                expected: self.config.resolution,
                got: frame.resolution(),
            });
        }
        let keyframe = std::mem::take(&mut self.force_keyframe);
        self.jobs
            .send(EncodeJob::Encode {
                frame: frame.clone(),
                timestamp_us,
                keyframe,
            })
            .map_err(|_| worker_gone())?;
        match self.replies.recv().map_err(|_| worker_gone())? {
            EncodeReply::Encoded(result) => result,
            EncodeReply::Tuned(_) => Err(err("out-of-order reply")),
        }
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        if bps == 0 {
            return Err(CodecError::InvalidConfig("bitrate is zero"));
        }
        let mut config = self.config.clone();
        config.bitrate_bps = bps;
        self.jobs
            .send(EncodeJob::Tune(tunings(&config)))
            .map_err(|_| worker_gone())?;
        match self.replies.recv().map_err(|_| worker_gone())? {
            EncodeReply::Tuned(result) => result?,
            EncodeReply::Encoded(_) => return Err(err("out-of-order reply")),
        }
        self.config = config;
        Ok(())
    }
}

/// H.264 decoder on the VA-API device. See the module docs.
pub struct VaapiDecoder {
    jobs: Sender<Vec<u8>>,
    replies: Receiver<Result<Option<YuvFrame>, CodecError>>,
    _thread: JoinHandle<()>,
}

impl VaapiDecoder {
    pub fn new() -> Result<Self, CodecError> {
        let (jobs, replies, thread) = spawn_worker("v6cast-vaapi-dec", decoder_worker)?;
        Ok(Self {
            jobs,
            replies,
            _thread: thread,
        })
    }
}

type VaDecoder = StatelessDecoder<H264, VaDecodeBackend<GenericDmaVideoFrame>>;

/// Moves pictures between system memory and GPU buffers through VA images.
///
/// Frames live in GPU memory, which is uncached for CPU access: mapping the
/// DMA-BUF and converting a decoded 2560x1440 frame took about 1.9 s (230 ms
/// with one bulk copy per plane). `vaGetImage`/`vaPutImage` let the GPU copy
/// the picture to or from a staging image in system memory instead.
struct Readback {
    display: Rc<Display>,
    format: VAImageFormat,
}

fn decoder_worker(
    ready: Sender<Result<(), CodecError>>,
    jobs: Receiver<Vec<u8>>,
    replies: Sender<Result<Option<YuvFrame>, CodecError>>,
) {
    let setup = || -> Result<(VaDecoder, GbmDevice, Readback), CodecError> {
        let node = render_node().ok_or(CodecError::Unavailable("DRM render node"))?;
        let display = Display::open_drm_display(&node).map_err(err)?;
        let gbm = open_gbm(&node)?;
        let readback = image_access(&display)?;
        let decoder = VaDecoder::new_vaapi(display, BlockingMode::Blocking).map_err(err)?;
        Ok((decoder, gbm, readback))
    };
    let (mut decoder, gbm, readback) = match setup() {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut stream: Option<StreamInfo> = None;
    let mut timestamp = 0u64;
    for access_unit in jobs {
        timestamp += 1;
        let result = decode_one(
            &mut decoder,
            &gbm,
            &readback,
            &mut stream,
            timestamp,
            &access_unit,
        );
        if replies.send(result).is_err() {
            break;
        }
    }
}

fn decode_one(
    decoder: &mut VaDecoder,
    gbm: &GbmDevice,
    readback: &Readback,
    stream: &mut Option<StreamInfo>,
    timestamp: u64,
    access_unit: &[u8],
) -> Result<Option<YuvFrame>, CodecError> {
    let mut picture = None;
    // An access unit delimiter after the access unit tells the decoder that the
    // picture is complete (see the vendored cros-codecs patch), so it is output
    // now instead of when the next access unit starts.
    const AUD: [u8; 6] = [0, 0, 0, 1, 0x09, 0xf0];
    let mut stream_data = Vec::with_capacity(access_unit.len() + AUD.len());
    stream_data.extend_from_slice(access_unit);
    stream_data.extend_from_slice(&AUD);
    for nal in NalIterator::<Nalu>::new(&stream_data) {
        let mut rest: &[u8] = &nal;
        let mut stalls = 0;
        while !rest.is_empty() {
            let mut alloc = || {
                let info = stream.as_ref()?;
                nv12_frame(gbm, info.coded_resolution).ok()
            };
            match decoder.decode(timestamp, rest, &mut alloc) {
                Ok(0) => break,
                Ok(n) => {
                    rest = &rest[n.min(rest.len())..];
                    stalls = 0;
                }
                Err(DecodeError::CheckEvents | DecodeError::NotEnoughOutputBuffers(_)) => {
                    stalls += 1;
                    if stalls > 2 {
                        return Err(err("decoder made no progress"));
                    }
                    drain_events(decoder, readback, stream, &mut picture)?;
                }
                Err(e) => return Err(err(e)),
            }
        }
    }
    drain_events(decoder, readback, stream, &mut picture)?;
    Ok(picture)
}

fn drain_events(
    decoder: &mut VaDecoder,
    readback: &Readback,
    stream: &mut Option<StreamInfo>,
    picture: &mut Option<YuvFrame>,
) -> Result<(), CodecError> {
    while let Some(event) = decoder.next_event() {
        match event {
            DecoderEvent::FormatChanged => *stream = decoder.stream_info().cloned(),
            DecoderEvent::FrameReady(handle) => {
                handle.sync().map_err(err)?;
                *picture = Some(read_nv12(
                    readback,
                    handle.video_frame().as_ref(),
                    handle.display_resolution(),
                )?);
            }
        }
    }
    Ok(())
}

fn image_access(display: &Rc<Display>) -> Result<Readback, CodecError> {
    let format = display
        .query_image_formats()
        .map_err(err)?
        .into_iter()
        .find(|f| f.fourcc == VA_FOURCC_NV12)
        .ok_or(CodecError::Unavailable("VA NV12 image format"))?;
    Ok(Readback {
        display: Rc::clone(display),
        format,
    })
}

/// Writes an I420 frame into the NV12 GPU buffer through a VA image: the CPU
/// fills the image in system memory and `vaPutImage` (when the image is
/// dropped) lets the GPU copy it. Writing straight into the mapped buffer cost
/// about 90 ms per 2560x1440 frame.
fn upload_nv12(
    images: &Readback,
    target: &GenericDmaVideoFrame,
    frame: &YuvFrame,
) -> Result<(), CodecError> {
    let coded = target.resolution();
    let mut surfaces = images
        .display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            coded.width,
            coded.height,
            Some(UsageHint::USAGE_HINT_ENCODER),
            vec![target.clone()],
        )
        .map_err(err)?;
    let surface = surfaces.pop().ok_or_else(|| err("no surface"))?;
    let r = frame.resolution();
    let mut image = Image::create_from(
        &surface,
        images.format,
        (coded.width, coded.height),
        (r.width, r.height),
    )
    .map_err(err)?;
    let va = *image.image();
    let (y_off, uv_off) = (va.offsets[0] as usize, va.offsets[1] as usize);
    let (y_pitch, uv_pitch) = (va.pitches[0] as usize, va.pitches[1] as usize);
    let (w, h) = (r.width as usize, r.height as usize);
    let data: &mut [u8] = image.as_mut();
    if y_off + y_pitch * (h - 1) + w > data.len()
        || uv_off + uv_pitch * (h / 2 - 1) + w > data.len()
    {
        return Err(err("VA image smaller than the frame"));
    }
    for row in 0..h {
        data[y_off + row * y_pitch..][..w].copy_from_slice(&frame.y()[row * w..][..w]);
    }
    let cw = w / 2;
    for row in 0..h / 2 {
        let dst = &mut data[uv_off + row * uv_pitch..][..w];
        let (u, v) = (&frame.u()[row * cw..][..cw], &frame.v()[row * cw..][..cw]);
        for (i, pair) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            *pair = [u[i], v[i]];
        }
    }
    drop(image); // vaPutImage
    surface.sync().map_err(err)?;
    Ok(())
}

/// Copies a decoded NV12 frame into an I420 [`YuvFrame`] (see [`Readback`]).
fn read_nv12(
    readback: &Readback,
    frame: &GenericDmaVideoFrame,
    display: CrosResolution,
) -> Result<YuvFrame, CodecError> {
    let r = Resolution::new(display.width & !1, display.height & !1);
    let coded = frame.resolution();
    let mut surfaces = readback
        .display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            coded.width,
            coded.height,
            Some(UsageHint::USAGE_HINT_DECODER),
            vec![frame.clone()],
        )
        .map_err(err)?;
    let surface = surfaces.pop().ok_or_else(|| err("no surface"))?;
    let image = Image::create_from(
        &surface,
        readback.format,
        (coded.width, coded.height),
        (r.width, r.height),
    )
    .map_err(err)?;
    let va = *image.image();
    let data: &[u8] = image.as_ref();
    let (w, h) = (r.width as usize, r.height as usize);
    let (y_off, uv_off) = (va.offsets[0] as usize, va.offsets[1] as usize);
    let (y_pitch, uv_pitch) = (va.pitches[0] as usize, va.pitches[1] as usize);
    if y_off + y_pitch * (h - 1) + w > data.len()
        || uv_off + uv_pitch * (h / 2 - 1) + w > data.len()
    {
        return Err(err("VA image smaller than the picture"));
    }
    let mut out = YuvFrame::black(r)?;
    let (y, u, v) = out.planes_mut();
    nv12_to_i420(
        &data[y_off..],
        y_pitch,
        y,
        w,
        &data[uv_off..],
        uv_pitch,
        u,
        w / 2,
        v,
        w / 2,
        w,
        h,
    );
    Ok(out)
}

impl VideoDecoder for VaapiDecoder {
    fn backend(&self) -> Backend {
        Backend::Vaapi
    }

    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        self.jobs
            .send(access_unit.to_vec())
            .map_err(|_| worker_gone())?;
        self.replies.recv().map_err(|_| worker_gone())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::software::{SoftwareDecoder, SoftwareEncoder};
    use crate::scale::{fit_preset, Alignment, FrameRate, Preset, VideoProfile};

    fn hardware() -> bool {
        let caps = probe();
        if caps.as_ref().is_none_or(|c| !(c.encode && c.decode)) {
            eprintln!("skipping: no VA-API device with H.264 encode and decode");
            return false;
        }
        true
    }

    fn pattern(r: Resolution, t: u32) -> YuvFrame {
        let mut f = YuvFrame::black(r).unwrap();
        let w = r.width as usize;
        let (y, u, v) = f.planes_mut();
        for (i, px) in y.iter_mut().enumerate() {
            *px = 16 + ((i % w + i / w + t as usize * 4) % 200) as u8;
        }
        u.fill(100);
        v.fill(150);
        f
    }

    fn luma_error(a: &YuvFrame, b: &YuvFrame) -> f64 {
        let sum: u64 = a
            .y()
            .iter()
            .zip(b.y())
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        sum as f64 / a.y().len() as f64
    }

    fn roundtrip(
        r: Resolution,
        profile: VideoProfile,
        decoder: &mut dyn VideoDecoder,
        frames: u32,
    ) -> u32 {
        let config = EncoderConfig::new(r, profile);
        let mut enc = VaapiEncoder::new(&config).unwrap();
        let mut decoded = 0;
        for t in 0..frames {
            let input = pattern(r, t);
            let out = enc
                .encode(&input, u64::from(t))
                .unwrap()
                .expect("no output");
            assert_eq!(out.keyframe, t == 0, "{r} frame {t}");
            if let Some(picture) = decoder.decode(&out.data).unwrap() {
                assert_eq!(picture.resolution(), r);
                let e = luma_error(&picture, &input);
                assert!(e < 8.0, "{r} frame {t}: luma error {e}");
                decoded += 1;
            }
        }
        decoded
    }

    /// Throughput report, not a pass/fail check: run with
    /// `dev/run.sh cargo test --release -p v6cast-media --features vaapi -- --ignored throughput --nocapture`.
    #[test]
    #[ignore = "timing report; needs a VA-API GPU and a release build"]
    fn throughput_at_2560x1440() {
        if !hardware() {
            return;
        }
        let r = Resolution::new(2560, 1440);
        let config = EncoderConfig::new(r, VideoProfile::new(Preset::P1440, FrameRate::Fps60));
        let mut enc = VaapiEncoder::new(&config).unwrap();
        let mut dec = VaapiDecoder::new().unwrap();
        let frames: Vec<YuvFrame> = (0..60).map(|t| pattern(r, t)).collect();
        let (mut te, mut td) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        for f in &frames {
            let a = std::time::Instant::now();
            let out = enc.encode(f, 0).unwrap().unwrap();
            te += a.elapsed();
            let b = std::time::Instant::now();
            dec.decode(&out.data).unwrap();
            td += b.elapsed();
        }
        eprintln!(
            "VA-API 2560x1440: encode {:.2} ms/frame, decode with readback {:.2} ms/frame",
            te.as_secs_f64() * 1000.0 / 60.0,
            td.as_secs_f64() * 1000.0 / 60.0
        );
    }

    #[test]
    fn hardware_encode_decodes_in_every_preset() {
        if !hardware() {
            return;
        }
        let sources = [
            Resolution::new(1920, 1080),
            Resolution::new(2560, 1440),
            Resolution::new(2560, 1600),
            Resolution::new(2560, 1080),
        ];
        for preset in [Preset::P720, Preset::P1080, Preset::P1440, Preset::Native] {
            for rate in [FrameRate::Fps30, FrameRate::Fps60] {
                for src in sources {
                    let r = fit_preset(src, preset, Alignment::Two).unwrap();
                    // Cross-check: VA-API encode, openh264 decode.
                    let mut sw = SoftwareDecoder::new().unwrap();
                    let n = roundtrip(r, VideoProfile::new(preset, rate), &mut sw, 4);
                    assert_eq!(n, 4, "{r}: openh264 decoded {n}/4");
                }
            }
        }
    }

    #[test]
    fn hardware_decoder_reads_both_encoders_without_delay() {
        if !hardware() {
            return;
        }
        let r = Resolution::new(1920, 1080);
        let profile = VideoProfile::new(Preset::P1080, FrameRate::Fps60);
        let mut hw = VaapiDecoder::new().unwrap();
        // roundtrip() compares each picture with the frame just sent, so a
        // count of 8 also proves zero decode latency.
        let n = roundtrip(r, profile, &mut hw, 8);
        assert_eq!(n, 8, "VA-API decoded {n}/8 of its own stream");
        // openh264 stream into the VA-API decoder.
        let mut enc = SoftwareEncoder::new(&EncoderConfig::new(r, profile)).unwrap();
        let mut hw = VaapiDecoder::new().unwrap();
        let mut decoded = 0;
        for t in 0..8 {
            let input = pattern(r, t);
            let out = enc.encode(&input, 0).unwrap().unwrap();
            if let Some(p) = hw.decode(&out.data).unwrap() {
                assert!(luma_error(&p, &input) < 8.0);
                decoded += 1;
            }
        }
        assert_eq!(decoded, 8, "VA-API decoded {decoded}/8 openh264 frames");
    }

    #[test]
    fn requested_idr_and_bitrate_change() {
        if !hardware() {
            return;
        }
        let r = Resolution::new(1280, 720);
        let config = EncoderConfig::new(r, VideoProfile::new(Preset::P720, FrameRate::Fps60));
        let mut enc = VaapiEncoder::new(&config).unwrap();
        let mut keyframes = Vec::new();
        for t in 0..6 {
            if t == 3 {
                enc.request_keyframe();
            }
            if t == 4 {
                enc.set_bitrate(2_000_000).unwrap();
            }
            if enc.encode(&pattern(r, t), 0).unwrap().unwrap().keyframe {
                keyframes.push(t);
            }
        }
        assert_eq!(keyframes, vec![0, 3]);
        assert_eq!(enc.config().bitrate_bps, 2_000_000);
    }

    #[test]
    fn probe_without_device_is_none_not_a_panic() {
        // Runs everywhere: on machines without /dev/dri this must just be None.
        let _ = probe();
        let _ = render_node();
    }
}
