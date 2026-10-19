use crate::backends::{CaptureBackend, CaptureError};
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use std::sync::{LazyLock, mpsc};
use std::time::Duration;

static TOKIO_RT: LazyLock<Result<tokio::runtime::Runtime, String>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("pw-tokio")
        .build()
        .map_err(|e| format!("Failed to create Tokio runtime: {e}"))
});

fn tokio_runtime() -> Result<&'static tokio::runtime::Runtime, &'static str> {
    TOKIO_RT.as_ref().map_err(|e| e.as_str())
}

fn close_session(session: Session<Screencast>) {
    if let Ok(rt) = tokio_runtime() {
        let _ = rt.block_on(async {
            let _ = tokio::time::timeout(Duration::from_secs(1), session.close()).await;
        });
    }
}

// pipewire-rs 0.9+ uses *Rc variants (shared-ownership) instead of unique ones.
// `spa` is re-exported directly via `pipewire::spa`.
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
// Explicitly import macro to avoid shadowing by the `pipewire::properties` module.
use pipewire::properties::properties;
use pipewire::spa::buffer::{Chunk, ChunkFlags};
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Pod, Value};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{StreamFlags, StreamRc};

/// Status message to sync the worker thread startup with `init()`.
enum WorkerStartup {
    Ready,
    Failed(String),
}

/// Serialize an `EnumFormat` pod requesting raw video in the 4-byte BGR
/// layouts the frame conversion understands.
///
/// The pod deliberately carries no `VideoModifier` property: PipeWire only
/// negotiates DMA-BUF when modifiers are part of the format, so leaving them
/// out makes the producer fall back to shared memory (`MemFd`/`MemPtr`) that
/// `MAP_BUFFERS` can map for CPU reads.
fn build_raw_format_params() -> Result<Vec<u8>, String> {
    let obj = pipewire::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pipewire::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pipewire::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        // In a SPA `Choice::Enum` the first value is the preferred default and
        // the remaining values are the alternatives, hence BGRx appears twice.
        pipewire::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
    );
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|error| format!("{error:?}"))
}

/// Extract the frame size from a negotiated `Format` param, provided it is one
/// of the raw layouts requested by `build_raw_format_params`.
fn parse_negotiated_size(param: &Pod) -> Option<(u32, u32)> {
    let (media_type, media_subtype) = format_utils::parse_format(param).ok()?;
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return None;
    }
    let mut info = VideoInfoRaw::new();
    info.parse(param).ok()?;
    if info.format() != VideoFormat::BGRx && info.format() != VideoFormat::BGRA {
        return None;
    }
    let size = info.size();
    (size.width > 0 && size.height > 0).then_some((size.width, size.height))
}

/// The `spa_chunk` fields describing where the valid pixels live in a buffer.
#[derive(Clone, Copy)]
struct FrameChunk {
    offset: u32,
    size: u32,
    stride: i32,
    corrupted: bool,
}

impl From<&Chunk> for FrameChunk {
    fn from(chunk: &Chunk) -> Self {
        Self {
            offset: chunk.offset(),
            size: chunk.size(),
            stride: chunk.stride(),
            corrupted: chunk.flags().contains(ChunkFlags::CORRUPTED),
        }
    }
}

/// Convert a mapped BGRx/BGRA buffer into tightly packed RGBA8.
///
/// Honors the chunk offset, size and row stride, so padded rows work. Returns
/// `None` for frames that do not contain `height` complete rows, so a partial
/// or corrupted buffer is dropped rather than shown as a black frame.
fn convert_to_rgba(map: &[u8], chunk: FrameChunk, width: u32, height: u32) -> Option<Vec<u8>> {
    if chunk.corrupted || height == 0 {
        return None;
    }
    let width = width as usize;
    let height = height as usize;
    let row_bytes = width.checked_mul(4)?;
    // A zero stride means tightly packed rows. Negative (bottom-up) strides
    // are not produced for screen capture and are rejected.
    let stride = match chunk.stride {
        0 => row_bytes,
        stride => usize::try_from(stride).ok()?,
    };
    if stride < row_bytes {
        return None;
    }

    let start = chunk.offset as usize;
    let end = start.checked_add(chunk.size as usize)?.min(map.len());
    let frame = map.get(start..end)?;
    // The last row does not need trailing stride padding.
    let needed = stride.checked_mul(height - 1)?.checked_add(row_bytes)?;
    if frame.len() < needed {
        return None;
    }

    let mut out = vec![0u8; row_bytes.checked_mul(height)?];
    for (src_row, dst_row) in frame.chunks(stride).zip(out.chunks_exact_mut(row_bytes)) {
        for (d, s) in dst_row.chunks_exact_mut(4).zip(src_row[..row_bytes].chunks_exact(4)) {
            d[0] = s[2]; // R
            d[1] = s[1]; // G
            d[2] = s[0]; // B
            d[3] = 255; // A
        }
    }
    Some(out)
}

