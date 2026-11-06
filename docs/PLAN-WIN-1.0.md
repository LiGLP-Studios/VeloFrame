# Plan: Windows 1.0

Goal: close the remaining gaps so the Windows engine can be called 1.0. Screenshots, the clip buffer and audio work and are measured; what is left is the mouse cursor in the video, a few cases that are built but not yet tested on hardware, and two correctness issues found while building audio.

Testing on other hardware (AMD and Intel GPUs, laptops with two GPUs, Windows 11) happens through the ClipHub pre-alpha and is not part of this plan.

## Targets

| Metric | Target |
| --- | --- |
| Clips play in sync in the Windows player and in Chromium, with and without B-frames | offset under 5 ms (`veloframe-bench sync`) |
| Mouse cursor in the video | switchable on/off by the host at any time, correct shape and hotspot, extra CPU under 0.05 % |
| Clip start | within one frame of the requested time (today: up to 2 s earlier in Performance mode) |
| Audio device change while recording | gap under 200 ms, video keeps running, clip not broken |
| Long run (1 h per mode) | RAM flat (± 1 MB), audio drift under 2 ms, no growing handle count |
| Start/stop 50 times | no crash, no leak |

## Work items

### W1. Video edit list in the Windows player (do first)

While building audio we measured that Media Foundation applies `media_time` in an edit list with the **wrong sign** (audio played later instead of being trimmed). The video track uses `media_time` too: whenever the encoder writes B-frames, the first shown frame has a composition offset and the edit list skips it. NVIDIA's HEVC encoder takes no B-frames, so this never showed up on the test PC, but AMD and Intel encoders do, and so does H.264 in Performance mode when HEVC is missing.

- Write a clip with B-frames and check it in the Windows decoder and in Chromium (`veloframe-bench check`, sync test with `codec: h264` in Performance mode).
- If Media Foundation is wrong here as well: drop `media_time` from the video edit list and shift the composition offsets instead (`ctts` version 1 with negative offsets, first shown frame at time 0). That is the standard way for players that ignore edit lists.
- Done when: B-frame clips are in sync in both players.

### W2. Mouse cursor in the video

- **The host decides, and can switch it at any time.** New field `cursor` in `buffer-start` (`true`/`false`, default `true`), plus a new command `cursor-set` (`{"visible": true|false}`) that switches it while the buffer runs, from the next frame on. ClipHub can bind it to a setting or a hotkey; the Rust API gets `Engine::set_cursor(bool)`. `buffer-status` reports the current state.
- Switching costs nothing: the cursor stream of the video processor is simply enabled or disabled, the encoder and the buffer keep running.
- Desktop Duplication delivers the pointer position with every frame and the shape only when it changes (`GetFramePointerShape`: color, masked color, monochrome). Convert the shape once per change into a small BGRA texture with alpha (monochrome and masked shapes need XOR handling, done on the CPU, only on change).
- Draw it with the GPU video processor as a **second input stream with alpha**, in the same pass as the BGRA → NV12 conversion: no shader, no extra copy, the CPU still never touches a frame.
- Hidden cursor (most games) is not drawn. Screenshots taken from the running buffer keep their own `cursor` setting.
- Done when: cursor visible with the right hotspot, also on scaled output (1080p from 1440p); `cursor-set` switches it within one frame in both directions; CPU impact measured.

### W3. Rotated screens

- Today `duplicate()` refuses rotated outputs in video mode. The video processor can rotate during conversion (`VideoProcessorSetStreamRotation`).
- Output size uses the rotated dimensions; screenshots from the running buffer (`RecorderTap`) read the region in rotated coordinates (the screenshot path already handles rotation).
- Done when: a portrait monitor records a correct, upright clip.

### W4. Frame-accurate clip start

A clip starts at the keyframe before the requested start (Performance: up to 2 s earlier). Two ways, chosen after W1:

