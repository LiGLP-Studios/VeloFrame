# Plan: Audio for clips on Windows

Goal: clips with game audio and optional microphone, **in sync to the millisecond**, without dropouts under load, and with almost no extra cost. Audio is cheap to compute; the hard parts are **timing, sync and robustness**. That is where VeloFrame should beat other recorders.

## Status (2026-11-06)

Phases A1–A5 are built, A6 is measured on the test PC (i5-6400, RTX 3050, Windows 10, G435 wireless headset). Measured with about 30 % CPU load from other programs:

| Target | Result |
| --- | --- |
| Extra CPU under 0.5 % | **+0.33–0.41 %** total (`veloframe-bench clip`, with and without audio) |
| Extra RAM under 5 MB | **+0.1 MB** Task Manager; audio buffers 1.3 MB for a 15 s buffer |
| A/V offset under 5 ms | **mean +0.7 ms** over 8 runs of `veloframe-bench sync` (system and process loopback); per run −2.9 … +5.6 ms, within a run ±1 ms |
| 0 dropouts | 0 dropouts, 0 ms late audio in all runs |
| Device change under 200 ms | implemented (reopen, gap = silence, video continues), **not yet measured on hardware** |
| No drift over 3 hours | clock conversion exact over 3 h (unit test); drift correction measured live: G435 drifts ~150 ppm, deviation stays under 2 ms |

Verified playback: Windows decoder (Media Foundation) and Chromium, H.264 and HEVC clips. Not yet tested live: the microphone (opening it records the room) and device changes; both run through the same paths as system audio.

### Findings that differ from this plan

1. **AAC priming is 0, not 1024–2112.** The Windows AAC encoder places its first window on sample 0. Encoding and decoding with the Windows decoder and with Chromium's decoder (WebCodecs) both give 0 samples offset; the first 1024 samples are a short fade-in. A guessed 1024 would have moved the audio by 21 ms. Test: `aac::tests::priming_matches_a_real_round_trip`.
2. **Media Foundation applies `media_time` in the edit list with the wrong sign.** A pre-roll skipped via `media_time` plays *later* by that amount in the Windows player (Chromium does it right). Empty edits work everywhere. So a clip's audio starts with the first AAC frame inside the video (at most 21 ms after the first frame) and is placed with an empty edit; `media_time` stays 0.
3. **System loopback is early by the output latency.** Its timestamps mark when Windows mixes, not when the sound is heard: 17–23 ms too early on the G435 headset (output latency ~20 ms, HDMI the same). VeloFrame measures the output latency once per device with a short silent output stream (`IAudioClock`) and shifts the stamps. Process loopback stamps are already right (+1 … +4 ms without correction).
4. **The image is older than its timestamp.** A frame is fetched at the frame clock and shows what Windows presented 0–16.7 ms earlier. Instead of a fixed half frame, the video thread measures this age (`LastPresentTime`) and the audio follows its smoothed value. This brought system loopback from +3.6 / +7.6 ms to −2.0 … +3.2 ms per run.
5. **Silence is not free with the Windows encoder.** It works at a constant bitrate: a frame of digital silence is ~213 bytes (normal ~340 bytes). 128 kbit/s × 60 s is still only ~1 MB.
6. **The mixer runs 60 ms behind real time** (plus 2 frames inside the AAC encoder), so late packets are not lost. `save-clip` therefore waits up to 250 ms until the audio covers the last video frame (saving a 10 s clip: ~120 ms instead of ~15 ms).

### Decisions on the open questions

1. Default system audio: **`all-except-host`** (follows nothing, misses nothing; works even if the game starts after the buffer).
2. Separate tracks: **only on request** (`"tracks": "separate"` or `"both"`); default is one track.
3. Microphone noise suppression: later.
4. Opus: later.

## Targets

| Metric | Target |
| --- | --- |
| Extra CPU (total) | under 0.5 % |
| Extra RAM (Task Manager) | under 5 MB |
| A/V offset in a saved clip | under 5 ms (one frame at 60 fps is 16.7 ms) |
| Dropouts under 60–70 % CPU load | 0 |
| Gap when the audio device changes (headset plugged in) | under 200 ms, video keeps running |
| Drift over a 3-hour session | 0 (every packet carries its own clock time) |

