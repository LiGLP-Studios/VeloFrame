# Status: Windows 1.0 (work in progress)

State of the work on [PLAN-WIN-1.0.md](PLAN-WIN-1.0.md) as of 2026-11-06. Nothing is committed yet. `cargo build --release` is clean, `cargo test` passes for `veloframe-core` and `veloframe-win`.

## Overview

| Item | State |
| --- | --- |
| W1 video edit list | **done**, measured |
| W2 mouse cursor | **done**, measured |
| W3 rotated screens | built, unit-tested, **not tested on a rotated monitor** |
| W5 microphone and devices | microphone **done** (and a track-order bug fixed); device changes and Bluetooth **still need manual help** |
| W6 robustness | start/stop ×50 and "no AAC" **done**; resolution change, UAC and sleep **still need manual help** |
| W4 frame-accurate clip start | **built**, H.264 measured; QuickSync/B-frames and HEVC (edit-list path) not finished |
| W7 long run | not started |
| W8 docs and commit | not started (README, CLIP-ENGINE.md, website not updated yet) |

## W1: video edit list

**Finding:** Media Foundation applies `media_time` with the wrong sign for video too. A clip with `media_time` = 0.5 s showed its first frame at +500 ms in the Windows decoder. Chromium applies it correctly: duration 4.549 s, playback starts at 0.

**Fix (`core/src/mp4.rs`):** the video edit list no longer uses `media_time`. All presentation times are shifted so the first shown frame sits at 0 (`ctts` version 1, negative offsets allowed). The presentation length now runs to the end of the last *shown* frame. Before, with B-frames, it ended at the last *decoded* frame and the last frame was cut off.

**Test hardware:** NVENC accepts B-frames for neither HEVC nor H.264 (`applied` lists none). Intel QuickSync on the second screen (`VELOFRAME_BENCH_SCREEN=1920,0,1680,1050`) does accept 2 B-frames.

**Measured:**
- B-frame clip (QuickSync, Performance H.264): first frame at 0 ms in the Windows decoder and in Chromium (Edge headless). Flash frames land on the same timestamps in both.
- Sync on the Intel screen: same offset with and without B-frames (+15.3 / +15.2 ms mean). So B-frames do not change sync. The +15 ms comes from the second screen or GPU in the test setup, not from B-frames.
- Main screen (NVENC, Long H.264): mean +5.4 / +4.4 ms. The earlier documented range was run means −2.9 … +5.6 ms.

**Consequence for W4:** edit list (A) only works in Chromium. So smart start (B) is used, see W4.

## W2: mouse cursor in the video

- **API:** `buffer-start` takes `cursor` (default `true`). New command `cursor-set` `{"visible": bool}` returns the buffer status. `buffer-status` has `cursor` and `cursorError`. Rust API: `Engine::set_cursor(bool)`.
- **Shape (`core/src/cursor.rs`):** color, masked color and monochrome shapes are converted to BGRA with alpha, only when the shape changes. XOR (inverting) pixels become black with a 1-px white outline, so they stay visible on light and dark backgrounds. Unit-tested.
- **GPU (`Win/src/video.rs`):** the cursor is the second input stream of the video processor, with per-pixel alpha, in the same blit as BGRA→NV12 and scaling. No shader, no extra copy. If the GPU lacks a second stream or alpha blending, `cursorError` explains it and the video runs without the cursor.
- **Position:** the DXGI pointer position is the top-left of the shape (verified, hotspot already applied).
- **Measured (`veloframe-bench cursor`, RTX 3050):**
  - Hotspot: crosshair centre within 0.5 px, arrow tip within 1 px. At 720p from 1080p, within 2 screen px (1.3 video px).
  - `cursor-set` takes effect in the same frame, in both directions. While off: 0 of 57 frames show the cursor.
  - Transparency and edges checked by eye (crops saved next to the clip as `*.fadenkreuz.png` and `*.pfeil.png`).
  - CPU: the converter costs +0.004 ms per frame with the cursor on, about 0.007 % of total CPU. Whole-process CPU over 8 on/off windows: ±0.000 %. Target is under 0.05 %.