/// A converted RGBA8 frame and the negotiated size it was produced at.
struct Frame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

pub struct PipeWireCaptureBackend {
    rx: mpsc::Receiver<Frame>,
    pw_thread: Option<std::thread::JoinHandle<()>>,
    quit_sender: Option<pipewire::channel::Sender<()>>,
    /// Size of the last frame dropped because the caller's buffer did not
    /// match it. Drained by the capture worker via `take_size_change`.
    size_change: Option<(u32, u32)>,
    session: Option<Session<Screencast>>,
}

impl CaptureBackend for PipeWireCaptureBackend {
    // The frame size comes from stream negotiation (the selected monitor or
    // window), not from the requested size, so the arguments are unused.
    fn init(_width: u32, _height: u32) -> Result<Self, String> {
        let rt = tokio_runtime().map_err(|e| format!("Tokio runtime error: {e}"))?;

        let (pw_fd, node_id, session) = rt
            .block_on(async {
                let proxy = Screencast::new().await?;
                let session = proxy.create_session(Default::default()).await?;

                proxy
                    .select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_sources(SourceType::Monitor | SourceType::Window)
                            .set_cursor_mode(CursorMode::Embedded),
                    )
                    .await?;

                let response = proxy.start(&session, None, Default::default()).await?.response()?;
                let stream = response.streams().first().ok_or(ashpd::Error::NoResponse)?;
                let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;

                Ok::<_, ashpd::Error>((fd, stream.pipe_wire_node_id(), session))
            })
            .map_err(|e| format!("XDG Portal error: {e}"))?;

        let (tx, rx) = mpsc::sync_channel::<Frame>(1);
        let (quit_sender, quit_receiver) = pipewire::channel::channel::<()>();
        let (startup_tx, startup_rx) = mpsc::channel::<WorkerStartup>();

