//! VeloFrame-Benchmark: misst die Engine, damit „schnell“ eine Zahl ist und keine Behauptung.
//!
//! ```text
//! veloframe-bench encode <bild.png>...   Encoder vergleichen (bisher, 1 Kern  vs.  VeloFrame, alle Kerne)
//! veloframe-bench capture [anzahl]       echte Screenshots des Hauptbildschirms über die Engine
//! veloframe-bench memory                 Arbeitsspeicher nach jedem Schritt (Start, Engine, Vorwärmen, Screenshots)
//! veloframe-bench clip [modus] [sek] [laufzeit] [codec] [ton]   Clip-Puffer messen (RAM, CPU, Bitrate), Clip speichern und prüfen
//! veloframe-bench audio                  Tongeräte auflisten
//! veloframe-bench sync [anzahl]          Sync-Test: Blitz + Klick aufnehmen, Versatz im gespeicherten Clip messen
//! ```
//!
//! Immer im Release-Build messen: `cargo run --release -p veloframe-bench -- ...`

use std::path::{Path, PathBuf};
use std::time::Instant;
use veloframe::{Area, Engine, ImageFormat, Rect, ScreenshotRequest};
use veloframe_core::RgbImage;
use veloframe_core::codec;

const RUNS: usize = 25;

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    values[values.len() / 2]
}

/// Führt `f` mehrmals aus: Bestwert und Median in ms, dazu die erzeugte Größe. Der Bestwert ist unter
/// Last (Spiel, Browser, Musik im Hintergrund) der verlässlichere Vergleich.
fn measure(mut f: impl FnMut() -> Vec<u8>) -> (f64, f64, usize) {
    let _ = f(); // Aufwärmen (Speicher, Caches)
    let mut times = Vec::with_capacity(RUNS);
    let mut size = 0;
    for _ in 0..RUNS {
        let start = Instant::now();
        size = f().len();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let best = times.iter().copied().fold(f64::MAX, f64::min);
    (best, median(times), size)
}

fn load_png(path: &Path) -> Result<RgbImage, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut data = vec![0; reader.output_buffer_size().ok_or("Bild zu groß")?];
    let info = reader.next_frame(&mut data).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        return Err("Nur 8-Bit-RGB-PNGs (wie VeloFrame-Screenshots).".into());
    }
    data.truncate(info.buffer_size());
    Ok(RgbImage { width: info.width as usize, height: info.height as usize, data })
}

/// Der bisherige PNG-Weg (png-Crate, „Fast“, ein Kern), wie im alten ClipHub-Helfer.
fn png_before(image: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&image.data).unwrap();
    writer.finish().unwrap();
    out
}

fn encode(paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("Mindestens ein PNG angeben.".into());
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("{cores} CPU-Kerne, {RUNS} Läufe je Encoder (Bestwert / Median)\n");
    for path in paths {
        let image = load_png(path)?;
        println!("{} ({}×{})", path.file_name().unwrap_or_default().to_string_lossy(), image.width, image.height);

        // VeloFrame-PNG muss verlustfrei sein: einmal zurücklesen und vergleichen.
        let mut ours = Vec::new();
        codec::png::encode(&image, &mut ours).map_err(|e| e.to_string())?;
        let decoded = {
            let mut reader = png::Decoder::new(std::io::Cursor::new(&ours)).read_info().map_err(|e| e.to_string())?;
            let mut data = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut data).map_err(|e| e.to_string())?;
            data.truncate(info.buffer_size());
            data
        };
        if decoded != image.data {
            return Err("VeloFrame-PNG ist nicht verlustfrei!".into());
        }

        let rows: [(&str, Box<dyn FnMut() -> Vec<u8>>); 4] = [
            ("PNG  bisher (1 Kern)", Box::new(|| png_before(&image))),
            ("PNG  VeloFrame", Box::new(|| {
                let mut out = Vec::new();
                codec::png::encode(&image, &mut out).unwrap();
                out
            })),
            ("JPEG bisher (1 Kern)", Box::new(|| {
                let mut out = Vec::new();
                codec::jpeg::encode_with_strips(&image, 90, 1, &mut out).unwrap();
                out
            })),
            ("JPEG VeloFrame", Box::new(|| {
                let mut out = Vec::new();
                codec::jpeg::encode(&image, 90, &mut out).unwrap();
                out
            })),
        ];
        let mut before = 0.0;
        for (i, (name, f)) in rows.into_iter().enumerate() {
            let (best, median, size) = measure(f);
            let speedup = if i % 2 == 1 { format!("  {:.1}× schneller", before / best) } else { String::new() };
            before = best;
            println!("  {name:<22} {best:>6.1} / {median:>6.1} ms  {:>6} KB{speedup}", size / 1024);
        }
        println!();
    }
    Ok(())
}

