# Hyprland DMA-BUF investigation: black capture texture in DesktopCaster

This document records a real-world bug report, the investigation, the root
cause, and the resolution, for the maintainer of the DesktopCaster GDExtension.
It was produced on a Hyprland (Wayland) session using an official Godot 4.7.2
build and a self-built copy of the extension's Linux PipeWire backend.

## The bug report

User environment:

- Compositor: Hyprland (Wayland), `xdg-desktop-portal-hyprland` as the screen
  capture backend
- Godot: official 4.7.2 stable, running a `TextureRect` scene
- Extension: DesktopCaster (this repo), Linux PipeWire/XDG Portal backend

Steps:

1. `DesktopCaster.start()` triggers the XDG portal "share your screen" dialog.
2. The user picks the monitor; the portal dialog closes.
3. DesktopCaster reports a successful start; the resulting texture size
   matches the desktop (1920x1080).
4. The `TextureRect` shows nothing. Saving a captured `Image` frame to PNG
   yields fully transparent black pixels (all `RGBA = 0x00000000`).

## Investigation

A standalone harness was built using the same `ashpd` + `pipewire-rs` crates
and the same portal flow as the extension, with listeners for
`state_changed`, `param_changed`, and `process`, logging the negotiated
format and the per-buffer memory characteristics.

### Reproducing the shipped behavior (connect with no format params)

```
NEGOTIATED: media_type=MediaType::Video media_subtype=MediaSubtype::Raw format=12 size=1920x1080
FRAME 60: type=DataType::DmaBuf flags=DataFlags(READABLE) fd=33 chunk_size=9
          chunk_offset=0 chunk_stride=7680 maxsize=0 data=NO
```

Key observations:

- A raw video format **is** negotiated: format id 12 (`SPA_VIDEO_FORMAT_BGRA`),
  1920x1080, row stride 7680 (= 1920 * 4).
- But every buffer arrives as `DataType::DmaBuf` with `data = NULL` and
  `maxsize = 0`. The dma-buf memory is never mapped, so `spa_data.data` is
  null and `pipewire_rs::Data::data()` returns `None`.
- The extension's process callback contains:

  ```rust
  if let Some(map) = data.data() {
      if map.len() == buffer_size { ... convert ... }
  }
  ```

  `data.data()` is `None` for every frame, so no frame is ever converted.
  The double buffer stays all zeros and the texture renders transparent
  black forever. Frames were arriving at ~60 FPS the whole time; they were
  silently discarded.

### Why the dma-bufs are not mapped

The stream is connected without any format parameter:

```rust
stream.connect(Direction::Input, Some(node_id),
    StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
    &mut [],   // <-- no EnumFormat param
);
```

With no format requested, the portal node (Hyprland) negotiates its default
format and, for zero-copy efficiency, delivers the frames as dma-buf file
descriptors. `MAP_BUFFERS` cannot map them (the data pointer stays null), so
the buffers are unreadable from CPU code. This is invisible to
`init()`/`start()`, which report success: nothing fails, frames just never
become readable.

This is the classic PipeWire screen-capture gotcha: screen capture nodes
prefer dma-buf delivery; a capture client that wants CPU-accessible memory
must explicitly request a raw format.

### Confirming the fix hypothesis (connect with an explicit raw format)

The same harness was run again, this time passing an `EnumFormat` param at
`stream.connect()` requesting raw video (`SPA_MEDIA_SUBTYPE_raw`) with
`BGRx`/`BGRA`/`RGBx`/`RGBA` as choices:

```
NEGOTIATED: media_type=MediaType::Video media_subtype=MediaSubtype::Raw format=12 size=1920x1080
FRAME 55: type=DataType::MemFd flags=DataFlags(READABLE | 0x8) fd=32
          chunk_size=8294400 chunk_offset=0 chunk_stride=7680 maxsize=8294400 data=yes
```

With the raw format request, Hyprland delivers regular `MemFd` shared-memory
buffers that PipeWire maps normally: `data.data()` is `Some`, `chunk_size`
is 8294400 (1920 * 1080 * 4) and the pixel bytes are non-zero. Frames are
CPU-readable.