        let pw_thread = std::thread::Builder::new()
            .name("pw-capture".into())
            .spawn(move || {
                // Report errors via `startup_tx` instead of panicking on this thread.
                macro_rules! try_setup {
                    ($expr:expr, $context:literal) => {
                        match $expr {
                            Ok(value) => value,
                            Err(error) => {
                                let _ = startup_tx.send(WorkerStartup::Failed(format!(
                                    "{}: {}",
                                    $context, error
                                )));
                                return;
                            }
                        }
                    };
                }

                pipewire::init();
                let mainloop =
                    try_setup!(MainLoopRc::new(None), "Failed to create PipeWire mainloop");
                let context = try_setup!(
                    ContextRc::new(&mainloop, None),
                    "Failed to create PipeWire context"
                );

                // `connect_fd_rc` takes ownership of the fd.
                let core = try_setup!(
                    context.connect_fd_rc(pw_fd, None),
                    "Failed to connect to PipeWire via portal FD"
                );

                let props = properties! {
                    *pipewire::keys::MEDIA_TYPE => "Video",
                    *pipewire::keys::MEDIA_CATEGORY => "Capture",
                    *pipewire::keys::MEDIA_ROLE => "Screen",
                };

                let stream = try_setup!(
                    StreamRc::new(core, "gd-capture", props),
                    "Failed to create PipeWire stream"
                );

                let loop_clone = mainloop.clone();
                let _receiver = quit_receiver.attach(&mainloop.loop_(), move |_| {
                    loop_clone.quit();
                });

                let _listener = try_setup!(
                    stream
                        .add_local_listener_with_user_data(None::<(u32, u32)>)
                        .param_changed(|_, negotiated_size, id, param| {
                            if id != ParamType::Format.as_raw() {
                                return;
                            }
                            *negotiated_size = param.and_then(parse_negotiated_size);
                        })
                        .process(move |stream, negotiated_size| {
                            // Buffers only carry pixels once a format is negotiated.
                            let Some((frame_width, frame_height)) = *negotiated_size else {
                                return;
                            };
                            if let Some(mut buffer) = stream.dequeue_buffer() {
                                let datas = buffer.datas_mut();
                                if let Some(data) = datas.first_mut() {
                                    let chunk = FrameChunk::from(data.chunk());
                                    // Renamed to `data()` in pipewire 0.9.
                                    if let Some(map) = data.data() {
                                        if let Some(pixels) =
                                            convert_to_rgba(map, chunk, frame_width, frame_height)
                                        {
                                            let _ = tx.try_send(Frame {
                                                width: frame_width,
                                                height: frame_height,
                                                pixels,
                                            });
                                        }
                                    }
                                }
                                // pipewire 0.9+: Buffer auto-queues back on Drop.
                            }
                        })
                        .register(),
                    "Failed to register PipeWire stream listener"
                );

                // Without a format request the producer's own format (with
                // DMA-BUF modifiers) is accepted as-is and buffers arrive as
                // unmapped dma-bufs, so `data.data()` is `None` for every frame.
                let format_bytes = try_setup!(
                    build_raw_format_params(),
                    "Failed to serialize capture format params"
                );
                let format_pod = try_setup!(
                    Pod::from_bytes(&format_bytes).ok_or("invalid pod"),
                    "Failed to parse capture format params"
                );
                let mut params = [format_pod];

                // No RT_PROCESS: `process` must run on this mainloop thread,
                // like `param_changed`, because both share the listener state.
                // This thread is dedicated to the stream, and the per-frame
                // conversion allocates, so a realtime data thread buys nothing.
                let connect_result = stream.connect(
                    Direction::Input,
                    Some(node_id),
                    StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                    &mut params,
                );
                if let Err(error) = connect_result {
                    let _ = startup_tx.send(WorkerStartup::Failed(format!(
                        "Failed to connect PipeWire stream: {error}"
                    )));
                    return;
                }

                let _ = startup_tx.send(WorkerStartup::Ready);

                mainloop.run();
                // Do not call `pipewire::deinit()` here: `pipewire::init()` is
                // guarded by a process-global `Once` in pipewire-rs, so tearing
                // down libpipewire on thread exit prevents re-initialization on
                // subsequent capture sessions.
            })
            .map_err(|e| format!("Failed to spawn PipeWire thread: {e}"))?;

        // Wait for worker thread initialization outcome.
        match startup_rx.recv() {
            Ok(WorkerStartup::Ready) => {}
            Ok(WorkerStartup::Failed(reason)) => {
                let _ = pw_thread.join();
                close_session(session);
                return Err(reason);
            }
            Err(_) => {
                let join_result = pw_thread.join();
                let panic_detail = match join_result {
                    Err(payload) => describe_panic_payload(&payload),
                    Ok(()) => {
                        "worker thread exited before reporting startup status".to_owned()
                    }
                };
                close_session(session);
                return Err(format!("PipeWire worker thread failed to start: {panic_detail}"));
            }
        }

