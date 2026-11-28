//! PipeWire side of audio capture: node enumeration and capture streams.
//!
//! * System loopback: one capture stream with `stream.capture.sink = true`,
//!   which PipeWire connects to the monitor of the default sink.
//! * Application: one capture stream per playback stream of the app, each with
//!   `target.object` set to the node's serial; PipeWire links the app's output
//!   ports to it, as `pw-record --target` does.
//!
//! Everything runs on one thread with its own main loop; chunks leave through
//! a bounded channel and are dropped when the consumer is slow.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::types::ObjectType;

use super::{AudioChunk, AudioError, AudioNode, AudioPlan, CHANNELS, SAMPLE_RATE};
use crate::capture::FrameClock;

fn pw_err(e: pw::Error) -> AudioError {
    AudioError::PipeWire(e.to_string())
}

fn node_from_props(id: u32, props: &spa::utils::dict::DictRef) -> Option<AudioNode> {
    let class = props.get(*pw::keys::MEDIA_CLASS)?;
    if !class.contains("Audio") {
        return None;
    }
    let text = |key: &str| props.get(key).map(str::to_owned);
    Some(AudioNode {
        id,
        serial: props
            .get(*pw::keys::OBJECT_SERIAL)
            .and_then(|s| s.parse().ok()),
        media_class: class.to_owned(),
        node_name: text(*pw::keys::NODE_NAME),
        app_name: text(*pw::keys::APP_NAME),
        app_id: text(*pw::keys::APP_ID),
        process_id: props
            .get(*pw::keys::APP_PROCESS_ID)
            .and_then(|s| s.parse().ok()),
        process_binary: text(*pw::keys::APP_PROCESS_BINARY),
    })
}

/// Audio nodes currently in the PipeWire graph (one registry roundtrip).
pub(super) fn audio_nodes() -> Result<Vec<AudioNode>, AudioError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_err)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_err)?;
    let core = context.connect_rc(None).map_err(pw_err)?;
    let registry = core.get_registry_rc().map_err(pw_err)?;
    let found = Rc::new(RefCell::new(Vec::new()));

    let sink = Rc::clone(&found);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Node {
                return;
            }
            if let Some(node) = global.props.and_then(|p| node_from_props(global.id, p)) {
                sink.borrow_mut().push(node);
            }
        })
        .register();

    let pending = core.sync(0).map_err(pw_err)?;
    let quit = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                quit.quit();
            }
        })
        .register();
    mainloop.run();
    let nodes = found.borrow().clone();
    Ok(nodes)
}

pub(super) struct AudioSession {
    chunks: Receiver<AudioChunk>,
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSession {
    pub(super) fn start(plan: &AudioPlan) -> Result<Self, AudioError> {
        let targets: Vec<Option<u64>> = match plan {
            AudioPlan::SystemLoopback => vec![None],
            AudioPlan::Application { nodes } => nodes.iter().map(|n| n.serial).collect(),
            AudioPlan::Silent(_) => return Err(AudioError::PipeWire("silent plan".into())),
        };
        if targets.iter().skip(1).any(Option::is_none)
            || (targets.len() > 1 && targets[0].is_none())
        {
            return Err(AudioError::PipeWire(
                "application stream without serial".into(),
            ));
        }
        let (chunk_tx, chunks) = mpsc::sync_channel(64);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("v6cast-audio".into())
            .spawn(move || {
                if let Err(e) = run(targets, chunk_tx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| AudioError::PipeWire(e.to_string()))?;
        let stop = ready_rx
            .recv()
            .map_err(|_| AudioError::PipeWire("audio thread ended".into()))??;
        Ok(Self {
            chunks,
            stop,
            thread: Some(thread),
        })
    }

    pub(super) fn recv_timeout(&self, timeout: Duration) -> Option<AudioChunk> {
        // Timeout and a stopped stream both mean "no audio right now".
        self.chunks.recv_timeout(timeout).ok()
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct StreamState {
    index: usize,
    clock: Rc<RefCell<FrameClock>>,
    chunks: SyncSender<AudioChunk>,
}

fn format_pod() -> Result<Vec<u8>, AudioError> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|e| AudioError::PipeWire(format!("cannot build format pod: {e:?}")))
}

type Ready = Result<pw::channel::Sender<()>, AudioError>;

fn run(
    targets: Vec<Option<u64>>,
    chunks: SyncSender<AudioChunk>,
    ready: &mpsc::Sender<Ready>,
) -> Result<(), AudioError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_err)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_err)?;
    let core = context.connect_rc(None).map_err(pw_err)?;
    let clock = Rc::new(RefCell::new(FrameClock::default()));
    let format = format_pod()?;

    let mut streams = Vec::new();
    for (index, target) in targets.into_iter().enumerate() {
        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "v6cast-audio",
        };
        match target {
            None => props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true"),
            Some(serial) => props.insert(*pw::keys::TARGET_OBJECT, serial.to_string()),
        }
        let stream =
            pw::stream::StreamRc::new(core.clone(), "v6cast-audio", props).map_err(pw_err)?;
        let state = StreamState {
            index,
            clock: Rc::clone(&clock),
            chunks: chunks.clone(),
        };
        let listener = stream
            .add_local_listener_with_user_data(state)
            .process(|stream, state| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let now = Instant::now();
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let (offset, len) = {
                    let chunk = data.chunk();
                    (chunk.offset() as usize, chunk.size() as usize)
                };
                let Some(bytes) = data.data() else { return };
                let Some(bytes) = bytes.get(offset..offset.saturating_add(len)) else {
                    return;
                };
                let samples: Vec<f32> = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect();
                if samples.is_empty() {
                    return;
                }
                let chunk = AudioChunk {
                    source: state.index,
                    samples,
                    timestamp_us: state.clock.borrow_mut().stamp(now),
                };
                // Full queue: the consumer is behind, drop this chunk.
                let _ = state.chunks.try_send(chunk);
            })
            .register()
            .map_err(pw_err)?;
        let mut params = [spa::pod::Pod::from_bytes(&format)
            .ok_or(AudioError::PipeWire("bad format pod".into()))?];
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(pw_err)?;
        streams.push((stream, listener));
    }

    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let quit = mainloop.clone();
    let _stop = stop_rx.attach(mainloop.loop_(), move |()| quit.quit());
    ready
        .send(Ok(stop_tx))
        .map_err(|_| AudioError::PipeWire("caller went away".into()))?;
    mainloop.run();
    drop(streams);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_format_pod_serializes() {
        let pod = format_pod().unwrap();
        assert!(spa::pod::Pod::from_bytes(&pod).is_some());
    }

    #[test]
    fn enumeration_without_a_pipewire_daemon_is_an_error_not_a_panic() {
        // In the dev container there is no PipeWire daemon; on a desktop this
        // lists real nodes. Either way it must return.
        match audio_nodes() {
            Ok(nodes) => assert!(nodes.iter().all(|n| n.media_class.contains("Audio"))),
            Err(e) => eprintln!("no PipeWire here: {e}"),
        }
    }
}