fn capture(runs: usize) -> Result<(), String> {
    let folder = std::env::temp_dir().join("veloframe-bench");
    let _ = std::fs::remove_dir_all(&folder);

    let started = Instant::now();
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screens = engine.warm_up().map_err(|e| e.to_string())?;
    println!("Engine bereit, {screens} Bildschirm(e) vorgewärmt in {:.0} ms\n", started.elapsed().as_secs_f64() * 1000.0);

    let report = engine.system_check().map_err(|e| e.to_string())?;
    let main = report["adapters"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|a| a["outputs"].as_array().into_iter().flatten())
        .find(|o| o["x"] == 0 && o["y"] == 0)
        .ok_or("Kein Hauptbildschirm gefunden.")?;
    let screen = Rect::new(0, 0, main["width"].as_i64().unwrap_or(0) as i32, main["height"].as_i64().unwrap_or(0) as i32);
    println!("Hauptbildschirm {}×{}, Median aus {runs} Screenshots", screen.width, screen.height);

    for (name, format) in [("PNG ", ImageFormat::Png), ("JPEG", ImageFormat::Jpeg)] {
        let request = ScreenshotRequest {
            format,
            quality: 90,
            cursor: false,
            area: Area::Screen,
            screen,
            folder: folder.clone(),
            clipboard: false,
        };
        let (mut capture, mut encode, mut write, mut total) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for _ in 0..runs {
            let shot = engine.screenshot(&request).map_err(|e| e.to_string())?;
            capture.push(shot.capture_ms);
            encode.push(shot.encode_ms);
            write.push(shot.write_ms);
            total.push(shot.total_ms);
        }
        let best = |v: &[f64]| v.iter().copied().fold(f64::MAX, f64::min);
        println!(
            "  {name}  Aufnahme {:>4.1} ms  Kodieren {:>5.1} ms  Schreiben {:>4.1} ms  gesamt {:>5.1} ms (Median), bester {:>5.1} ms",
            median(capture.clone()),
            median(encode.clone()),
            median(write.clone()),
            median(total.clone()),
            best(&total)
        );
    }
    let _ = std::fs::remove_dir_all(&folder);
    Ok(())
}

/// Arbeitsspeicher dieses Prozesses: (aktuell im RAM, privat zugesagt, Spitze im RAM) in Bytes.
#[cfg(windows)]
fn memory_now() -> (usize, usize, usize) {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private: usize,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut core::ffi::c_void, size: u32) -> i32;
    }
    let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, ..Default::default() };
    // SAFETY: Puffer passt zu PROCESS_MEMORY_COUNTERS_EX, das Pseudo-Handle gilt immer.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut Counters).cast(), c.cb) };
    (c.working_set, c.private, c.peak_working_set)
}

#[cfg(not(windows))]
fn memory_now() -> (usize, usize, usize) {
    (0, 0, 0)
}

/// Privater Arbeitssatz (das, was der Task-Manager als Arbeitsspeicher zeigt) in Bytes.
#[cfg(windows)]
fn private_working_set() -> usize {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        sizes: [usize; 9],
        private: usize,
        private_working_set: usize,
        shared_commit: u64,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut core::ffi::c_void, size: u32) -> i32;
    }
    let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, ..Default::default() };
    // SAFETY: Puffer passt zu PROCESS_MEMORY_COUNTERS_EX2 (ab Windows 10 1809).
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut Counters).cast(), c.cb) };
    c.private_working_set
}

/// CPU-Zeit dieses Prozesses (Kernel + Benutzer) in Sekunden.
#[cfg(windows)]
fn process_cpu_seconds() -> f64 {
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(process: isize, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    }
    let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
    // SAFETY: vier FILETIME-Werte (je 64 Bit), Pseudo-Handle des eigenen Prozesses.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    (k + u) as f64 / 1e7
}