        Ok(Self {
            rx,
            pw_thread: Some(pw_thread),
            quit_sender: Some(quit_sender),
            size_change: None,
            session: Some(session),
        })
    }

    fn capture_frame(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<bool, CaptureError> {
        let result = if timeout_ms == 0 {
            self.rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => CaptureError::Recoverable("No frame".into()),
                mpsc::TryRecvError::Disconnected => {
                    CaptureError::Fatal("PipeWire disconnected".into())
                }
            })
        } else {
            self.rx
                .recv_timeout(Duration::from_millis(timeout_ms as u64))
                .map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => CaptureError::Recoverable("Timeout".into()),
                    mpsc::RecvTimeoutError::Disconnected => {
                        CaptureError::Fatal("PipeWire disconnected".into())
                    }
                })
        };

        match result {
            Ok(frame) => {
                let expected_len = (frame.width as usize)
                    .checked_mul(frame.height as usize)
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or_else(|| {
                        CaptureError::Fatal("Capture dimensions overflow usize".to_owned())
                    })?;
                if frame.pixels.len() != expected_len {
                    return Err(CaptureError::Reinitialize(format!(
                        "PipeWire frame has {} bytes; expected {expected_len}",
                        frame.pixels.len()
                    )));
                }
                // Godot-side buffers are stale; drop the frame and let the
                // capture worker ask Godot to reallocate.
                if buffer.len() != expected_len {
                    self.size_change = Some((frame.width, frame.height));
                    return Ok(false);
                }
                buffer.copy_from_slice(&frame.pixels);
                Ok(true)
            }
            Err(CaptureError::Recoverable(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn take_size_change(&mut self) -> Option<(u32, u32)> {
        self.size_change.take()
    }

    fn destroy(&mut self) {
        if let Some(sender) = self.quit_sender.take() {
            let _ = sender.send(());
        }
        if let Some(thread) = self.pw_thread.take() {
            if let Err(payload) = thread.join() {
                eprintln!(
                    "[DesktopCapture] PipeWire worker thread panicked during shutdown: {}",
                    describe_panic_payload(&payload)
                );
            }
        }
        if let Some(session) = self.session.take() {
            close_session(session);
        }
    }
}

impl Drop for PipeWireCaptureBackend {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Extracts a human-readable panic message from the thread join payload.
fn describe_panic_payload(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameChunk, convert_to_rgba};

    /// A 2x2 BGRx frame whose rows are `stride` bytes apart (padding = 0xEE),
    /// preceded by `offset` bytes of garbage.
    fn frame(stride: usize, offset: usize) -> Vec<u8> {
        let mut map = vec![0xEE; offset];
        for row in 0..2u8 {
            let mut line = vec![0xEE; stride];
            for col in 0..2u8 {
                let px = row * 2 + col;
                line[col as usize * 4..col as usize * 4 + 4].copy_from_slice(&[px, 10 + px, 20 + px, 0]);
            }
            map.extend_from_slice(&line);
        }
        map
    }

    fn chunk(offset: usize, size: usize, stride: i32) -> FrameChunk {
        FrameChunk {
            offset: offset as u32,
            size: size as u32,
            stride,
            corrupted: false,
        }
    }

    const EXPECTED: [u8; 16] = [
        20, 10, 0, 255, 21, 11, 1, 255, //
        22, 12, 2, 255, 23, 13, 3, 255,
    ];

    #[test]
    fn swizzles_tightly_packed_frames() {
        let map = frame(8, 0);
        assert_eq!(convert_to_rgba(&map, chunk(0, 16, 8), 2, 2).unwrap(), EXPECTED);
    }

    #[test]
    fn zero_stride_means_tightly_packed() {
        let map = frame(8, 0);
        assert_eq!(convert_to_rgba(&map, chunk(0, 16, 0), 2, 2).unwrap(), EXPECTED);
    }

    #[test]
    fn skips_row_padding_and_chunk_offset() {
        let map = frame(12, 4);
        assert_eq!(convert_to_rgba(&map, chunk(4, 24, 12), 2, 2).unwrap(), EXPECTED);
    }

    #[test]
    fn last_row_needs_no_trailing_padding() {
        let map = frame(12, 0);
        assert_eq!(convert_to_rgba(&map, chunk(0, 20, 12), 2, 2).unwrap(), EXPECTED);
    }

    #[test]
    fn drops_incomplete_frames() {
        let map = frame(8, 0);
        assert!(convert_to_rgba(&map, chunk(0, 15, 8), 2, 2).is_none());
        // A chunk claiming more bytes than are mapped is clamped to the map.
        assert!(convert_to_rgba(&map[..12], chunk(0, 16, 8), 2, 2).is_none());
        assert!(convert_to_rgba(&map, chunk(20, 16, 8), 2, 2).is_none());
    }

    #[test]
    fn rejects_unusable_strides_and_corrupted_chunks() {
        let map = frame(8, 0);
        assert!(convert_to_rgba(&map, chunk(0, 16, -8), 2, 2).is_none());
        assert!(convert_to_rgba(&map, chunk(0, 16, 4), 2, 2).is_none());
        let corrupted = FrameChunk {
            corrupted: true,
            ..chunk(0, 16, 8)
        };
        assert!(convert_to_rgba(&map, corrupted, 2, 2).is_none());
    }
}
