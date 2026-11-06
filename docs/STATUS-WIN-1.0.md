# Status: Windows 1.0 (work in progress)

State of the work on [PLAN-WIN-1.0.md](PLAN-WIN-1.0.md) as of 2026-11-06. `cargo build --release` is clean, `cargo test --workspace` passes.

## Overview

| Item | State |
| --- | --- |
| W1 video edit list | **done**, measured |
| W2 mouse cursor | **done**, measured |
| W3 rotated screens | built, unit-tested, **not tested on a rotated monitor** |
| W5 microphone and devices | microphone **done** (and a track-order bug fixed); device changes and Bluetooth **still need manual help** |
| W6 robustness | start/stop ×50 and "no AAC" **done**; display-mode change at a game start **found broken, fixed and verified** (see W7); resolution change, UAC and sleep **still need manual help** |
| W4 frame-accurate clip start | **done**, measured (NVENC H.264, QuickSync with B-frames, HEVC edit-list path) |
| W7 long run | **done**: 1 h per mode, Long partly while playing; found and fixed a video that froze at the game start; dropped frames while playing (8–20 %) measured, **not yet improved** |
| W8 docs and commit | README, CLIP-ENGINE.md, ARCHITECTURE.md and website updated; commit commands given |

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

**QuickSync with B-frames** (`VELOFRAME_BENCH_SCREEN=1920,0,1680,1050 veloframe-bench start performance h264`, 2 s GOP): 6 of 6 clips exactly as long as requested, first frame at 0 in the Windows decoder. Saving takes 400–1210 ms (up to 120 frames, on the Intel HD 530).
- Some clips have 1–2 frames fewer than expected (298 instead of 300). Not caused by the re-encode: it delivers exactly as many frames as expected. The frames were already missing in the buffer, because the integrated GPU dropped them under load.
- One 3 s clip started 100 ms late because such a gap was exactly at the requested time; there was no frame there.

**HEVC without decoder (edit-list path, NVENC):** saving stays fast (108–118 ms). Chromium reports the requested length (5.013 s, 10.005 s), although the file holds the video from the keyframe before. Headless Edge does not decode HEVC frames, so only the length could be checked.

**Seam:** the largest brightness jump between two frames is at most 0.07 of 255 in all clips. No broken frame at the boundary between the re-encoded and the copied part.

**Sync after smart start:** `veloframe-bench sync 10 self` (Long H.264, start re-encoded): means +4.2 ms and −1.7 ms, 10 of 10 flashes found. The earlier run with only 1 flash was a disturbance on screen.

## Dropped frames (checked before the pre-alpha)

The first long run dropped 1–15 % of the frames per minute (earlier: 0.4–1.6 %), so the 1.0 changes were checked against the last commit under the same conditions.

| | Dropped |
| --- | --- |
| Last commit (before 1.0) | 28–46 of ~1800 (1.7–2.5 %) |
| 1.0, cursor off | 45–65 (2.5–3.6 %) |
| 1.0, cursor off, second input not set up at all | 43–70 |
| 1.0, cursor on | 75–97 in the first round, mixed later |

- **Same buffer, cursor switched every 15 s (`veloframe-bench cursorload`):** in 6 runs sometimes the cursor-on half won and sometimes the cursor-off half, by a large margin each time (e.g. 0.02 % vs. 4.6 %, then 6.5 % vs. 0.6 %). The drops come in bursts that hit whichever half they fall into.
- **Cause:** Brave was playing a video at the same time: GPU video decode 20–33 %, and the desktop compositor (dwm) at 37–53 % of the 3D engine. In Performance mode the conversion runs at low GPU priority on purpose ("game first") and waits behind that load. The last commit behaves the same.
- **Conclusion:** no regression from the 1.0 changes, cursor included. Nothing to fix.
- **GPU cost of the cursor** (`cargo test -p veloframe-win --release -- --ignored converter_cost --nocapture`): conversion 0.15–0.19 ms per frame without cursor, 0.25–0.27 ms with the cursor as second input.
- **Tried and dropped:** a separate small video-processor pass for the cursor area only. It cost 0.5–0.8 ms per frame, because every extra video-processor call has a high fixed cost on this GPU.