## W3: rotated screens (not tested on hardware)

- `duplicate()` no longer refuses rotated outputs. The video processor rotates the image upright (`VideoProcessorSetStreamRotation`; checks the rotation capability, clear `unsupported` error otherwise).
- Screenshots from the running buffer (`RecorderTap`) read the region in rotated coordinates. `image_box` is unit-tested for 90/180/270.
- When the resolution or rotation changes during recording, the video processor is rebuilt and the video keeps its size (the new image is scaled into it). This also covers the W6 resolution-change case in code.
- **Still to do:** rotate a monitor to portrait and record (`veloframe-bench clip …` with `VELOFRAME_BENCH_SCREEN`).

## W5: microphone and devices

**Found and fixed: wrong default track in the Windows player.** Media Foundation lists tracks in reverse `trak` order and plays the first one it finds. It ignores the "enabled" flag. With `both`, the Windows player played the **microphone** track instead of the mix. Chromium follows the enabled flag and played the mix.
- Fix in `core/src/mp4.rs`: audio `trak` boxes are written in reverse order (track IDs stay ascending, the mix stays ID 2 and enabled).
- Now both players play track 1: the mix with `both`, system audio with `separate`. Verified with the MF decoder and Chromium (WebAudio level pattern).

**Measured (`veloframe-bench mic`):**
- `default` (G435 dongle) with `both`: 3 tracks written, all 3 visible to the Windows decoder.
- Device by id (TONOR TD510) with `separate`: 2 tracks.
- `audio-set` mute: system muted at 2–4 s and the microphone at 5–7 s both show as digital silence (−120 dBFS) for exactly that time, only in the right tracks.

**Still to do (manual):**
- Unplug and replug the headset dongle, and change the default output and input device while recording: `veloframe-bench devices 60` logs each change, gap and video state, then checks the clip.
- Bluetooth: no Bluetooth headset is connected right now (the G435 runs on its dongle).

## W6: robustness

**Measured (`veloframe-bench robust 50`):**
- 50 × start/stop with audio (system + microphone, 3 tracks), a clip saved and decoded every 10th run: no crash, audio started every time.
- Handles stay at +63…+74 after the first 10 runs. That is a one-time load by the test decoder, not growth.
- RAM is flat (−0.1 MB).
- Without an AAC encoder (test switch `VELOFRAME_SIMULATE_NO_AAC`, behaves like Windows N): the video keeps running (173 frames in 3 s), `audio.error` explains the cause, and the clip is saved without audio.

**Still to do (manual):** resolution or display-mode change, UAC prompt, sleep/hibernate while recording. The code paths exist: re-opening Desktop Duplication, rebuilding the video processor, and the audio mixer jumping ahead after sleep.

## W4: frame-accurate clip start

Done so far:
- **Approach:** re-encode only from the requested frame up to the next keyframe, then copy the rest (`Win/src/smartstart.rs`).
  - Decoding uses the Media Foundation decoder with D3D11/DXVA. Frames stay on the GPU and are copied on the GPU into the pool of a second hardware encoder (same settings, no keyframe inside, fastest effort).
  - Decode times of the new part are shifted to come before the next keyframe.
- **Parameter sets:** if the new part's parameter sets differ, the video track gets a second sample description (`stsd` with 2 entries, `stsc` switches exactly at the boundary, unit-tested). With NVENC they are identical, so the clip has one entry.
- **No decoder for the codec** (HEVC on Windows 10 without "HEVC Video Extensions", as on this PC): the edit list sets the start (`media_time`). The Windows player cannot play such a clip anyway, and all other players handle the edit list correctly.
- **Warm encoder:** GPU device and the second encoder are kept after the first clip, for the lifetime of the buffer. On reuse: `NOTIFY_START_OF_STREAM` plus a forced keyframe. Without the new stream start the encoder accepted no more frames after draining (found and fixed).
- **Fallback:** if re-encoding fails, the clip starts at the keyframe as before (`start: "keyframe"`).
- **Protocol (`save-clip` result):** new fields `startOffsetMs`, `start` (`reencoded` / `edit-list` / `keyframe`) and `startEncodeMs`. Without `seconds`, the clip is as long as the buffer setting; before, it could be up to one GOP longer.

