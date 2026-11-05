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

## Per-frame flow (60 fps = every 16.7 ms)

```
Desktop Duplication ─► GPU texture (only when something changed)
        │
GPU video processor: BGRA → NV12, BT.709, scaling ─► frame from the encoder pool
        │
Queue (up to 4 frames) ─► hardware encoder (NVENC / AMF / QuickSync via Media Foundation)
        │
Packets ─► replay buffer (fixed ring, always a keyframe at the front)
        │
save-clip ─► MP4 (index at the front, packets unchanged)
```

The CPU never touches a pixel. The clock thread is registered with Windows as a capture task (MMCSS) and is woken on time even under load.

## What VeloFrame does itself

- **Queue in front of the encoder:** Frames are converted on time and queued; the encoder picks them up as soon as it is ready. Short delays (GPU busy, encoder thread slowed down) no longer cost a frame. Measured: drops under load went from 8–11 % to 0.4–1.6 %.
- **Encoder effort controller:** If more than 1 % of frames are lost, VeloFrame lowers the encoder effort while running; once there is headroom again, it raises it step by step. Smooth video comes before the last percent of compression.
- **Buffer in a temporary file:** Windows keeps it in the cache where possible and only writes it to the SSD when memory runs low; it is deleted on exit (even after a crash). This memory does not count towards the engine, and Windows can give it back to the game at any time.
- **Saving without a copy:** The index is copied under the lock, the data is read afterwards while recording continues. The clip is never held completely in memory. If the ring was overwritten meanwhile, this is detected.
- **One capture for both:** Windows allows only one Desktop Duplication session per screen and program. While the buffer runs, screenshots of that screen come from the live image (measured 17 ms total).
- **Clean teardown:** The encoder is drained (`DRAIN`) before it is shut down; Media Foundation stays started for the lifetime of the process. Before, stopping crashed in about one in five runs; afterwards 40 out of 40 runs ended cleanly.

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

- **No audio** and **no mouse cursor** in the video yet.
- One screen per buffer; rotated screens not yet supported.
- A clip starts at the last keyframe before the requested start (Performance: up to 2 s earlier). "Smart start" (re-encode only the beginning, accurate to the frame) is still to do.
- Windows 10 only plays HEVC with the "HEVC Video Extensions" from the Microsoft Store; browsers (Chrome, Edge, Electron) play HEVC with hardware support. For maximum compatibility use `codec: "h264"`.
- The NVIDIA HEVC encoder does not accept B-frames (they are then missing from `applied`).