## W7: long run

`veloframe-bench longrun [performance|long] [minutes]` logs every minute: RAM, CPU, encoded and dropped frames (with the reason), new screen images, capture restarts, encoder effort, audio drift, late audio, dropouts and handle count, also as a CSV next to the clips. At the end it saves a clip.

**One hour per mode (1920×1080 @ 60 fps, system audio):**

| | Performance (HEVC, 30 s) | Long (H.264, 120 s) |
| --- | --- | --- |
| What ran | Brave playing a video for the first 22 min, then an idle desktop | display off for the first 10 min (power timeout), desktop until min 17, Minecraft from min 18 |
| RAM (private working set) | 19.6 MB, flat for 60 min | 12.7 MB; 21–55 MB after the game start, no upward trend |
| CPU (total) | 0.5–1.5 % | 0.1–0.6 %; 1.2–2.9 % after the game start (see below) |
| Dropped frames | 3.4 % while the video played (0.7–7.6 % per minute), 0.02 % afterwards | 2.1 % on the desktop; 6.5 % after the game start (on a frozen image, see below) |
| Audio | drift ≤ 2 ms, no late audio, no dropouts | same |
| Handles | 590–652, flat for the last 40 min | 566–627, flat |
| Clip at the end | 30.0 s, saved in 127 ms | 120.0 s, decodes fine, saved in 1563 ms |

- A buffer started while the display is off records nothing until the display comes back (Windows delivers no first image). After that it runs normally.

**Found and fixed: the video froze when the game started.** The Long clip at the end showed the same image for all 120 s: the desktop at 17:37 (taskbar clock in the video), the moment the Minecraft window opened. The buffer had kept encoding that image for 43 minutes.
- Cause: the game start changed the display mode, so Desktop Duplication lost access. The grabber reopened it with the screen object from the start of the buffer, while the old capture was still alive. Every new capture was lost again before it delivered an image, and now and then reopening failed with "access denied". That also explains the higher CPU (reopening on every frame), the RAM moving in steps of one desktop image, and the error in the status.
- Also found: `error` in `buffer-status` never went away, even after the capture worked again.

**Fix (`Win/src/video.rs`, `Win/src/record.rs`):**
- The old capture is released first. Then the screen is looked up again (fresh DXGI enumeration, same screen name and GPU) and reopened. This costs 1–2 ms (`reopen_cost`, below), and the first image arrives 3–19 ms later.
- If reopening fails, or access is lost again before an image arrives, the next attempt comes after 250 ms instead of on every frame. If that goes on for 2 s, `error` says so instead of the video silently standing still.
- If no new image arrives for 2 s, the capture is reopened as a precaution. On a quiet screen this simply delivers the same image again.
- `error` in `buffer-status` now disappears 5 s after the error last occurred (unit-tested).
- New in `buffer-status`: `screenUpdates` (new screen images so far) and `captureRestarts` (reopens after lost access).

**Verified:** Minecraft was restarted (19:00:12) while a Long buffer was running. There were 2 capture restarts, and the clip shows the loading screen and then the title screen, so the video kept running. With the old code the same event froze it. That run still had a small bug, now fixed: the old capture was released one step too late, so reopening needed one extra attempt after 250 ms. The final code then ran about 10 minutes with the game, without problems.

**Dropped frames while playing** (Minecraft fullscreen on the RTX 3050, live capture, 2–3 min per run):

| | Dropped per minute | Reasons |
| --- | --- | --- |
| Long H.264 | 11–18 % | about 60 % "encoder busy", 40 % "thread late" |
| Performance HEVC | 8–20 % | (not logged in that run) |

- This is the first measurement with a game. The README value (0.4–1.6 %) was measured under CPU load without a game. The 6.5 % in the long run above was on the frozen image, with no new image to copy.
- "Encoder busy": converted frames wait for the GPU, which works on the game first. "Thread late": the capture thread came more than two frames late (4-core CPU, Minecraft uses a lot of it).
- Saving with frame-accurate start took 6.6 s under game load; the re-encode waits behind the game too.
- Not fixed for the pre-alpha. This is the next performance item.
- Recording the second screen (Intel HD 530) while the game runs drops about two thirds of the frames, with the old and the new code alike.