/// Hauptbildschirm laut Systemprüfung.
fn main_screen(engine: &Engine) -> Result<Rect, String> {
    let report = engine.system_check().map_err(|e| e.to_string())?;
    let main = report["adapters"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|a| a["outputs"].as_array().into_iter().flatten())
        .find(|o| o["x"] == 0 && o["y"] == 0)
        .ok_or("Kein Hauptbildschirm gefunden.")?;
    Ok(Rect::new(0, 0, main["width"].as_i64().unwrap_or(0) as i32, main["height"].as_i64().unwrap_or(0) as i32))
}

/// Ton-Zeilen für den Status.
fn print_audio(status: &veloframe::AudioStatus) {
    if let Some(error) = &status.error {
        println!("        Ton: Fehler {error}");
    }
    println!("        Bildalter beim Abholen (Ausgleich für den Ton): {:.1} ms", status.video_lag_ms);
    for s in &status.sources {
        println!(
            "        Ton {:?} ({}, {}, Latenzausgleich {:.1} ms): Pegel {:.2}, Aussetzer {}, zu spät {:.0} ms, Stille gefüllt {:.0} ms, Drift {:.2} ms ({} Korrekturen), Gerätewechsel {}{}",
            s.source,
            s.capture,
            s.device.clone().unwrap_or_default(),
            s.latency_ms,
            s.level,
            s.dropouts,
            s.late_ms,
            s.silence_ms,
            s.drift_ms,
            s.drift_corrections,
            s.device_changes,
            s.warnings.iter().map(|w| format!(", Hinweis {}: {}", w.code, w.message)).collect::<String>()
        );
    }
}

#[cfg(windows)]
fn audio_devices() -> Result<(), String> {
    let engine = Engine::new().map_err(|e| e.to_string())?;
    for d in engine.audio_devices().map_err(|e| e.to_string())? {
        println!(
            "{:<6} {}{}{}\n       {}",
            d.kind,
            d.name,
            if d.default { "  [Standard]" } else { "" },
            if d.bluetooth { "  [Bluetooth]" } else { "" },
            d.id
        );
    }
    Ok(())
}