- **A. Edit list.** Keep the packets from the keyframe, let the edit list start playback at the requested frame. Cheap (no re-encoding), but only usable if W1 shows Media Foundation handles video `media_time` correctly.
- **B. Smart start.** Decode the first partial GOP with the hardware decoder and re-encode only the frames from the requested start up to the next keyframe, then copy the rest unchanged. Works in every player; costs one GOP of encoding during the save (estimate: 30–80 ms on the encoder chip). The re-encoded part has its own parameter sets, so the MP4 gets a second sample description (or the encoder is configured to produce identical ones).
- Audio already starts at the first frame (`clip_range`), nothing to change there.
- Done when: clip starts within one frame of the requested time, save time measured.

### W5. Microphone and device changes, live

Built in the audio work, but not yet tested with real hardware:

- Microphone: `default`, a device chosen by id, tracks `separate` and `both` (three tracks, check that editors see all of them), `audio-set` mute while recording.
- Bluetooth: if a Bluetooth headset is available, check that `default` does not open its microphone and the warning appears.
- Device changes: unplug and replug the headset dongle, change the default output and input device while recording. Measure the gap, check that the clip plays through.
- Fix whatever breaks.

### W6. Robustness

- Start/stop the buffer 50 times with audio (clean teardown of the audio thread, WASAPI and the AAC encoder; handle and memory count before/after).
- Audio not available (Windows N without the Media Feature Pack): force the AAC encoder to fail and check that the video buffer keeps running and `buffer-status.audio.error` explains it.
- Resolution or display mode change and UAC prompt during recording (Desktop Duplication access lost path).
- Sleep/hibernate while the buffer runs (the audio mixer skips ahead instead of encoding hours of silence; the video thread catches up).

### W7. Long run

- One hour each in Performance and Long mode while playing a game, audio on.
- Log every minute: RAM, CPU, dropped frames, encoder effort, audio drift, late audio, dropouts, handle count.
- Done when: the targets above hold; results go into the README.

### W8. Documentation and release

- README, `CLIP-ENGINE.md`, website docs: cursor, rotation, clip start, new measurements; remove the limitations that are fixed.
- Commit commands for the 1.0 commit.

## Order

| Step | Item | Why this order |
| --- | --- | --- |
| 1 | W1 edit lists | can break sync on the pre-alpha hardware, decides W4 |
| 2 | W2 cursor | biggest visible gap compared to other recorders |
| 3 | W3 rotation | small, same code area as W2 (video processor) |
| 4 | W5 microphone and devices | needs a few minutes of help (plugging the headset) |
| 5 | W6 robustness | |
| 6 | W4 clip start | largest item, approach depends on W1 |
| 7 | W7 long run | runs last, on the finished code |
| 8 | W8 docs and commit | |

## Decisions

1. **Cursor:** switchable by the host, at start (`cursor` in `buffer-start`) and while running (`cursor-set`). Default on, like most recorders; games hide the cursor anyway, so it only shows where it is visible on screen.
2. **Clip start:** edit list (A) if W1 shows Media Foundation handles video `media_time` correctly, otherwise smart start (B). Decided right after W1.
3. **Long run:** one hour per mode. The three-hour drift target of the audio plan is covered by the exact clock math (unit test) and the bounded drift correction; a longer run follows if the one-hour runs show any trend.

## Protocol changes

| Command | Change |
| --- | --- |
| `buffer-start` | new optional `cursor` (`true`/`false`, default `true`) |
| `cursor-set` | new: `{"visible": true|false}`, switches the cursor while the buffer runs; returns the buffer status |
| `buffer-status` | new `cursor` (current state) |
| `save-clip` | clip starts at the requested time (W4); new `startOffsetMs` (how far the actual start is from the request, normally 0) |

All changes are additive: hosts that do not send the new fields keep working (they get the cursor, which is the new default).

## Out of scope

- Other GPUs, laptops and Windows 11: ClipHub pre-alpha.
- HEVC in the Windows 10 player: needs the "HEVC Video Extensions" from the Microsoft Store; ClipHub and browsers play it.
- Windows Graphics Capture (phase 2), macOS, Linux.
