# Plan: Screenshot engine for Windows

Goal: VeloFrame captures screenshots of games on Windows, faster and cleaner than the current ClipHub helper, without the game noticing. The video engine builds on the same pipeline.

## Targets

| Metric | Target |
| --- | --- |
| Key press to finished file, 2560×1440, JPEG | under 50 ms |
| Key press to finished file, 2560×1440, PNG | under 150 ms |
| FPS loss in the game during the screenshot | not measurable (under 1 %) |
| RAM when idle | under 30 MB |
| CPU when idle | close to 0 % |

The benchmark tool (phase 6) checks these values. They are the proof that VeloFrame is an engine.

## Screenshot flow

```
Request from the host application (e.g. ClipHub)
        │
engine/commands ──► core/pipeline
                        │
        Win: choose capture path
        ├─ WGC (window or monitor)             ← main path
        └─ Desktop Duplication (monitor)       ← fallback
                        │
        D3D11 texture on the GPU (BGRA8, or FP16 for HDR)
                        │
        GPU shader: crop, HDR → SDR, color format
                        │
        Copy to the CPU once (staging texture)
                        │
        Worker thread: encode PNG / JPEG / WebP
                        │
        Save file, optionally clipboard
                        │
        Answer to ClipHub: path, size, per-step timings
```

Per-step timings are included in every answer, so you can always see where time is lost.

## Modules

| File | Purpose |
| --- | --- |
| `core/frame.rs` | Image types: size, pixel format (BGRA8, RGBA16F), color space (SDR, HDR), timestamp |
| `core/capture.rs` | "Capture source" interface: open, get latest image, close |
| `core/pipeline.rs` | Flow from capture to file, measures timings |
| `core/screenshot.rs` | Encoding and saving, file names, clipboard interface |
| `core/ipc.rs` | JSON protocol to ClipHub (one line per message) |
| `core/config.rs` | Format, quality, location, capture path, hotkey |
| `core/error.rs` | Error types with clear messages for ClipHub |
| `Win/capture.rs` | Windows Graphics Capture |
| `Win/duplication.rs` | DXGI Desktop Duplication |
| `Win/gpu.rs` *(new)* | D3D11 device, shaders for crop, HDR → SDR, format conversion |
| `Win/clipboard.rs` *(new)* | Put the image on the Windows clipboard |
| `Win/system.rs` | System check: Windows version, GPU, monitors, HDR, DPI |
| `engine/commands.rs` | Commands `system-check`, `screenshot`, `copy`, `ping` |
| `bench/` *(new)* | Benchmark tool for the targets |

## Phases

### Phase 0: Foundation ✅ done
- Define types in `core` (frame, rect, pixel format, errors, config).
- Adopt the IPC protocol so that it is **compatible with the current ClipHub helper**. ClipHub can then start VeloFrame without changes to its Electron code.
- Main loop in `engine/main.rs`: `ready` event, read commands, answer.

**Done when:** `ping` and `system-check` work from ClipHub.

### Phase 1: Adopt Desktop Duplication ✅ done