## What makes it "next gen"

1. **Game audio only, without Discord or Spotify.** Windows 10 2004+ can capture the audio of a single process tree (process loopback). The clip contains the game, not the voice chat or the music in the background. The test PC supports it (`system-check`: `singleAppAudio: true`).
2. **Never records its own host.** By default VeloFrame captures *everything except the host application* (exclude mode), so ClipHub's own screenshot sound never ends up in a clip.
3. **Separate tracks for editing, one mixed track for playback.** Track 1 is game + mic mixed (browsers only play the first track), tracks 2 and 3 hold game and mic separately for video editors.
4. **Sample-accurate sync.** Video and audio share one clock (QPC). The AAC encoder delay ("priming", 1024–2112 samples) is compensated in the MP4 edit list, which most recorders get slightly wrong.
5. **Proven by measurement.** A sync test in `veloframe-bench` shows a white flash and plays a click at the same moment, records it, and measures the offset in the saved clip.
6. **No Bluetooth trap.** Opening the microphone of a Bluetooth headset switches it to the hands-free profile (mono, 16 kHz): the game suddenly sounds terrible. VeloFrame detects this and warns instead of silently ruining the sound.

## Flow

```
Game process ──► WASAPI process loopback ─┐   (or system loopback on older Windows)
Microphone   ──► WASAPI capture ──────────┤
                                          ▼
              One audio thread (event-driven, MMCSS "Pro Audio")
              ├─ timestamp every packet with its QPC time
              ├─ fill silence gaps, correct drift (insert/drop samples)
              ├─ float → 16 bit directly into the encoder buffer (no extra copy)
              ├─ mix game + mic (gain per source)
              └─ AAC encoder (Media Foundation, built into Windows)
                                          ▼
              Audio ring per track (128 kbit/s × 300 s ≈ 5 MB at most, RAM)
                                          ▼
save-clip ──► MP4: video + 1–3 audio tracks, interleaved, priming compensated
```

## Building blocks

### 1. Capture (WASAPI)
- **System audio:** `IAudioClient` in shared mode with `AUDCLNT_STREAMFLAGS_LOOPBACK` on the default output device.
- **Game only / everything except the host:** `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (include or exclude a process tree). Needs a small COM completion handler (`IActivateAudioInterfaceCompletionHandler`, agile).
- **Microphone:** `IAudioClient` in shared mode on the default (or chosen) input device.
- **Format:** `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` lets Windows deliver 48 kHz stereo float, whatever the device runs at.
- **Event-driven:** `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, 10 ms buffers. One thread waits on all sources with `WaitForMultipleObjects`: no polling, no CPU when nothing happens.
- **Priority:** the audio thread registers with MMCSS as "Pro Audio". It uses almost no CPU but must never be late, otherwise samples are lost.

### 2. Clock and sync
- WASAPI returns the QPC time of every packet (`GetBuffer` → `u64QPCPosition`). The video already runs on the same clock source; both get timestamps on one shared timeline.
- **Silence:** system loopback delivers *no packets* while nothing plays. VeloFrame fills the gap with silence based on the clock, so audio never "slides" against the video.
- **Drift:** the sound card clock runs a few ppm different from QPC. VeloFrame compares the sample counter with the QPC time; if the deviation exceeds 2 ms it inserts or drops single samples (inaudible). No cumulative drift, even after hours.
- **AAC priming:** the encoder delay is written to the MP4 edit list (`elst`), so the first sample plays exactly at its time.

### 3. Encoding
- **AAC-LC via the Media Foundation AAC encoder** (part of Windows, no extra library): plays everywhere (browsers, Windows player, phones, editors).
- Bitrate per track: Performance 128 kbit/s, Long 160 kbit/s, optional 192 kbit/s.
- **Silence is almost free:** an AAC frame of digital silence is only a few bytes.
- Opus would be ~20–30 % better per bit, but needs an external library and is poorly supported by the Windows player. Possible later as an option.