## Root cause

The Linux PipeWire backend connected the capture stream **without requesting a
format**, so the compositor delivered unmapped **DMA-BUF** buffers whose data
pointer is null. The `data.data()` gate in the process callback silently
dropped every frame, leaving the capture texture at its initial all-zero
state. The extension reported success because nothing in the startup path can
observe this.

Additional latent issues found along the way:

1. The buffer size gate compared `data.data().len()` (which is `maxsize`)
   against `width * height * 4`. With padded strides this would also silently
   drop frames even for raw formats.
2. The conversion assumed the negotiated layout was `BGRx`. The compositor
   actually negotiates `BGRA` (identical channel swap, so harmless here, but
   `RGBx`/`RGBA` deliveries would have produced wrong colors).
3. The texture size came from `DisplayServer.screen_get_size()`; the stream
   negotiates its own size (monitor geometry, and a different size when a
   window is selected). Any mismatch is silently dropped by the same gate.

## Resolution

1. **Request raw, CPU-readable formats at connect time.** Build an
   `EnumFormat` pod (`MediaType::Video`, `MediaSubtype::Raw`, format choice
   `BGRx | BGRA | RGBx | RGBA`) and pass it to `stream.connect()` so the
   compositor delivers mapped `MemFd` buffers instead of dma-bufs. Implemented
   in `rust/src/backends/linux_pipewire.rs` (`build_raw_format_params`).
2. **Track the negotiated format.** A `param_changed` listener parses the
   `SPA_PARAM_Format` param into a `VideoInfoRaw` and stores the actual
   format/size (`ListenerState`). Conversion dispatches on the negotiated
   format id (`convert_row`), so `BGRx`/`BGRA` and `RGBx`/`RGBA` are both
   handled.
3. **Honor the buffer stride.** `convert_to_rgba` converts scanline by
   scanline using `spa_chunk.stride` and the valid byte count
   (`spa_chunk.size`), so padded rows no longer break conversion.
4. **Propagate the stream size to Godot.** Frames carry their dimensions
   through the mpsc channel; when they differ from the Godot-side buffers,
   the capture worker pauses (`CaptureControl::pending_size`) and the Godot
   main thread reallocates front/back buffers and recreates the
   `Image`/`ImageTexture` at the new size (`desktop_caster.rs::process`).
   This makes window capture and scaled monitors work too.
5. **Surface failures instead of black frames.** Negotiation success is
   printed via stderr; a 3-second watchdog reports when no raw format was
   negotiated (compositors that only offer dma-buf); repeated unmapped-buffer
   delivery is reported once.

### Files changed

- `rust/src/backends/linux_pipewire.rs` — format negotiation, format/stride
  aware conversion, size-change reporting, error surfacing
- `rust/src/backends/mod.rs` — `CaptureBackend::take_size_change()`
- `rust/src/backends/linux.rs` — delegate `take_size_change`
- `rust/src/capture_thread.rs` — `pending_size` handling in `CaptureControl`
  and the capture loop
- `rust/src/desktop_caster.rs` — apply stream size changes on the main thread

## Verification

- Harness in "no params" mode: dma-buf, `data=NO`, no readable frames
  (reproduces the bug).
- Harness in "raw format" mode: MemFd, `data=yes`, `chunk_size=8294400`,
  non-zero pixels.
- Full end-to-end in Godot 4.7.2 with the fixed `.so`, running the user's
  scene: `Negotiated stream format: 12 at 1920x1080`, then a 1920x1080
  frame sampled 32400/32400 pixels with `alpha = 255`, and the saved PNG
  contains the actual desktop content (dark theme, multiple distinct colors).

## Notes for other compositors

- Hyprland honors a raw-format request and switches to `MemFd` delivery.
- Compositors that expose only dma-buf formats will refuse the raw format;
  negotiation then never completes and the watchdog prints a clear message.
  A future follow-up could mmap the dma-buf fd (`fstat` gives the buffer
  size) as a fallback for such compositors.
- The `chunk_size` observed for the unmapped dma-buf buffers (9) is not a
  byte count; do not rely on it when the buffer is not mapped.