#[cfg(windows)]
fn clip(mode: veloframe::ClipMode, seconds: u32, run_for: u32, codec: Option<veloframe::VideoCodec>, audio: veloframe::AudioRequest) -> Result<(), String> {
    use veloframe::{BufferRequest, Quality, Resolution, SaveClipRequest};
    let mb = |b: f64| b / 1_048_576.0;
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    println!("Vor dem Start: Task-Manager {:.1} MB", mb(private_working_set() as f64));
    let screen = main_screen(&engine)?;
    let request = BufferRequest { mode, seconds, fps: 60, quality: Quality::High, resolution: Resolution::Native, codec, storage: None, screen, audio };
    let started = Instant::now();
    let status = engine.start_buffer(&request).map_err(|e| e.to_string())?;
    let plan = status.plan.clone().ok_or("kein Plan")?;
    println!(
        "Gestartet in {:.0} ms: {:?}, {:?} {}×{} @ {} fps, Obergrenze {:.1} Mbit/s, Mittel {:.1} Mbit/s, Puffer {:?}",
        started.elapsed().as_secs_f64() * 1000.0,
        plan.mode,
        plan.codec,
        plan.width,
        plan.height,
        plan.fps,
        plan.max_bitrate as f64 / 1e6,
        plan.mean_bitrate as f64 / 1e6,
        plan.storage
    );
    println!("Encoder: {}", status.encoder.clone().unwrap_or_default());
    println!("Angenommen: {}", status.applied.join(", "));
    match (&plan.audio, &status.audio) {
        (Some(a), Some(s)) => println!(
            "Ton: {:?} + Mikrofon {:?}, Spuren {:?}, {} kbit/s je Spur, Encoder {}",
            a.system,
            a.microphone,
            a.tracks,
            a.bitrate / 1000,
            s.encoder.clone().unwrap_or_else(|| s.error.clone().unwrap_or_default())
        ),
        _ => println!("Ton: aus"),
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    let cpu0 = process_cpu_seconds();
    let t0 = Instant::now();
    let mut peak = 0usize;
    for s in 1..=run_for {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let st = engine.buffer_status();
        let ram = private_working_set();
        peak = peak.max(ram);
        if s % 5 == 0 || s == run_for {
            println!(
                "  {s:>3} s: Puffer {:>5.1} s / {:>5.1} MB ({:.1} Mbit/s), davon im Engine-RAM {:.1} MB, Bilder {} kodiert / {} verworfen (Encoder belegt {}, Pool voll {}, zu spät {}), Aufwand {} ({}× angepasst), Task-Manager {:.1} MB{}",
                st.buffered_seconds,
                mb(st.buffered_bytes as f64),
                st.actual_bitrate as f64 / 1e6,
                mb(st.ram_bytes as f64),
                st.frames_encoded,
                st.frames_dropped,
                st.dropped_encoder_busy,
                st.dropped_pool_full,
                st.dropped_late,
                st.encoder_effort,
                st.effort_changes,
                mb(ram as f64),
                st.error.map(|e| format!(", Fehler: {e}")).unwrap_or_default()
            );
            if let Some(audio) = &st.audio {
                println!("        Ton im Puffer {:.1} s, RAM {:.2} MB", audio.buffered_seconds, mb(audio.ram_bytes as f64));
                print_audio(audio);
            }
        }
    }
    let cpu = (process_cpu_seconds() - cpu0) / t0.elapsed().as_secs_f64();
    println!("CPU während der Aufnahme: {:.1} % eines Kerns = {:.2} % der gesamten CPU ({} Kerne)", cpu * 100.0, cpu * 100.0 / cores, cores);
    println!("Task-Manager während der Aufnahme: höchstens {:.1} MB", mb(peak as f64));

    let clip = engine.save_clip(&SaveClipRequest { folder: folder.clone(), seconds: None }).map_err(|e| e.to_string())?;
    println!(
        "Gespeichert in {:.0} ms: {} ({:.1} MB, {:.2} s, {} Bilder, {:?} {}×{})",
        clip.save_ms,
        clip.path,
        mb(clip.bytes as f64),
        clip.duration_seconds,
        clip.frames,
        clip.codec,
        clip.width,
        clip.height
    );
    match veloframe_win::verify::check_video(std::path::Path::new(&clip.path), clip.frames) {
        Ok(check) => {
            println!("Windows-Decoder: {} Bilder, {:.2} s, {}×{} → abspielbar", check.frames, check.duration_seconds, check.width, check.height);
            if let Some(frame) = check.middle_frame {
                let png = std::path::Path::new(&clip.path).with_extension("mitte.png");
                let mut out = Vec::new();
                codec::png::encode(&frame, &mut out).map_err(|e| e.to_string())?;
                std::fs::write(&png, out).map_err(|e| e.to_string())?;
                println!("Bild aus der Mitte: {}", png.display());
            }
        }
        Err(e) => println!("Windows-Decoder konnte den Clip nicht prüfen: {e}"),
    }
    if !clip.audio_tracks.is_empty() {
        println!("Tonspuren: {:?}, {:.2} s", clip.audio_tracks, clip.audio_seconds);
        match veloframe_win::verify::check_audio(std::path::Path::new(&clip.path)) {
            Ok(Some(check)) => println!(
                "Windows-Decoder (Ton): {:.2} s, {} Hz, {} Kanäle, höchster Pegel {:.2}, erstes Sample bei {:.1} ms → abspielbar",
                check.seconds(),
                check.sample_rate,
                check.channels,
                check.peak(),
                check.first_time as f64 / 1e4
            ),
            Ok(None) => println!("Windows-Decoder findet keine Tonspur!"),
            Err(e) => println!("Windows-Decoder konnte den Ton nicht prüfen: {e}"),
        }
    }
    engine.stop_buffer();
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("Nach dem Beenden: Task-Manager {:.1} MB", mb(private_working_set() as f64));
    Ok(())
}

/// Prüft eine MP4-Datei mit dem Windows-Decoder (Bild und Ton).
#[cfg(windows)]
fn check_file(path: &str) -> Result<(), String> {
    veloframe_win::init_process();
    let path = std::path::Path::new(path);
    match veloframe_win::verify::check_video(path, 0) {
        Ok(v) => println!("Bild: {} Bilder, {:.3} s, {}×{}", v.frames, v.duration_seconds, v.width, v.height),
        Err(e) => println!("Bild: {e}"),
    }
    match veloframe_win::verify::check_audio(path).map_err(|e| e.to_string())? {
        Some(a) => println!("Ton: {:.3} s, erstes Sample bei {:.2} ms, Pegel {:.2}", a.seconds(), a.first_time as f64 / 1e4, a.peak()),
        None => println!("Ton: keiner"),
    }
    if let Some((empty, media)) = audio_edit(&std::fs::read(path).map_err(|e| e.to_string())?) {
        println!("Bearbeitungsliste Ton: {empty} ms leer, ab Sample {media}");
    }
    Ok(())
}

/// Bearbeitungsliste der ersten Tonspur: (leer vorne in ms, erstes gespieltes Sample).
fn audio_edit(file: &[u8]) -> Option<(i64, i64)> {
    fn children(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            let mut size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            let mut head = 8;
            if size == 1 {
                size = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()) as usize;
                head = 16;
            }
            if size < head || i + size > data.len() {
                break;
            }
            out.push((data[i + 4..i + 8].try_into().unwrap(), &data[i + head..i + size]));
            i += size;
        }
        out
    }
    fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
        children(data).into_iter().find(|(k, _)| k == kind).map(|(_, d)| d)
    }
    let moov = child(file, b"moov")?;
    let trak = children(moov).into_iter().filter(|(k, _)| k == b"trak").map(|(_, d)| d).nth(1)?;
    let elst = child(child(trak, b"edts")?, b"elst")?;
    let entries = u32::from_be_bytes(elst[4..8].try_into().unwrap()) as usize;
    let at = |i: usize, o: usize| i64::from_be_bytes(elst[8 + i * 20 + o..8 + i * 20 + o + 8].try_into().unwrap());
    let (mut empty, mut media) = (0, 0);
    for i in 0..entries {
        if at(i, 8) == -1 {
            empty += at(i, 0);
        } else {
            media = at(i, 8);
        }
    }
    Some((empty, media))
}