### 4. Buffer and MP4
- Generalize the replay buffer into a plain packet ring; video keeps its keyframe logic, every AAC frame is a sync point.
- Audio is small (128 kbit/s × 60 s ≈ 1 MB), so its ring lives in RAM.
- `save-clip` takes the audio packets that cover exactly the video time span of the clip.
- MP4: audio track (`soun`, `mp4a` + `esds` with AudioSpecificConfig), **interleaved** in 0.5 s chunks so browsers can play while loading (important for ClipHub uploads), plus edit list for priming and offset.

### 5. Robustness
- **Device changes** (headset plugged in, default device changed) via `IMMNotificationClient`: reopen the source, fill the gap with silence, video keeps running.
- **Mute/gain while running:** the host decides (hotkey for push-to-talk or mute), VeloFrame applies it immediately.
- **Bluetooth headset:** detect the hands-free endpoint and report it in `buffer-status` instead of opening the mic silently.
- **Exclusive mode / protected audio:** some apps bypass the mixer or block capture. VeloFrame records silence and reports it instead of failing.

## API and protocol

New optional field `audio` in `buffer-start`:

```json
"audio": {
  "system": "all-except-host",      // "all" | "all-except-host" | {"process": 1234} | "off"
  "microphone": "default",          // "default" | device id | "off"
  "tracks": "both",                 // "mixed" | "separate" | "both"
  "bitrate": 128000
}
```

- Without `audio` the default is `all-except-host` without microphone, one mixed track.
- New commands: `audio-devices` (list inputs/outputs), `audio-set` (mute/gain per source while running).
- `buffer-status` gets per source: level, dropouts, drift correction, device, warnings (e.g. Bluetooth).

## Phases

| Phase | Content | Done when | Status |
| --- | --- | --- | --- |
| A1 | Generic packet ring, audio plan in `clip.rs`, MP4 audio track with interleaving and edit list, unit tests | MP4 with a synthetic AAC track passes structure tests | done (`core/replay.rs`, `core/audio.rs`, `core/mp4.rs`) |
| A2 | WASAPI system loopback + microphone, one event-driven thread, QPC timestamps, silence fill | raw audio arrives without gaps under load | done (`Win/wasapi.rs`, `Win/audio.rs`); 0 dropouts measured |
| A3 | AAC encoder, mixing, separate tracks | clip with sound plays in the Windows player and in the browser | done (`Win/aac.rs`); plays in Media Foundation and Chromium |
| A4 | Process loopback (one app / everything except the host), fallback to system loopback | game audio without Discord in the clip | done; `veloframe.exe` excludes the program that started it |
| A5 | Drift correction, device changes, mute/gain, Bluetooth detection, sync test in `veloframe-bench` | offset under 5 ms, device switch without a broken clip | done; offset measured, device switch not yet tested on hardware |
| A6 | Measure (CPU, RAM, dropouts, offset under load), document | targets above met | measured, see Status |

## Does audio need more performance?

Not in throughput: 48 kHz stereo is 96,000 samples per second; AAC encoding of a track costs well under 1 % of one core. The performance work is about **timing**:
- the thread must be woken on time (MMCSS "Pro Audio", event-driven, no polling),
- no extra copies (WASAPI buffer → encoder buffer directly),
- no locks that can block the audio thread (hand-off to the ring is short and never waits for the save),
- one thread for all sources instead of one per source.

## Open decisions

1. **Default system audio:** `all-except-host` (everything you hear, minus ClipHub) or only the foreground game? Following the foreground game is more precise but harder: the game may start after the buffer.
2. **Separate tracks by default** or only on request (they make files ~10 % larger)?
3. **Microphone noise suppression** (e.g. RNNoise) in 1.0 or later?
4. **Opus** as an option later?

## Risks

| Risk | Mitigation |
| --- | --- |
| Process loopback API is young and sparsely documented | Fallback to system loopback, test on Windows 10 and 11 |
| Bluetooth headset switches to hands-free profile | Detect and warn, never open the mic unasked |
| Games in exclusive audio mode bypass the mixer | Record silence, report it in the status |
| A/V offset differs per device (driver latency) | Use the QPC time of each packet, verify with the sync test on several devices |