Measured (release, 1920×1080, capture warm): PNG 29 ms, JPEG 47 ms total, capture itself 4–5 ms. Also works with two monitors on two different GPUs. Values in the [README](../README.md#measurements).
- Take over the working code from `cliphub/1.0.0/native/capture` and split it into `core` and `Win`: find monitors, rotation, multiple monitors, DPI, crop to the active window, PNG/JPEG, clipboard.
- Bring along the existing tests.

**Done when:** VeloFrame can do everything the ClipHub helper can do today (same commands, same results). This is the first milestone: ClipHub can switch over.

### Intermediate step: Speed ✅ done
- **Pre-warming:** `warm_up()` / command `warm-up` opens all screens up front; `veloframe.exe` does this right after start. The first screenshot is as fast as any other (287 ms before, now about 20–30 ms).
- **Custom encoders on all cores** (`core/src/codec`): PNG with a custom DEFLATE (strips with sync flush, own Huffman code per strip, AVX2 filters), JPEG with restart markers. The results are ordinary files; JPEG decodes pixel-identical to a single pass, PNG is verified lossless.
- **The game comes first:** The encoder helpers run at lower priority and only use CPU time that is currently free.
- **Benchmark tool** `bench/` (`veloframe-bench encode …` / `capture`).

This brings phase 4 (encoding) forward for PNG and JPEG; WebP and metadata remain open there.

### Phase 2: Windows Graphics Capture (WGC)

**Goal:** A second capture source next to Desktop Duplication that captures **a single window** instead of a region of the screen.

**Why:** Desktop Duplication always captures the whole screen; "window" only means "the region where the window is". If something lies on top (notification, chat window, volume indicator), it ends up in the image. WGC instead gets the pixels of the window itself, directly from the window manager (DWM):
- Nothing from other windows in the image, even if the game is partly covered or extends past the screen edge.
- Windows on any screen and any GPU, without looking up the screen.
- Mouse cursor on/off directly via the API instead of drawing it ourselves.
- Can deliver FP16 images: the basis for real HDR screenshots in phase 3.

**Building blocks** (`Win/src/capture.rs`, implemented as a `ScreenSource`, the pipeline stays unchanged):
1. **Choose the target:** `GraphicsCaptureItem` for a window (`CreateForWindow`, HWND) or a monitor (`CreateForMonitor`).
2. **D3D11 device** on the GPU that shows the window, wrapped as a WinRT `IDirect3DDevice`.
3. **Frame pool** `Direct3D11CaptureFramePool::CreateFreeThreaded` with 1–2 frames: Windows puts the latest frame in it without VeloFrame needing its own thread.
4. **Session** `GraphicsCaptureSession`: mouse cursor on/off (`IsCursorCaptureEnabled`), yellow border off (`IsBorderRequired = false`, Windows 11 only).
5. **Screenshot:** take the latest frame from the pool → `ID3D11Texture2D` → staging copy → RGB as before → encoder.
6. **Edge cases:** window size changes (adjust the pool with `Recreate`), window minimized (no image → fallback), window closed (`Closed` event → end session), game switches to exclusive fullscreen (detect black frames → Desktop Duplication).

**Keep warm, but mind Windows 10:** An open session delivers the screenshot immediately, otherwise starting it costs about 50–150 ms. On Windows 10, however, Windows shows a **yellow border** around the window during every WGC session; it can only be turned off from Windows 11 (the test PC: Windows 10, build 19045, no borderless WGC according to `system-check`). Therefore:
- Windows 11: keep the session for the foreground game open (detect foreground changes via `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`).
- Windows 10: open WGC only for the screenshot and close it right away (border only visible briefly), or stay with Desktop Duplication entirely if the host application wants that.

**New in the protocol:** optional argument `source` (`"auto"`, `"window-capture"`, `"desktop-duplication"`); the answer names in `source` who delivered the image. Without it everything stays as before.

**Done when:**
- A screenshot with `area: "window"` only shows the game window, even if another window lies on top.
- Resizing, minimizing, closing and fullscreen switches produce no error but fall back cleanly to Desktop Duplication.
- `veloframe-bench` reports the timings for WGC cold and warm.

**Risks:** yellow border on Windows 10 (see above); black frames with exclusive fullscreen or protected content (DRM videos); with two GPUs Windows copies the image between the cards internally, which costs some time.

### Phase 3: GPU processing
- Custom compute shaders (HLSL) for crop and color format. The image stays on the GPU until the very end.
- **HDR → SDR:** WGC delivers an FP16 image on HDR monitors. The shader converts it using the SDR white level from the Windows settings, so screenshots do not look washed out.
- Only the finished, cropped image is copied to the CPU once.

**Done when:** screenshots on an HDR monitor look like they do on screen.

### Phase 4: Encoding and output (PNG/JPEG brought forward ✅)
- ~~Encode PNG and JPEG fast~~: done in the "Speed" step.
- WebP in addition for the ClipHub upload (much smaller than PNG).
- Metadata: time, game name (from the window), monitor.

**Done when:** the targets for JPEG and PNG are reached.

### Phase 5: Automatic selection and fallback
- VeloFrame chooses itself: WGC for game windows, Desktop Duplication for whole monitors or when WGC is not available.
- If one path fails, it automatically tries the other. ClipHub only gets an error if both fail.
- `system-check` reports which paths work on the system.

**Done when:** no screenshot fails without reason anymore.

### Phase 6: Benchmark and polish
- `bench/`: measures per-step timings, RAM, CPU, GPU and FPS loss in the game and compares with the old helper.
- Record the results, e.g. for the website.

## Open decisions

1. **Minimum version:** Windows 10 1903 (WGC available), or older systems with Desktop Duplication only?
2. **Keep capture warm:** always, only when a game is detected, or start on key press?
3. ~~**Hotkey:** does it stay in ClipHub or does VeloFrame take it over?~~ **Decided:** VeloFrame is only the engine, without UI. Hotkeys belong to the host application.
4. **HDR:** save only SDR screenshots, or additionally a real HDR file (JPEG XR / AVIF)?
5. **WebP:** right in 1.0 or later?
6. **Shaders:** compile at runtime via the `d3dcompiler_47.dll` included in Windows (simple), or precompile at build time (faster start, needs the Windows SDK)?
7. **Toolchain:** `x86_64-pc-windows-gnu` is installed. For D3D and the Windows SDK, `x86_64-pc-windows-msvc` is the usual way. Switch or stay with GNU?

## Risks

| Risk | Mitigation |
| --- | --- |
| Games in exclusive fullscreen deliver black frames via WGC | Fall back to Desktop Duplication, notify the user |
| Laptops with two GPUs: capture on the wrong GPU | Always create the device on the adapter the monitor is connected to |
| Anti-cheat flags VeloFrame as suspicious | Official APIs only, no hooking into the game, signed executable |
| HDR conversion looks different depending on the monitor | Read the white level from the Windows settings, test on several monitors |
