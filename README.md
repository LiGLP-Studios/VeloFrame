<p align="center">
  <img src="logo/veloframe-icon-512.png" alt="VeloFrame logo" width="160">
</p>

<h1 align="center">VeloFrame</h1>

<p align="center">High-performance screenshot and video capture engine for embedding into applications.</p>

---

VeloFrame has **no user interface**. The host application (e.g. ClipHub) decides when to capture (hotkey, button, automation) and what happens with the result. VeloFrame does the capturing.

**Status (Windows):**
- **Screenshots:** Desktop Duplication, custom PNG and JPEG encoders running on all CPU cores, pre-warmed capture ([plan](docs/PLAN-SCREENSHOT-WIN.md)).
- **Clip buffer:** continuously keeps the last seconds as video and saves them as MP4 on request. Two modes: **Performance** (5–60 s, tuned for minimal resource usage) and **Long** (up to 300 s). Details in [docs/CLIP-ENGINE.md](docs/CLIP-ENGINE.md).
- **Audio in clips:** game or system audio and an optional microphone, AAC in the MP4, on the same clock as the video. By default everything you hear *except the host application* (ClipHub's own sounds never end up in a clip). Only one game (without Discord or Spotify), separate tracks for editors, mute/gain while running, Bluetooth headset protection. Measured A/V offset in saved clips: mean +0.7 ms ([plan and results](docs/PLAN-AUDIO-WIN.md)).

Not yet: mouse cursor in video, Windows Graphics Capture (phase 2), macOS and Linux.

## Embedding

### As a Rust library

```rust
use veloframe::{Engine, ScreenshotRequest, ImageFormat, Area, Rect};

let mut engine = Engine::new()?;
engine.warm_up()?;                         // open capture up front: even the first screenshot is fast
let shot = engine.screenshot(&ScreenshotRequest {
    format: ImageFormat::Jpeg,
    quality: 90,
    cursor: false,
    area: Area::Window,                    // foreground window, otherwise the screen
    screen: Rect::new(0, 0, 2560, 1440),   // physical pixels
    folder: "C:\\Users\\Name\\Pictures\\Screenshots".into(),
    clipboard: true,                       // copied to the clipboard while encoding
})?;
println!("{} in {:.1} ms", shot.path, shot.total_ms);
```

### As a separate process (Electron, Node, Python, C# …)

Start `veloframe.exe` and send one JSON message per line on stdin; answers arrive line by line on stdout. If the engine crashes, the host application keeps running.

```
→ {"id":1,"cmd":"screenshot","args":{"format":"png","area":"screen","screen":{"x":0,"y":0,"width":1920,"height":1080},"folder":"C:\\Pictures"}}
← {"id":1,"ok":true,"result":{"path":"C:\\Pictures\\Screenshot 2026-11-05 163141.png","width":1920,"height":1080,"captureMs":4.0,"encodeMs":25.0,"totalMs":29.0,"warm":true,...}}
```

| Command | Arguments | Result |
| --- | --- | --- |
| `ping` | | `"pong"` |
| `version` | | Engine, version, active capture source |
| `system-check` | | GPUs, monitors, HDR, encoders, available capture paths |
| `warm-up` | | Open capture on all screens up front (`veloframe.exe` does this on start) |
| `screenshot` | `format` (`png`/`jpeg`), `quality`, `cursor`, `area` (`screen`/`window`), `screen`, `folder`, `clipboard` | Path, size, bytes, source, per-step timings (`captureMs`, `encodeMs`, `writeMs`, `totalMs`, `clipboardMs`) |
| `copy` | `path` | Copy the last screenshot to the clipboard again, otherwise error `not-cached` |
| `buffer-start` | `mode` (`performance`/`long`), `seconds`, `fps`, `quality` (`low`…`ultra`), `resolution` (`native`, `1440p`, `1080p`, `720p`), `screen`, optional `codec` (`h264`/`hevc`), `storage` (`file`/`ram`), `audio` (see below) | Plan (including the audio plan), encoder, accepted settings |
| `buffer-status` | | Seconds and bytes in the buffer, encoded and dropped frames (by reason), encoder effort; `audio`: per source capture path, device, level, dropouts, late/silence ms, drift, device changes, warnings |
| `save-clip` | `folder`, optional `seconds` | Path, size, duration, frames, codec, save time, `audioTracks`, `audioSeconds` |
| `buffer-stop` | | Stop the buffer, release memory and encoder |
| `audio-devices` | | Audio inputs and outputs: `id`, `name`, `kind` (`output`/`input`), `default`, `bluetooth` |
| `audio-set` | `system` and/or `microphone`: `{"gain": 0…4, "muted": true/false}` | Applied immediately (push-to-talk, mute); returns the audio status |

### Audio

Optional field `audio` in `buffer-start`. Without it: everything except the host application, no microphone, one track.

```json
"audio": {
  "system": "all-except-host",   // "all" | "all-except-host" | {"process": 1234} | "off"
  "microphone": "off",           // "off" | "default" | device id from audio-devices
  "tracks": "mixed",             // "mixed" | "separate" | "both"
  "bitrate": 128000              // per track: 96000, 128000, 160000, 192000 (default: Performance 128000, Long 160000)
}
```

- `{"process": 1234}` records only that program and its child processes (Windows 10 2004 and newer); `all-except-host` excludes the program that started `veloframe.exe` (or the embedding process, or `hostProcess`). If process audio is not available, VeloFrame records the whole system audio and says so in `buffer-status` (warning `process-loopback-unavailable`).
- `both`: track 1 is everything mixed (browsers only play the first track), tracks 2 and 3 hold system and microphone separately for video editors.
- A Bluetooth headset's microphone is never opened via `"default"`: it would switch the headset to the hands-free profile and ruin the game audio (warning `bluetooth-hands-free`). Choose it by its id to use it anyway.
- If audio cannot start (for example Windows N without the Media Feature Pack), the video buffer still runs and `buffer-status.audio.error` says why.

Errors look like `{"id":1,"ok":false,"error":"Message for the user","code":"no-screen"}`. Applications should react to `code`; the message can be shown directly. Codes: `invalid-request`, `no-screen`, `capture-lost`, `capture-failed`, `save-failed`, `clipboard-failed`, `not-cached`, `unsupported`.

The protocol is compatible with the previous ClipHub capture helper: ClipHub can start `veloframe.exe` instead of `cliphub-capture.exe` without changing its Electron code.

## Structure

| Folder | Crate | Contents |
| --- | --- | --- |
| `core` | `veloframe-core` | Platform independent: image types, interfaces (`ScreenSource`, `Desktop`), screenshot pipeline, custom PNG/JPEG encoders (`codec`), clip modes (`clip`), audio timeline, drift correction and mixer (`audio`), replay and audio buffers (`replay`), MP4 writer with audio tracks (`mp4`), protocol, errors |
| `engine` | `veloframe` | The engine API (`Engine`) and the `veloframe.exe` program |
| `Win` | `veloframe-win` | Desktop Duplication, mouse cursor, clipboard, system check, clip buffer (GPU capture, NV12 conversion, hardware encoder via Media Foundation), audio (WASAPI system/process loopback and microphone, AAC via Media Foundation) |
| `Mac` | `veloframe-mac` | ScreenCaptureKit, VideoToolbox (empty for now) |
| `Linux` | `veloframe-linux` | PipeWire, xdg-desktop-portal, X11, VAAPI (empty for now) |
| `bench` | `veloframe-bench` | Benchmark tool: compare encoders, measure screenshots, memory and the clip buffer |
| `docs` | | [Architecture](docs/ARCHITECTURE.md), [Windows screenshot plan](docs/PLAN-SCREENSHOT-WIN.md), [Clip engine](docs/CLIP-ENGINE.md), [Audio plan](docs/PLAN-AUDIO-WIN.md) |
| `logo` | | Logo |

Empty files describe in their first line what will go there.

## Build, test, measure

```
cargo build --release      # target/release/veloframe.exe
cargo test --workspace
cargo run --release -p veloframe-bench -- encode "Screenshot.png"   # compare encoders
cargo run --release -p veloframe-bench -- capture 20                 # measure real screenshots
cargo run --release -p veloframe-bench -- memory                     # memory after each step
cargo run --release -p veloframe-bench -- clip performance 30        # measure the clip buffer, save and verify a clip
cargo run --release -p veloframe-bench -- clip long 60
cargo run --release -p veloframe-bench -- clip performance 30 35 auto off   # without audio (audio: host | all | mic | off | <pid>)
cargo run --release -p veloframe-bench -- audio                      # list audio devices
cargo run --release -p veloframe-bench -- sync 10                    # A/V sync test: white flash + click, offset measured in the saved clip
cargo run --release -p veloframe-bench -- check "Clip.mp4"           # decode a clip with the Windows decoder (video and audio)
```

## Measurements

Release build, Intel Core i5-6400 (4 cores), RTX 3050, Windows 10, two monitors on two GPUs (RTX 3050 1920×1080, Intel HD 530 1680×1050). **Measured under load:** other programs were already using 60–70 % of the CPU, so values vary.

### Clip buffer (1920×1080 @ 60 fps)

| | Performance (30 s) | Long (60 s) |
| --- | --- | --- |
| RAM, clip buffer only (Task Manager) | 12.7–19.6 MB | 12.7–19.6 MB |
| RAM, screenshot engine + clip buffer (`veloframe.exe`) | 36 MB | |
| CPU (total) | 0.4–0.6 % | 0.5 % |
| GPU 3D engine | 2–4 % | |
| Dropped frames under load | 0.4–1.6 % | 2.4 % |
| Saving | 75–138 ms (31 s clip) | 465 ms (60 s, 65 MB) |

### Audio in the clip buffer

Same PC, other programs using about 30 % of the CPU, 1920×1080 @ 60 fps, system audio in one AAC track at 128 kbit/s.

| | Without audio | With audio | Target |
| --- | --- | --- | --- |
| CPU (total) | 0.49 % | 0.82–0.90 % (**+0.33–0.41 %**) | under +0.5 % |
| RAM (Task Manager) | 19.5 MB | 19.6 MB (audio buffers 1.3 MB) | under +5 MB |
| Dropouts, late audio | | 0, 0 ms | 0 |
| A/V offset in the saved clip (`veloframe-bench sync`, 8 runs, 6–7 flash/click pairs each) | | mean **+0.7 ms**, run means −2.9 … +5.6 ms | under 5 ms |
| Saving (5–16 s clip) | 12–31 ms | 117–133 ms (waits until the audio covers the last frame) | |

Verified: clips with audio in the Windows decoder (Media Foundation) and in Chromium (HEVC and H.264). Details and findings in [docs/PLAN-AUDIO-WIN.md](docs/PLAN-AUDIO-WIN.md).

### Encoders, 1920×1080 game scene (`veloframe-bench encode`)

Best of 25 runs / median.

| | previous (1 core) | VeloFrame (all cores) | |
| --- | --- | --- | --- |
| PNG | 25.8 ms / 29.8 ms, 2249 KB | 11.6 ms / 19.3 ms, 2225 KB | 2.2× faster, slightly smaller |
| JPEG | 42.9 ms / 48.1 ms, 410 KB | 15.7 ms / 28.7 ms, 411 KB | 2.7× faster |

For a desktop screenshot (UI instead of a game), VeloFrame PNG is also 16 % smaller (361 vs. 431 KB).

The encoder helper threads run at lower priority: when other programs (or a game) use the CPU, they come first and the screenshot takes slightly longer instead of costing the game frames. The table above was measured at normal priority; at lower priority with 60–70 % foreign load, the PNG advantage shrinks to 1.1–1.4×, JPEG stays 2.2–2.8× faster.

### Screenshots

- **First screenshot after start:** 287 ms before (capture had to be opened), now 20 ms thanks to pre-warming.
- **Capture itself** (Desktop Duplication, warm): 4–6 ms. Writing the file: about 2 ms.

| State | Task Manager (private working set) | Total working set | Committed |
| --- | --- | --- | --- |
| After start + warm-up, idle | **23.5 MB** | 45.5 MB | 57.5 MB |
| After screenshots, idle | **29.9 MB** | 52.1 MB | 64.1 MB |
| After a screenshot with clipboard | **37.9 MB** | 60.1 MB | 72.0 MB |

- **Task Manager** shows the private working set: memory used only by VeloFrame.
- **Total working set** additionally includes the NVIDIA and Intel driver files (about 22 MB), shared by all programs using the same GPU, including the game.
- **Committed** is reserved memory including copies of both screens (15 MB), which does not have to be fully in RAM.
- Clipboard: the image in it (8 MB at 1920×1080) belongs to the program that put it there until something else is copied. That is the same for every screenshot tool.
- The last screenshot stays in memory for `copy` and as a reusable buffer (6 MB).
- CPU when idle: 0.00 % (no background polling). GPU memory: 16 MB dedicated, 23 MB shared. No memory leak: 20 more screenshots change usage by less than 1 MB.

## License

VeloFrame is free for personal, educational and open-source use. **Commercial use** — in a business or in any app, product or service that is not open source — **requires a license from LiGLP Studios**. Reselling the engine is not permitted. See [LICENSE](LICENSE).

Licensing: [business@liglp.de](mailto:business@liglp.de)