## New measurement tools (`veloframe-bench`)

| Command | Purpose |
| --- | --- |
| `sync [n] [self] [performance\|long] [h264\|hevc]` | sync test, now also with B-frames / Performance |
| `cursor [720p\|1080p\|1440p]` | hotspot, switching, CPU (moves the mouse for about 40 s) |
| `mic [default\|<id>] [both\|separate]` | tracks, mute, level per track in the clip |
| `robust [n]` | start/stop with audio, handles and RAM, then without AAC |
| `devices [s]` | live device changes (manual: unplug the headset, change the default device) |
| `start [performance\|long] [h264\|hevc]` | frame-accurate start: length, method, save time, seam |
| `longrun [performance\|long] [minutes]` | long run, one line per minute (drop reasons, new screen images, capture restarts), CSV |
| `cursorload [rounds]` | dropped frames with and without cursor in the same buffer (switched every 15 s); `VELOFRAME_BENCH_OFF_FIRST`, `VELOFRAME_BENCH_HOLD=on\|off` |
| `check <clip>` | now also shows the first frame time and all audio tracks (which one plays by default) |

Hardware tests (ignored by default, `cargo test -p veloframe-win --release -- --ignored <name> --nocapture`): `converter_cost` (GPU time of the conversion), `duplication_probe` (what Desktop Duplication reports for 8 s: new images, timeouts, lost access, reopen results), `reopen_cost` (reopening with fresh enumeration, per screen).

`VELOFRAME_BENCH_SCREEN=x,y,w,h` picks another screen, e.g. the Intel GPU for its encoder. `VELOFRAME_BENCH_CURSOR=off` records `clip` without the cursor.

## Changed files

- `core/src/mp4.rs`: composition offsets instead of `media_time`, presentation length, reversed audio `trak` order, second sample description, `skip`
- `core/src/cursor.rs` (new), `core/src/clip.rs` (`cursor`, `CursorSetRequest`, `cursorError`, `ClipStart`, new result fields, `screenUpdates`, `captureRestarts`), `core/src/lib.rs`
- `Win/src/video.rs`: pointer, cursor stream, rotation, `adapter_by_luid`, reopening the capture after lost access (fresh enumeration, retry delay, 2 s watchdog)
- `Win/src/record.rs`: cursor switch, rotation, rebuilding the video processor, smart start in `save`, `error` disappears 5 s after the last occurrence
- `Win/src/smartstart.rs` (new), `Win/src/encode.rs` (`finish`, `restart`), `Win/src/aac.rs` (test switch), `Win/src/verify.rs` (crops, all audio tracks, first frame time), `Win/src/synctest.rs` (cursor shape), `Win/src/lib.rs`
- `engine/src/lib.rs`, `engine/src/platform.rs`, `engine/src/commands.rs`: `set_cursor` / `cursor-set`
- `bench/src/main.rs`: the new commands above
- Docs: `README.md`, `docs/CLIP-ENGINE.md`, `docs/ARCHITECTURE.md`; website `../website/docs.html` and `../website/en/docs.html` (not in this repository)

## Next steps

1. Manual tests together:
   - Rotated monitor (W3).
   - Unplugging the headset and changing the default device (W5): `veloframe-bench devices 60`.
   - Resolution change, UAC and sleep while recording (W6). `captureRestarts` and `screenUpdates` in `longrun` now show whether the video keeps running.
2. Dropped frames while playing (8–20 % on this PC) and save time under game load (6.6 s): look at GPU priority in Long mode, the capture thread's CPU priority and the number of waiting frames. The drop reasons in `buffer-status` and `longrun` show where frames are lost.
3. A full hour per mode while playing, with the final code (it ran about 10 minutes with the game).
4. Then add the remaining numbers to README and CLIP-ENGINE.md and remove the limitations that are verified.
