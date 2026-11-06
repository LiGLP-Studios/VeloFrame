# Architecture

## Principles

- **Engine only, no UI.** VeloFrame is embedded into applications. Hotkeys, settings and display belong to the host application; VeloFrame receives finished requests and returns results with measurements.
- **Two ways to embed.** As a Rust library (`veloframe::Engine`) or as a separate process (`veloframe.exe`, JSON lines over stdin/stdout). The process route protects the host application from crashes and works with any language.
- **One pipeline, swappable platforms.** The flow lives platform-independently in `core`. The platform crates only implement two interfaces:
  - `ScreenSource`: delivers an image of a desktop region (Desktop Duplication, later Windows Graphics Capture, ScreenCaptureKit, PipeWire).
  - `Desktop`: foreground window, mouse cursor, clipboard, timestamp.
- **Stay warm.** `warm_up()` opens capture up front (`veloframe.exe` does this on start), capture sources keep it open, buffers are reused.
- **Everything is measured.** Every answer contains per-step timings (`captureMs`, `encodeMs`, `writeMs`, `totalMs`, `clipboardMs`) and which source delivered the image. `bench/` measures encoders and the engine reproducibly.

## Screenshot flow

```
Request (Rust API or JSON line)
  └─ ScreenshotPipeline::take                      core/pipeline.rs
       ├─ Target: screen or foreground window (Desktop)
       ├─ ScreenSource::capture -> RgbImage        Win/duplication.rs
       │    └─ screen with the largest share of the target, D3D device on its GPU,
       │       BGRA -> RGB (4 pixels per step), rotation handled
       ├─ draw mouse cursor (optional)             Win/desktop.rs
       ├─ in parallel: clipboard (own thread)      Win/clipboard.rs
       │               encode PNG/JPEG in memory   core/codec (all cores)
       │               then write in one go        core/screenshot.rs
       └─ result with measurements; image stays in memory for `copy`
```

## Custom encoders (`core/src/codec`)

Both split the image into horizontal strips (one per CPU core) and join the parts into an ordinary file.

- **PNG:** For each row all five PNG filters are computed (with AVX2 when the CPU supports it), the best one wins. Then a custom DEFLATE encoder: zero runs become repeats, each strip gets its own optimal Huffman code and ends byte-aligned (sync flush). The strips are concatenated, the Adler-32 checksums combined mathematically.
- **JPEG:** Each strip is encoded separately (SIMD), the data streams are joined with restart markers (RST0–RST7). The file decodes pixel for pixel exactly like a single pass.
- **The game comes first:** The helper threads run below normal priority (`THREAD_PRIORITY_BELOW_NORMAL`) and only get CPU time that is currently free.

## Clip engine

See [CLIP-ENGINE.md](CLIP-ENGINE.md). In short: `Win/record.rs` (capture thread) → `Win/video.rs` (Desktop Duplication + GPU video processor BGRA→NV12) → queue → `Win/encode.rs` (hardware encoder via Media Foundation) → `core/replay.rs` (fixed ring, RAM or temporary file) → `core/mp4.rs` (MP4 without re-encoding). Modes and encoder plan (`core/clip.rs`), buffer and MP4 are platform independent.

Audio runs next to it on the same clock: `Win/wasapi.rs` (sources) → `Win/audio.rs` (one thread: timeline, mixing) using `core/audio.rs` (silence fill, drift correction, mixer) → `Win/aac.rs` (AAC via Media Foundation) → `core/replay.rs` (`AudioBuffer`, RAM) → `core/mp4.rs` (audio tracks, interleaving, edit list).

## Errors

`core/error.rs`: Every error has a fixed code (`no-screen`, `capture-lost`, …) for applications and a message that applications can show their users directly. The engine handles `capture-lost` (resolution change, UAC, fullscreen switch) itself: reopen once and try again.

## Next steps

See [PLAN-SCREENSHOT-WIN.md](PLAN-SCREENSHOT-WIN.md): Windows Graphics Capture as a second `ScreenSource` (phase 2), GPU processing (crop, HDR) via shaders, automatic source selection.
