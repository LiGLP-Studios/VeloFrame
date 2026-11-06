# Clip engine

The clip buffer continuously captures a screen, encodes in hardware and keeps the last seconds ready. On request they are saved as MP4, **without re-encoding**.

## Modes

| | Performance | Long |
| --- | --- | --- |
| Length | 5–60 s | 5–300 s |
| Codec | HEVC (if the GPU supports it), otherwise H.264 | H.264 (plays everywhere) |
| Bitrate 1080p60 "high" | mean 6.2 / cap 10.4 Mbit/s, follows the content | mean 13.6 / cap 16 Mbit/s |
| Keyframe | every 2 s | every second |
| Encoder effort | 85 (controller lowers it when needed) | 70 (controller lowers it when needed) |
| GPU priority | behind the game | normal |
| Buffer | temporary file | temporary file |

Codec and buffer location can be overridden per request (`codec`, `storage`).

With audio, the clip gets one to three AAC tracks next to the video (see [Audio](#audio)).

## Per-frame flow (60 fps = every 16.7 ms)

```
Desktop Duplication ─► GPU texture (only when something changed)
        │
GPU video processor: BGRA → NV12, BT.709, scaling, rotation, ─► frame from the encoder pool
                     mouse cursor as a second input with alpha
        │
Queue (up to 4 frames) ─► hardware encoder (NVENC / AMF / QuickSync via Media Foundation)
        │
Packets ─► replay buffer (fixed ring, always a keyframe at the front)
        │
save-clip ─► MP4 (index at the front, packets unchanged; only the beginning up to the next
              keyframe is re-encoded so the clip starts at the requested frame)
```

The CPU never touches a pixel. The clock thread is registered with Windows as a capture task (MMCSS) and is woken on time even under load.

## What VeloFrame does itself

- **Queue in front of the encoder:** Frames are converted on time and queued; the encoder picks them up as soon as it is ready. Short delays (GPU busy, encoder thread slowed down) no longer cost a frame. Measured: drops under load went from 8–11 % to 0.4–1.6 %.
- **Encoder effort controller:** If more than 1 % of frames are lost, VeloFrame lowers the encoder effort while running; once there is headroom again, it raises it step by step. Smooth video comes before the last percent of compression.
- **Buffer in a temporary file:** Windows keeps it in the cache where possible and only writes it to the SSD when memory runs low; it is deleted on exit (even after a crash). This memory does not count towards the engine, and Windows can give it back to the game at any time.
- **Saving without a copy:** The index is copied under the lock, the data is read afterwards while recording continues. The clip is never held completely in memory. If the ring was overwritten meanwhile, this is detected.
- **One capture for both:** Windows allows only one Desktop Duplication session per screen and program. While the buffer runs, screenshots of that screen come from the live image (measured 17 ms total).
- **Mouse cursor:** Desktop Duplication delivers the pointer position with every frame and the shape only when it changes. The shape is converted once per change into a small BGRA texture with alpha (`core/cursor.rs`; inverting pixels become black with a white outline) and drawn by the video processor as a second input stream in the same pass as the NV12 conversion. Switching it (`cursor-set`) only enables or disables that stream; encoder and buffer keep running. A hidden cursor (most games) is not drawn. Screenshots from the running buffer draw the cursor according to their own `cursor` setting.
- **Rotated screens:** the video processor rotates the image upright during the conversion; screenshots from the running buffer read the region in rotated coordinates. If the resolution or rotation changes while recording, the conversion is set up again and the video keeps its size.
- **Frame-accurate start (`Win/smartstart.rs`):** the buffer can only start at a keyframe. On save, the frames from the requested start up to the next keyframe are decoded (Media Foundation decoder, D3D11) and re-encoded by a second hardware encoder with the same settings; everything after that keyframe is copied unchanged. Frames stay on the GPU. GPU device and encoder are kept after the first clip. If the new beginning has different parameter sets, the MP4 gets a second sample description. If Windows has no decoder for the codec (HEVC on Windows 10 without the Store extension), the edit list sets the start instead: no Media Foundation player can play that clip anyway, all others honor the edit list.
- **MP4 for every player:** Media Foundation applies `media_time` in edit lists with the wrong sign (measured for audio and video), so the video edit list never uses it; composition offsets are shifted so the first shown frame is at 0 (`ctts` version 1). Media Foundation also plays the last audio `trak` it finds and ignores the "enabled" flag, Chromium follows the flag: audio tracks are written in reverse order so both play track 1.
- **Lost capture:** when Desktop Duplication loses access (UAC, a game switching to fullscreen, resolution change), the old capture is released first, the screen is looked up again (fresh DXGI enumeration, same screen and GPU) and reopened. With the screen object from the start, every new capture was lost again at once and the video stood still. Failed attempts are retried every 250 ms; if no image arrives for 2 s, `error` says so. Independently, the capture is reopened when no new image came for 2 s (1–2 ms; on a quiet screen it just delivers the same image again). `screenUpdates` and `captureRestarts` in `buffer-status` show whether the video is live.
- **Clean teardown:** The encoder is drained (`DRAIN`) before it is shut down; Media Foundation stays started for the lifetime of the process. Before, stopping crashed in about one in five runs; afterwards 40 out of 40 runs ended cleanly.

## Audio

```
Game / system ─► WASAPI process or system loopback ─┐
Microphone    ─► WASAPI ────────────────────────────┤
                                                    ▼
   One audio thread (event-driven, MMCSS "Pro Audio")              Win/audio.rs
   ├─ every packet placed at its QPC time: silence fill, drift     core/audio.rs
   ├─ mix (gain per source), float → 16 bit straight into the encoder
   └─ AAC per track (Media Foundation)                             Win/aac.rs
                                                    ▼
   Audio buffer per track (RAM) ─► save-clip: MP4 with video + 1–3 audio tracks
```

- **One clock.** Video and audio count in 100 ns from the same QPC origin. Every audio packet carries its own clock time, nothing adds up over hours.
- **Silence and drift.** Loopback delivers nothing while nothing plays; the gap becomes silence, audio never slides against the video. If the sound card clock drifts (measured on the G435 headset: about 150 ppm), single samples are inserted or dropped once the deviation passes 2 ms; jumps over 4 ms are gaps and are realigned at once.
- **Image age.** A frame is fetched at the frame clock and shows what Windows presented a little earlier. The video thread measures this age (`LastPresentTime`) and the audio is shifted by its smoothed value (`buffer-status.audio.videoLagMs`).
- **System loopback latency.** System loopback stamps the time Windows mixes, not when it is heard (here 19–21 ms earlier). VeloFrame measures the output latency once with a short, silent output stream and shifts the stamps by it. Process loopback is already correct.
- **Clip cut.** Audio starts with the first AAC frame inside the video and is moved to its exact time with an empty edit. A pre-roll skipped via `media_time` would be wrong in the Windows player: Media Foundation applies `media_time` with the opposite sign (measured).
- **Never blocks.** The audio thread hands packets to the buffer with `try_lock`; if a save holds the lock, they wait for the next round. `save-clip` waits up to 250 ms until the audio covers the last frame.
- **Device changes** (headset plugged in, default changed, device removed): the source is reopened, the gap is silence, the video keeps running.

## Measurements

Intel Core i5-6400, RTX 3050, Windows 10, 1920×1080 @ 60 fps, CPU already 60–70 % busy with other programs. Task Manager = private working set.

| | Performance (30 s) | Long (60 s) |
| --- | --- | --- |
| RAM, clip buffer only (`veloframe-bench clip`) | 12.7–19.6 MB | 12.7–19.6 MB |
| RAM with screenshot engine (`veloframe.exe`) | 36 MB | |
| CPU (total) | 0.4–0.6 % | 0.5 % |
| GPU 3D engine | 2–4 % | |
| Dropped frames under load | 0.4–1.6 % | 2.4 % |
| Saving | 75–138 ms (31 s clip) | 465 ms (60 s, 65 MB) |
| Actual bitrate | 3.6–6.3 Mbit/s | 7.5–9.1 Mbit/s |

Verified: H.264 clips with the Windows decoder (all frames, correct duration, middle frame correct), HEVC clips in the Chromium browser (smooth 58 fps, 0 dropped).

## Known limitations

- Audio: device changes are implemented but not yet measured on hardware; the microphone is measured (see [STATUS-WIN-1.0.md](STATUS-WIN-1.0.md)). The A/V offset per recording varies by a few ms with the phase between the capture clock and the display refresh.
- One screen per buffer. Rotated screens are supported but not yet tested on a rotated monitor.
- The frame-accurate start costs save time: 150–530 ms with NVENC and a 1 s GOP (Long), more with a 2 s GOP or a weaker GPU (measured up to 1.2 s on an Intel HD 530), and 6.6 s while a game kept the same GPU busy.
- While a game uses the same GPU, frames are dropped: 8–20 % measured with Minecraft fullscreen on an RTX 3050 with a 4-core CPU.
- A buffer started while the display is off records nothing until the display comes back.
- Windows 10 only plays HEVC with the "HEVC Video Extensions" from the Microsoft Store; browsers (Chrome, Edge, Electron) play HEVC with hardware support. For maximum compatibility use `codec: "h264"`.
- The NVIDIA encoder does not accept B-frames, neither for HEVC nor H.264 (they are then missing from `applied`).