**Measured (`veloframe-bench start long h264`, NVENC, 1 s GOP):** 6 of 6 clips exactly as long as requested (3, 4, 5, 7, 10, 15 s → 3.000 … 15.000 s), frame count right, first frame at 0 in the Windows decoder.

| | Before | Now |
| --- | --- | --- |
| Save time, first clip | 12–138 ms | 529 ms |
| Save time, later clips | 12–138 ms | 300–470 ms |
| Of which re-encoding (warm, up to ~60 frames at ~4 ms each) | — | 280–360 ms |

**Still open:**
- Performance mode with QuickSync B-frames (`VELOFRAME_BENCH_SCREEN=1920,0,1680,1050 veloframe-bench start performance h264`, 2 s GOP): 6 of 6 clips are exactly as long as requested and the first frame is at 0 in the Windows decoder. The frame count is sometimes 1–2 short (298 instead of 300, 599 instead of 600); check why. Saving takes 400–1210 ms (up to 120 frames are re-encoded, on the integrated GPU).
- HEVC edit-list path: check in Chromium.
- Check by eye that the seam between the re-encoded and copied part looks right.
- **Sync test after smart start:** the last `veloframe-bench sync 10 self` found only 1 flash (brightness range 80–163 instead of 0–255). That may have been something on screen while it ran, or a regression. **Repeat before relying on sync.**
- The plan's estimate of 30–80 ms for the re-encode was too low.

## New measurement tools (`veloframe-bench`)

| Command | Purpose |
| --- | --- |
| `sync [n] [self] [performance\|long] [h264\|hevc]` | sync test, now also with B-frames / Performance |
| `cursor [720p\|1080p\|1440p]` | hotspot, switching, CPU (moves the mouse for about 40 s) |
| `mic [default\|<id>] [both\|separate]` | tracks, mute, level per track in the clip |
| `robust [n]` | start/stop with audio, handles and RAM, then without AAC |
| `devices [s]` | live device changes (manual: unplug the headset, change the default device) |
| `start [performance\|long] [h264\|hevc]` | frame-accurate start: length, method, save time |
| `check <clip>` | now also shows the first frame time and all audio tracks (which one plays by default) |

`VELOFRAME_BENCH_SCREEN=x,y,w,h` picks another screen, e.g. the Intel GPU for its encoder. `VELOFRAME_BENCH_CURSOR=off` records `clip` without the cursor.

## Changed files

- `core/src/mp4.rs`: composition offsets instead of `media_time`, presentation length, reversed audio `trak` order, second sample description, `skip`
- `core/src/cursor.rs` (new), `core/src/clip.rs` (`cursor`, `CursorSetRequest`, `cursorError`, `ClipStart`, new result fields), `core/src/lib.rs`
- `Win/src/video.rs`: pointer, cursor stream, rotation, `adapter_by_luid`
- `Win/src/record.rs`: cursor switch, rotation, rebuilding the video processor, smart start in `save`
- `Win/src/smartstart.rs` (new), `Win/src/encode.rs` (`finish`, `restart`), `Win/src/aac.rs` (test switch), `Win/src/verify.rs` (crops, all audio tracks, first frame time), `Win/src/synctest.rs` (cursor shape), `Win/src/lib.rs`
- `engine/src/lib.rs`, `engine/src/platform.rs`, `engine/src/commands.rs`: `set_cursor` / `cursor-set`
- `bench/src/main.rs`: the new commands above

## Next steps

1. Repeat the sync test (see W4) and finish W4: QuickSync/B-frames, HEVC in Chromium, check the seam.
2. Manual tests together: rotated monitor (W3); unplugging the headset and changing the default device (W5); resolution change, UAC and sleep (W6).
3. W7: long runs, one hour per mode while playing a game (logging tool still to write).
4. W8: README, CLIP-ENGINE.md and website (cursor, rotation, clip start, track order, new numbers, remove the fixed limitations), then the commit commands.