/// Sync-Test: Ein Fenster blitzt auf und gleichzeitig spielt ein Klick, mehrmals. Danach wird der
/// gespeicherte Clip dekodiert und gemessen, wie weit Klick und Blitz darin auseinanderliegen.
#[cfg(windows)]
fn sync(count: u32, only_self: bool, output: Option<String>) -> Result<(), String> {
    use veloframe::{AudioRequest, BufferRequest, ClipMode, Quality, Resolution, SaveClipRequest, SystemAudio, VideoCodec};
    use veloframe_win::audio::qpc_now;
    use veloframe_win::synctest::{Clicker, Flasher};
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    // H.264 ohne B-Frames: Die Bildzeiten sind dann eindeutig. Ton: alles (der Klick kommt von hier).
    let request = BufferRequest {
        mode: ClipMode::Long,
        seconds: count + 10,
        fps: 60,
        quality: Quality::Medium,
        resolution: Resolution::Native,
        codec: Some(VideoCodec::H264),
        storage: None,
        screen,
        // `self`: nur der Ton dieses Programms (der Klick), unabhängig von Musik o. Ä. im Hintergrund.
        audio: AudioRequest { system: if only_self { SystemAudio::Process(std::process::id()) } else { SystemAudio::All }, ..Default::default() },
    };
    engine.start_buffer(&request).map_err(|e| e.to_string())?;
    let area = Rect::new(screen.width / 2 - 240, screen.height / 2 - 135, 480, 270);
    let flasher = Flasher::new(area).map_err(|e| e.to_string())?;
    let clicker = Clicker::new(0.3, output).map_err(|e| e.to_string())?;
    let wait = |ms: u64| {
        let until = Instant::now() + std::time::Duration::from_millis(ms);
        while Instant::now() < until {
            flasher.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    flasher.show(false);
    // Etwas länger vorlaufen: Die Engine misst in dieser Zeit, wie alt ein Bild beim Abholen ist.
    wait(4000);
    println!("{count} Blitze mit Klick, je eine Sekunde Abstand …");
    let mut truth = Vec::new(); // (zu hören, zu sehen) in QPC-Zeit
    let mut device_lags = Vec::new();
    for _ in 0..count {
        let (heard, device_lag) = clicker.click().map_err(|e| e.to_string())?;
        device_lags.push(device_lag as f64 / 1e4);
        while qpc_now() < heard {
            std::hint::spin_loop();
        }
        let seen = flasher.show(true);
        truth.push((heard, seen));
        wait(150);
        flasher.show(false);
        wait(850);
    }
    wait(1000);
    let clip = engine.save_clip(&SaveClipRequest { folder, seconds: None }).map_err(|e| e.to_string())?;
    if let Some(audio) = engine.buffer_status().audio {
        print_audio(&audio);
    }
    engine.stop_buffer();
    let clicker_latency = clicker.stream_latency_ms;
    drop(clicker);
    drop(flasher);
    println!("Clip: {} ({:.1} s, Ton {:.1} s)", clip.path, clip.duration_seconds, clip.audio_seconds);
    println!(
        "Ausgabegerät: spielt im Mittel {:.1} ms nach dem Mischen (laut IAudioClock), Latenz des Streams {:.1} ms",
        device_lags.iter().sum::<f64>() / device_lags.len().max(1) as f64,
        clicker_latency
    );

    // Auswerten wie ein Player: Bild- und Tonzeiten aus dem Windows-Decoder, Bearbeitungsliste angewandt.
    let path = std::path::Path::new(&clip.path);
    let region = (
        (area.x + 60) as f32 / screen.width as f32,
        (area.y + 40) as f32 / screen.height as f32,
        (area.right() - 60) as f32 / screen.width as f32,
        (area.bottom() - 40) as f32 / screen.height as f32,
    );
    let frames = veloframe_win::verify::video_brightness(path, region).map_err(|e| e.to_string())?;
    let audio = veloframe_win::verify::check_audio(path).map_err(|e| e.to_string())?.ok_or("Der Clip hat keine Tonspur.")?;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let (empty_ms, media_time) = audio_edit(&bytes).ok_or("Bearbeitungsliste der Tonspur fehlt.")?;
    let rate = audio.sample_rate as f64;
    // Wendet der Windows-Decoder die Bearbeitungsliste nicht an, beginnt er bei 0: dann hier anwenden.
    let applied = audio.first_time != 0 || media_time == 0;
    println!(
        "Bearbeitungsliste Ton: {empty_ms} ms leer, ab Sample {media_time}; Windows-Decoder beginnt bei {:.1} ms ({})",
        audio.first_time as f64 / 1e4,
        if applied { "wendet sie an" } else { "ignoriert sie, wird hier angewandt" }
    );
    let audio_time = |i: usize| -> f64 {
        if applied { audio.first_time as f64 / 1e4 + i as f64 * 1000.0 / rate } else { empty_ms as f64 + (i as f64 - media_time as f64) * 1000.0 / rate }
    };
    let channels = audio.channels.max(1) as usize;
    let left: Vec<f32> = audio.samples.iter().step_by(channels).map(|&s| (s as f32 / 32768.0).abs()).collect();
    let quiet = (rate * 0.03) as usize;
    let mut clicks = Vec::new();
    let mut i = quiet;
    while i < left.len() {
        if left[i] > 0.12 && left[i - quiet..i].iter().all(|&x| x < 0.05) {
            clicks.push(audio_time(i));
            i += (rate * 0.3) as usize;
        } else {
            i += 1;
        }
    }
    let (lo, hi) = frames.iter().fold((f32::MAX, f32::MIN), |(lo, hi), f| (lo.min(f.1), hi.max(f.1)));
    let threshold = (lo + hi) / 2.0;
    let flashes: Vec<f64> = frames.windows(2).filter(|w| w[0].1 < threshold && w[1].1 >= threshold).map(|w| w[1].0 as f64 / 1e4).collect();
    println!("Gefunden: {} Blitze (Helligkeit {lo:.0}–{hi:.0}), {} Klicks", flashes.len(), clicks.len());
    let mut errors = Vec::new();
    for (k, &flash) in flashes.iter().enumerate() {
        let Some(&click) = clicks.iter().min_by(|a, b| (*a - flash).abs().total_cmp(&(*b - flash).abs())) else { continue };
        if (click - flash).abs() > 200.0 {
            continue;
        }
        let measured = click - flash;
        let expected = truth.get(k).map_or(0.0, |&(heard, seen)| (heard - seen) as f64 / 1e4);
        errors.push(measured - expected);
        println!(
            "  Blitz {:>2} bei {flash:>8.1} ms: Ton − Bild im Clip {measured:>+6.1} ms, in Wirklichkeit {expected:>+6.1} ms → Fehler {:>+6.1} ms",
            k + 1,
            measured - expected
        );
    }
    if errors.is_empty() {
        return Err("Keine Paare aus Blitz und Klick gefunden (läuft gerade anderer Ton?).".into());
    }
    let mean = errors.iter().sum::<f64>() / errors.len() as f64;
    let (min, max) = errors.iter().fold((f64::MAX, f64::MIN), |(a, b), &e| (a.min(e), b.max(e)));
    println!(
        "Versatz im Clip (Ton − Bild, gegenüber der Wirklichkeit): Mittel {mean:+.1} ms, Spanne {min:+.1} … {max:+.1} ms ({} Paare). Ein Bild dauert 16,7 ms.",
        errors.len()
    );
    Ok(())
}

fn memory() -> Result<(), String> {
    let folder = std::env::temp_dir().join("veloframe-bench-memory");
    let mb = |b: usize| b as f64 / 1_048_576.0;
    let mut last = memory_now();
    let mut step = |name: &str| {
        let now = memory_now();
        println!(
            "  {name:<36} RAM {:>6.1} MB ({:>+6.1})   privat {:>6.1} MB ({:>+6.1})   Spitze {:>6.1} MB",
            mb(now.0),
            mb(now.0) - mb(last.0),
            mb(now.1),
            mb(now.1) - mb(last.1),
            mb(now.2)
        );
        last = now;
    };
    println!("Arbeitsspeicher nach jedem Schritt (Veränderung zum vorigen in Klammern)
");
    step("Programmstart (noch keine Engine)");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    step("Engine::new()");
    let screens = engine.warm_up().map_err(|e| e.to_string())?;
    step(&format!("warm_up() ({screens} Bildschirme, 2 GPUs)"));
    let screen = Rect::new(0, 0, 1920, 1080);
    let shot = |engine: &mut Engine, format| {
        let request = ScreenshotRequest { format, quality: 90, cursor: false, area: Area::Screen, screen, folder: folder.clone(), clipboard: true };
        engine.screenshot(&request).map(|_| ()).map_err(|e| e.to_string())
    };
    shot(&mut engine, ImageFormat::Png)?;
    step("1. Screenshot (PNG, Zwischenablage)");
    for _ in 0..10 {
        shot(&mut engine, ImageFormat::Png)?;
        shot(&mut engine, ImageFormat::Jpeg)?;
    }
    step("nach 20 weiteren Screenshots");
    drop(engine);
    step("Engine freigegeben");
    let _ = std::fs::remove_dir_all(&folder);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("encode") => encode(&args[1..].iter().map(PathBuf::from).collect::<Vec<_>>()),
        Some("capture") => capture(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(20)),
        Some("memory") => memory(),
        #[cfg(windows)]
        Some("clip") => {
            let mode = match args.get(1).map(String::as_str) {
                Some("long") => veloframe::ClipMode::Long,
                _ => veloframe::ClipMode::Performance,
            };
            let seconds = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(30);
            let run_for = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(seconds + 5);
            let codec = match args.get(4).map(String::as_str) {
                Some("h264") => Some(veloframe::VideoCodec::H264),
                Some("hevc") => Some(veloframe::VideoCodec::Hevc),
                _ => None,
            };
            let audio = match args.get(5).map(String::as_str) {
                Some("off") => veloframe::AudioRequest { system: veloframe::SystemAudio::Off, ..Default::default() },
                Some("all") => veloframe::AudioRequest { system: veloframe::SystemAudio::All, ..Default::default() },
                Some("mic") => veloframe::AudioRequest {
                    system: veloframe::SystemAudio::All,
                    microphone: veloframe::Microphone::Default,
                    tracks: veloframe::TrackLayout::Both,
                    ..Default::default()
                },
                Some(pid) if pid.parse::<u32>().is_ok() => veloframe::AudioRequest { system: veloframe::SystemAudio::Process(pid.parse().unwrap()), ..Default::default() },
                _ => veloframe::AudioRequest::default(),
            };
            clip(mode, seconds, run_for, codec, audio)
        }
        #[cfg(windows)]
        Some("audio") => audio_devices(),
        #[cfg(windows)]
        Some("check") => args.get(1).map_or(Err("Datei fehlt.".to_string()), |p| check_file(p)),
        #[cfg(windows)]
        Some("sync") => sync(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(8), args.get(2).map(String::as_str) == Some("self"), args.get(3).cloned()),
        _ => Err("Aufruf: veloframe-bench encode <bild.png>...  |  capture [anzahl]  |  memory  |  clip [performance|long] [sekunden] [laufzeit] [auto|h264|hevc] [host|all|mic|off|<pid>]  |  audio  |  sync [anzahl]".into()),
    };
    if let Err(error) = result {
        eprintln!("Fehler: {error}");
        std::process::exit(1);
    }
}
