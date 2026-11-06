//! VeloFrame-Benchmark: misst die Engine, damit „schnell“ eine Zahl ist und keine Behauptung.
//!
//! ```text
//! veloframe-bench encode <bild.png>...   Encoder vergleichen (bisher, 1 Kern  vs.  VeloFrame, alle Kerne)
//! veloframe-bench capture [anzahl]       echte Screenshots des Hauptbildschirms über die Engine
//! veloframe-bench memory                 Arbeitsspeicher nach jedem Schritt (Start, Engine, Vorwärmen, Screenshots)
//! veloframe-bench clip [modus] [sek] [laufzeit] [codec] [ton]   Clip-Puffer messen (RAM, CPU, Bitrate), Clip speichern und prüfen
//! veloframe-bench audio                  Tongeräte auflisten
//! veloframe-bench sync [anzahl]          Sync-Test: Blitz + Klick aufnehmen, Versatz im gespeicherten Clip messen
//! veloframe-bench cursor [auflösung]     Mauszeiger im Video: Hotspot, Umschalten, CPU (bewegt die Maus kurz)
//! veloframe-bench mic [default|<id>] [both|separate]   Mikrofon: Spuren, Stummschalten, Pegel im Clip
//! veloframe-bench start [performance|long] [h264|hevc]   bildgenauer Clip-Anfang: Länge, Art, Speicherzeit
//! veloframe-bench longrun [performance|long] [minuten]   Langzeitlauf: jede Minute RAM, CPU, Bilder, Drift, Handles (CSV)
//! veloframe-bench cursorload [runden]   verworfene Bilder mit/ohne Zeiger im selben Puffer (alle 15 s umgeschaltet)
//! veloframe-bench robust [anzahl]        Start/Stopp mit Ton (Handles, Speicher), dann ohne AAC-Encoder
//! veloframe-bench devices [sekunden]     Gerätewechsel live (Headset ab-/anstecken, Standardgerät umstellen)
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

/// Hauptbildschirm laut Systemprüfung. `VELOFRAME_BENCH_SCREEN=x,y,breite,höhe` wählt einen anderen
/// (z. B. den Bildschirm an einer zweiten Grafikkarte, um deren Encoder zu messen).
fn main_screen(engine: &Engine) -> Result<Rect, String> {
    if let Ok(value) = std::env::var("VELOFRAME_BENCH_SCREEN") {
        let n: Vec<i32> = value.split(',').filter_map(|v| v.trim().parse().ok()).collect();
        if let [x, y, w, h] = n[..] {
            return Ok(Rect::new(x, y, w, h));
        }
        return Err("VELOFRAME_BENCH_SCREEN erwartet x,y,breite,höhe.".into());
    }
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
    let cursor = std::env::var("VELOFRAME_BENCH_CURSOR").map_or(true, |v| v != "off");
    let request = BufferRequest { mode, seconds, fps: 60, quality: Quality::High, resolution: Resolution::Native, codec, storage: None, screen, audio, cursor };
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
    println!("Anfang: {:?}, {:+.1} ms neben der gewünschten Zeit, Neu-Kodieren {:.0} ms", clip.start, clip.start_offset_ms, clip.start_encode_ms);
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
        Ok(v) => println!("Bild: {} Bilder, {:.3} s, {}×{}, erstes Bild bei {:.2} ms", v.frames, v.duration_seconds, v.width, v.height, v.first_time as f64 / 1e4),
        Err(e) => println!("Bild: {e}"),
    }
    match veloframe_win::verify::check_audio(path).map_err(|e| e.to_string())? {
        Some(a) => println!("Ton: {:.3} s, erstes Sample bei {:.2} ms, Pegel {:.2}", a.seconds(), a.first_time as f64 / 1e4, a.peak()),
        None => println!("Ton: keiner"),
    }
    let tracks = veloframe_win::verify::check_audio_tracks(path).map_err(|e| e.to_string())?;
    if tracks.len() > 1 {
        for (n, t) in tracks.iter().enumerate() {
            let levels: Vec<String> = t.rms_db(0.5).iter().map(|db| format!("{db:>4.0}")).collect();
            println!("  Tonstrom {} ({}{}): {:.2} s, Pegel je 0,5 s: {}", n + 1, t.stream, if t.selected { ", spielt von sich aus" } else { "" }, t.seconds(), levels.join(" "));
        }
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
    // Die erste Tonspur steht als letzte `trak`-Box in der Datei (siehe mp4.rs).
    let traks: Vec<&[u8]> = children(moov).into_iter().filter(|(k, _)| k == b"trak").map(|(_, d)| d).collect();
    let trak = *traks.last().filter(|_| traks.len() > 1)?;
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
fn sync(count: u32, only_self: bool, output: Option<String>, mode: veloframe::ClipMode, codec: veloframe::VideoCodec) -> Result<(), String> {
    use veloframe::{AudioRequest, BufferRequest, Quality, Resolution, SaveClipRequest, SystemAudio};
    use veloframe_win::audio::qpc_now;
    use veloframe_win::synctest::{Clicker, Flasher};
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    // Standard: Long + H.264 ohne B-Frames. `performance h264` prüft Clips mit B-Frames (Bearbeitungsliste
    // und Zeitversätze der Videospur). Ton: alles (der Klick kommt von hier).
    let request = BufferRequest {
        mode,
        seconds: (count + 10).max(5).min(mode.seconds_range().1),
        fps: 60,
        quality: Quality::Medium,
        resolution: Resolution::Native,
        codec: Some(codec),
        storage: None,
        screen,
        // `self`: nur der Ton dieses Programms (der Klick), unabhängig von Musik o. Ä. im Hintergrund.
        audio: AudioRequest { system: if only_self { SystemAudio::Process(std::process::id()) } else { SystemAudio::All }, ..Default::default() },
        // Der Zeiger könnte über dem Blitz liegen und die Helligkeit verfälschen.
        cursor: false,
    };
    let started = engine.start_buffer(&request).map_err(|e| e.to_string())?;
    println!("{:?} {:?}, Encoder {}: {}", mode, codec, started.encoder.unwrap_or_default(), started.applied.join(", "));
    let area = Rect::new(screen.x + screen.width / 2 - 240, screen.y + screen.height / 2 - 135, 480, 270);
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
        (area.x - screen.x + 60) as f32 / screen.width as f32,
        (area.y - screen.y + 40) as f32 / screen.height as f32,
        (area.right() - screen.x - 60) as f32 / screen.width as f32,
        (area.bottom() - screen.y - 40) as f32 / screen.height as f32,
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

/// Bewegt den Mauszeiger (für den Zeiger-Test). Gibt die vorige Position zurück.
#[cfg(windows)]
fn move_mouse(x: i32, y: i32) -> (i32, i32) {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    unsafe extern "system" {
        fn GetCursorPos(point: *mut Point) -> i32;
        fn SetCursorPos(x: i32, y: i32) -> i32;
    }
    let mut old = Point { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut old);
        SetCursorPos(x, y);
    }
    (old.x, old.y)
}

/// Zeiger-Test: Ein Fenster (weiß, dann schwarz, dann weiß) liegt unter dem Mauszeiger, der Zeiger
/// wird per `set_cursor` aus- und wieder eingeschaltet, dann wechselt die Form vom Fadenkreuz zum
/// Pfeil. Im gespeicherten Clip wird gemessen: Lage des Hotspots, wann das Umschalten wirkt, und am
/// Ende die CPU mit und ohne Zeiger bei bewegter Maus.
#[cfg(windows)]
fn cursor_test(resolution: veloframe::Resolution) -> Result<(), String> {
    use veloframe::{AudioRequest, BufferRequest, ClipMode, Quality, SaveClipRequest, SystemAudio, VideoCodec};
    use veloframe_win::synctest::Flasher;
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    let request = BufferRequest {
        mode: ClipMode::Long,
        seconds: 10,
        fps: 60,
        quality: Quality::High,
        resolution,
        codec: Some(VideoCodec::H264),
        storage: None,
        screen,
        audio: AudioRequest { system: SystemAudio::Off, ..Default::default() },
        cursor: true,
    };
    let status = engine.start_buffer(&request).map_err(|e| e.to_string())?;
    let plan = status.plan.clone().ok_or("kein Plan")?;
    println!("Video {}×{} von {}×{}, Zeiger: {}", plan.width, plan.height, screen.width, screen.height, status.cursor_error.clone().unwrap_or_else(|| "möglich".into()));
    let area = Rect::new(screen.x + screen.width / 2 - 240, screen.y + screen.height / 2 - 135, 480, 270);
    let flasher = Flasher::new(area).map_err(|e| e.to_string())?;
    // Ungerade Lage, damit Rundungsfehler beim Skalieren auffallen.
    let target = (area.x + 237, area.y + 131);
    let old = move_mouse(target.0, target.1);
    flasher.set_cursor(true);
    flasher.show(true);
    let wait = |ms: u64| {
        let until = Instant::now() + std::time::Duration::from_millis(ms);
        while Instant::now() < until {
            flasher.tick();
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    };
    wait(2500);
    // Gleichzeitig schwarz werden und den Zeiger abschalten, eine Sekunde später umgekehrt.
    flasher.show(false);
    let off = engine.set_cursor(false).map_err(|e| e.to_string())?;
    wait(1000);
    flasher.show(true);
    let on = engine.set_cursor(true).map_err(|e| e.to_string())?;
    wait(1000);
    flasher.set_cursor(false);
    move_mouse(target.0 + 1, target.1);
    move_mouse(target.0, target.1);
    wait(1200);
    println!("buffer-status.cursor nach cursor-set: aus → {}, an → {}", off.cursor, on.cursor);
    let clip = engine.save_clip(&SaveClipRequest { folder, seconds: None }).map_err(|e| e.to_string())?;

    // CPU: je 8 s mit Zeiger an und aus, die Maus kreist dabei (wie beim Arbeiten auf dem Desktop).
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    let mut cpu = Vec::new();
    for visible in [true, false, true, false] {
        engine.set_cursor(visible).map_err(|e| e.to_string())?;
        let (cpu0, t0) = (process_cpu_seconds(), Instant::now());
        let mut step = 0f64;
        while t0.elapsed().as_secs_f64() < 8.0 {
            step += 0.05;
            move_mouse(target.0 + (step.cos() * 60.0) as i32, target.1 + (step.sin() * 60.0) as i32);
            flasher.pump();
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        cpu.push((visible, (process_cpu_seconds() - cpu0) / t0.elapsed().as_secs_f64() * 100.0 / cores));
    }
    move_mouse(old.0, old.1);
    drop(flasher);
    engine.stop_buffer();
    println!("Clip: {} ({:.1} s)", clip.path, clip.duration_seconds);

    // Auswerten: Ausschnitt um die erwartete Stelle, umgerechnet auf die Videogröße.
    let scale = plan.width as f64 / screen.width as f64;
    let expected = (((target.0 - screen.x) as f64 * scale), ((target.1 - screen.y) as f64 * scale));
    let half = (48.0 * scale).ceil() as i32;
    let crop = Rect::new(expected.0 as i32 - half, expected.1 as i32 - half, 2 * half, 2 * half);
    let frames = veloframe_win::verify::video_crops(std::path::Path::new(&clip.path), crop).map_err(|e| e.to_string())?;
    // Hintergrund aus dem Rand des Ausschnitts, Zeigerpixel heben sich davon ab.
    let analyse = |gray: &[u8]| -> (bool, usize, Option<(i32, i32, i32, i32)>) {
        let w = crop.width as usize;
        let border: u32 = (0..w).map(|x| gray[x] as u32).sum::<u32>() / w as u32;
        let white = border > 128;
        let mut bbox: Option<(i32, i32, i32, i32)> = None;
        let mut count = 0;
        for (i, &g) in gray.iter().enumerate() {
            let hit = if white { g < 100 } else { g > 155 };
            if hit {
                count += 1;
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                bbox = Some(bbox.map_or((x, y, x, y), |(a, b, c, d)| (a.min(x), b.min(y), c.max(x), d.max(y))));
            }
        }
        (white, count, bbox)
    };
    let rows: Vec<(f64, bool, usize, Option<(i32, i32, i32, i32)>)> =
        frames.iter().map(|(t, g)| { let (w, c, b) = analyse(g); (*t as f64 / 1e4, w, c, b) }).collect();
    let present = |c: usize| c > (8.0 * scale * scale) as usize;
    // Fadenkreuz: Mitte der dunklen Pixel = Hotspot. Pfeil: Spitze (links oben) = Hotspot.
    let to_screen = |v: i32, axis: f64| (crop.x as f64 * (1.0 - axis) + crop.y as f64 * axis + v as f64) / scale;
    // Das Fenster ist anfangs kurz schwarz: erst die weiße Phase suchen, dann den Wechsel nach schwarz.
    let first_white = rows.iter().position(|r| r.1).ok_or("Weiße Phase nicht gefunden.")?;
    let first_black = first_white + rows[first_white..].iter().position(|r| !r.1).ok_or("Schwarze Phase nicht gefunden.")?;
    let back_white = first_black + rows[first_black..].iter().position(|r| r.1).ok_or("Zweite weiße Phase nicht gefunden.")?;
    let cross: Vec<_> = rows[..first_black].iter().rev().skip(5).take(30).filter_map(|r| r.3).collect();
    let arrow: Vec<_> = rows[rows.len().saturating_sub(30)..].iter().filter_map(|r| r.3).collect();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    // Beide Zeiger als Bild (Ausschnitt, vergrößert) neben den Clip legen: Ränder und Durchsichtigkeit
    // lassen sich so mit eigenen Augen prüfen.
    for (name, index) in [("fadenkreuz", first_black.saturating_sub(10)), ("pfeil", frames.len() - 10)] {
        let zoom = 4;
        let mut image = RgbImage::default();
        image.reset(crop.width as usize * zoom, crop.height as usize * zoom);
        for (i, &g) in frames[index].1.iter().enumerate() {
            let (x, y) = (i % crop.width as usize, i / crop.width as usize);
            for dy in 0..zoom {
                let row = (y * zoom + dy) * image.width;
                for dx in 0..zoom {
                    let at = (row + x * zoom + dx) * 3;
                    image.data[at..at + 3].fill(g);
                }
            }
        }
        let mut png = Vec::new();
        codec::png::encode(&image, &mut png).map_err(|e| e.to_string())?;
        let file = std::path::Path::new(&clip.path).with_extension(format!("{name}.png"));
        std::fs::write(&file, png).map_err(|e| e.to_string())?;
        println!("Zeiger als Bild: {}", file.display());
    }
    if let (false, false) = (cross.is_empty(), arrow.is_empty()) {
        let cx = mean(&cross.iter().map(|b| to_screen(b.0 + b.2, 0.0) / 2.0 + crop.x as f64 / scale / 2.0).collect::<Vec<_>>());
        let cy = mean(&cross.iter().map(|b| to_screen(b.1 + b.3, 1.0) / 2.0 + crop.y as f64 / scale / 2.0).collect::<Vec<_>>());
        let ax = mean(&arrow.iter().map(|b| to_screen(b.0, 0.0)).collect::<Vec<_>>());
        let ay = mean(&arrow.iter().map(|b| to_screen(b.1, 1.0)).collect::<Vec<_>>());
        let (tx, ty) = ((target.0 - screen.x) as f64, (target.1 - screen.y) as f64);
        println!("Fadenkreuz: Mitte bei ({cx:.1}, {cy:.1}), Maus bei ({tx}, {ty}) → Abweichung ({:+.1}, {:+.1}) px", cx - tx, cy - ty);
        println!("Pfeil: Spitze bei ({ax:.1}, {ay:.1}) → Abweichung ({:+.1}, {:+.1}) px", ax - tx, ay - ty);
    } else {
        println!("Zeiger im Video nicht gefunden (Fadenkreuz {} Bilder, Pfeil {} Bilder).", cross.len(), arrow.len());
    }
    // Umschalten: Bild und Zeiger wechseln im selben Moment; gezählt wird, wie viele Bilder danach der
    // Zeiger noch (bzw. noch nicht) zu sehen ist.
    let lag_off = rows[first_black..].iter().take_while(|r| present(r.2)).count();
    let lag_on = rows[back_white..].iter().take_while(|r| !present(r.2)).count();
    println!(
        "Ausschalten: Zeiger noch {lag_off} Bild(er) nach dem Bildwechsel zu sehen; Einschalten: {lag_on} Bild(er) ohne Zeiger nach dem Bildwechsel (Bild bei {:.0} / {:.0} ms)",
        rows[first_black].0, rows[back_white].0
    );
    let during_off = rows[first_black + 3..back_white.saturating_sub(1)].iter().filter(|r| present(r.2)).count();
    println!("Während ausgeschaltet: {during_off} von {} Bildern mit Zeiger", back_white.saturating_sub(first_black + 4));
    for (visible, c) in &cpu {
        println!("CPU mit bewegter Maus, Zeiger {}: {c:.2} % der gesamten CPU", if *visible { "an " } else { "aus" });
    }
    let on: Vec<f64> = cpu.iter().filter(|c| c.0).map(|c| c.1).collect();
    let off: Vec<f64> = cpu.iter().filter(|c| !c.0).map(|c| c.1).collect();
    println!("Mehrkosten des Zeigers: {:+.3} % der gesamten CPU", mean(&on) - mean(&off));
    Ok(())
}

/// Mikrofon-Test: System + Mikrofon (Standard oder Geräte-ID) in drei Spuren (`both`) oder je Quelle
/// (`separate`). Erst das System (2–4 s), dann das Mikrofon (5–7 s) stummschalten (`audio-set`). Danach werden alle
/// Tonspuren mit dem Windows-Decoder gelesen: Pegel je halbe Sekunde.
#[cfg(windows)]
fn mic_test(microphone: veloframe::Microphone, tracks: veloframe::TrackLayout) -> Result<(), String> {
    use veloframe::{AudioRequest, AudioSetRequest, BufferRequest, ClipMode, Quality, Resolution, SaveClipRequest, SourceControl, SystemAudio};
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    let request = BufferRequest {
        mode: ClipMode::Performance,
        seconds: 15,
        fps: 60,
        quality: Quality::Medium,
        resolution: Resolution::Native,
        codec: None,
        storage: None,
        screen,
        audio: AudioRequest { system: SystemAudio::All, microphone, tracks, ..Default::default() },
        cursor: true,
    };
    let status = engine.start_buffer(&request).map_err(|e| e.to_string())?;
    if let Some(audio) = &status.audio {
        println!("Spuren: {:?}, Encoder {}", audio.tracks, audio.encoder.clone().unwrap_or_else(|| audio.error.clone().unwrap_or_default()));
    }
    let mute = |muted: bool| AudioSetRequest { system: None, microphone: Some(SourceControl { gain: None, muted: Some(muted) }) };
    let system = |muted: bool| AudioSetRequest { system: Some(SourceControl { gain: None, muted: Some(muted) }), microphone: None };
    // System bei 2–4 s stumm, Mikrofon bei 5–7 s: so ist jede Spur an ihren Lücken zu erkennen.
    std::thread::sleep(std::time::Duration::from_secs(2));
    engine.set_audio(&system(true)).map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    engine.set_audio(&system(false)).map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    let muted = engine.set_audio(&mute(true)).map_err(|e| e.to_string())?;
    println!("Stumm: {:?}", muted.sources.iter().map(|s| (s.source, s.muted)).collect::<Vec<_>>());
    std::thread::sleep(std::time::Duration::from_secs(2));
    engine.set_audio(&mute(false)).map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    let clip = engine.save_clip(&SaveClipRequest { folder, seconds: None }).map_err(|e| e.to_string())?;
    if let Some(audio) = engine.buffer_status().audio {
        print_audio(&audio);
    }
    engine.stop_buffer();
    println!("Clip: {} ({:.1} s), Spuren laut Engine: {:?}", clip.path, clip.duration_seconds, clip.audio_tracks);
    let decoded = veloframe_win::verify::check_audio_tracks(std::path::Path::new(&clip.path)).map_err(|e| e.to_string())?;
    println!("Windows-Decoder findet {} Tonspur(en):", decoded.len());
    for (n, track) in decoded.iter().enumerate() {
        let levels: Vec<String> = track.rms_db(0.5).iter().map(|db| format!("{db:>4.0}")).collect();
        println!(
            "  Spur {} (Strom {}{}): {:.2} s, Pegel je 0,5 s (dBFS): {}",
            n + 1,
            track.stream,
            if track.selected { ", spielt von sich aus" } else { "" },
            track.seconds(),
            levels.join(" ")
        );
    }
    Ok(())
}

/// Offene Handles dieses Prozesses (Dateien, Threads, Ereignisse, GPU-Objekte …).
#[cfg(windows)]
fn handle_count() -> u32 {
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
    }
    let mut count = 0;
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    count
}

/// Robustheit: den Puffer `cycles`-mal mit Ton (System + Mikrofon) starten und stoppen, jeden zehnten
/// Durchgang einen Clip speichern; Handles und Speicher vorher/nachher. Danach ohne AAC-Encoder (wie
/// Windows N ohne Media Feature Pack): Das Video muss weiterlaufen, `audio.error` erklärt es.
#[cfg(windows)]
fn robust(cycles: u32) -> Result<(), String> {
    use veloframe::{AudioRequest, BufferRequest, ClipMode, Microphone, Quality, Resolution, SaveClipRequest, SystemAudio, TrackLayout};
    let mb = |b: usize| b as f64 / 1_048_576.0;
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    let request = |audio: AudioRequest| BufferRequest {
        mode: ClipMode::Performance,
        seconds: 5,
        fps: 60,
        quality: Quality::High,
        resolution: Resolution::Native,
        codec: Some(veloframe::VideoCodec::H264), // der Windows-Decoder prüft die Clips (HEVC braucht eine Store-Erweiterung)
        storage: None,
        screen,
        audio,
        cursor: true,
    };
    let with_audio = AudioRequest { system: SystemAudio::All, microphone: Microphone::Default, tracks: TrackLayout::Both, ..Default::default() };
    // Ein Durchgang zum Aufwärmen: Media Foundation, Treiber und Encoder laden einmalig Bibliotheken.
    engine.start_buffer(&request(with_audio.clone())).map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(1500));
    engine.stop_buffer();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (handles0, ram0) = (handle_count(), private_working_set());
    println!("Nach dem Aufwärmen: {handles0} Handles, Task-Manager {:.1} MB", mb(ram0));
    let started = Instant::now();
    let mut failures = 0;
    for cycle in 1..=cycles {
        let status = engine.start_buffer(&request(with_audio.clone())).map_err(|e| format!("Start {cycle}: {e}"))?;
        if status.audio.as_ref().is_none_or(|a| a.error.is_some()) {
            failures += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        if cycle % 10 == 0 {
            let clip = engine.save_clip(&SaveClipRequest { folder: folder.clone(), seconds: None }).map_err(|e| format!("Speichern {cycle}: {e}"))?;
            let check = veloframe_win::verify::check_video(std::path::Path::new(&clip.path), clip.frames).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&clip.path);
            engine.stop_buffer();
            std::thread::sleep(std::time::Duration::from_millis(300));
            println!(
                "  {cycle:>3} Durchgänge ({:.0} s): {} Handles ({:+}), Task-Manager {:.1} MB ({:+.1}), letzter Clip {} Bilder / {:?} → {} Bilder lesbar",
                started.elapsed().as_secs_f64(),
                handle_count(),
                handle_count() as i64 - handles0 as i64,
                mb(private_working_set()),
                mb(private_working_set()) - mb(ram0),
                clip.frames,
                clip.audio_tracks,
                check.frames
            );
        } else {
            engine.stop_buffer();
        }
    }
    println!("{cycles} × Start/Stopp ohne Absturz, Ton fehlte {failures}×");

    // Ohne AAC-Encoder.
    unsafe { std::env::set_var("VELOFRAME_SIMULATE_NO_AAC", "1") };
    let status = engine.start_buffer(&request(AudioRequest::default())).map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    let status2 = engine.buffer_status();
    let clip = engine.save_clip(&SaveClipRequest { folder, seconds: None }).map_err(|e| e.to_string())?;
    engine.stop_buffer();
    unsafe { std::env::remove_var("VELOFRAME_SIMULATE_NO_AAC") };
    println!(
        "Ohne AAC-Encoder: Start ok (running {}), nach 3 s {} Bilder kodiert, audio.error = {:?}; Clip {:.1} s, {} Bilder, Tonspuren {:?}",
        status.running,
        status2.frames_encoded,
        status2.audio.and_then(|a| a.error),
        clip.duration_seconds,
        clip.frames,
        clip.audio_tracks
    );
    let _ = std::fs::remove_file(&clip.path);
    Ok(())
}

/// Gerätewechsel live: nimmt `seconds` lang mit System + Mikrofon auf und meldet jede Änderung
/// (Gerät, Gerätewechsel, gefüllte Stille). Währenddessen von Hand: Headset-Dongle ab- und wieder
/// anstecken, Standard-Ausgabe und -Eingabe in Windows umstellen. Am Ende wird der Clip geprüft.
#[cfg(windows)]
fn devices_test(seconds: u32) -> Result<(), String> {
    use veloframe::{AudioRequest, BufferRequest, ClipMode, Microphone, Quality, Resolution, SaveClipRequest, SystemAudio, TrackLayout};
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    let request = BufferRequest {
        mode: ClipMode::Performance,
        seconds: seconds.clamp(5, 60),
        fps: 60,
        quality: Quality::Medium,
        resolution: Resolution::Native,
        codec: None,
        storage: None,
        screen,
        audio: AudioRequest { system: SystemAudio::All, microphone: Microphone::Default, tracks: TrackLayout::Both, ..Default::default() },
        cursor: true,
    };
    engine.start_buffer(&request).map_err(|e| e.to_string())?;
    println!("Läuft {seconds} s. Jetzt Headset ab-/anstecken oder das Standardgerät umstellen …");
    let started = Instant::now();
    let mut last: Vec<(String, u32, f64, bool)> = Vec::new();
    let mut frames0 = 0;
    while started.elapsed().as_secs() < seconds as u64 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let status = engine.buffer_status();
        let Some(audio) = status.audio else { continue };
        let now: Vec<(String, u32, f64, bool)> =
            audio.sources.iter().map(|s| (s.device.clone().unwrap_or_default(), s.device_changes, s.silence_ms, s.active)).collect();
        for (i, source) in audio.sources.iter().enumerate() {
            let before = last.get(i);
            if before.is_none_or(|b| b.0 != now[i].0 || b.1 != now[i].1 || b.3 != now[i].3) {
                println!(
                    "  {:>6.1} s: {:?} → {} (aktiv {}, Wechsel {}, Stille gefüllt bisher {:.0} ms, Video {} Bilder {}){}",
                    started.elapsed().as_secs_f64(),
                    source.source,
                    now[i].0,
                    now[i].3,
                    now[i].1,
                    now[i].2,
                    status.frames_encoded,
                    if status.frames_encoded > frames0 { "läuft" } else { "steht!" },
                    source.warnings.iter().map(|w| format!(", Hinweis {}: {}", w.code, w.message)).collect::<String>()
                );
            }
        }
        frames0 = status.frames_encoded;
        last = now;
    }
    let clip = engine.save_clip(&SaveClipRequest { folder, seconds: None }).map_err(|e| e.to_string())?;
    if let Some(audio) = engine.buffer_status().audio {
        print_audio(&audio);
    }
    let status = engine.buffer_status();
    engine.stop_buffer();
    println!("Video: {} Bilder kodiert, {} verworfen", status.frames_encoded, status.frames_dropped);
    println!("Clip: {} ({:.1} s, Ton {:.1} s, Spuren {:?})", clip.path, clip.duration_seconds, clip.audio_seconds, clip.audio_tracks);
    let video = veloframe_win::verify::check_video(std::path::Path::new(&clip.path), clip.frames).map_err(|e| e.to_string())?;
    println!("Windows-Decoder: {} Bilder, {:.2} s", video.frames, video.duration_seconds);
    for (n, track) in veloframe_win::verify::check_audio_tracks(std::path::Path::new(&clip.path)).map_err(|e| e.to_string())?.iter().enumerate() {
        let levels: Vec<String> = track.rms_db(0.5).iter().map(|db| format!("{db:>4.0}")).collect();
        println!("  Tonspur {}: {:.2} s, Pegel je 0,5 s: {}", n + 1, track.seconds(), levels.join(" "));
    }
    Ok(())
}

/// Clip-Anfang: speichert im selben Puffer mehrere Clips verschiedener Länge und prüft, ob jeder
/// genau so lang ist wie verlangt (bildgenauer Anfang), wie er beginnt und wie lange das Speichern
/// dauert. `codec`: H.264 (Windows-Decoder prüft) oder HEVC.
#[cfg(windows)]
fn start_test(mode: veloframe::ClipMode, codec: veloframe::VideoCodec) -> Result<(), String> {
    use veloframe::{AudioRequest, BufferRequest, Quality, Resolution, SaveClipRequest, SystemAudio};
    let folder = std::env::temp_dir().join("veloframe-clips");
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    let request = BufferRequest {
        mode,
        seconds: 15,
        fps: 60,
        quality: Quality::High,
        resolution: Resolution::Native,
        codec: Some(codec),
        storage: None,
        screen,
        audio: AudioRequest { system: SystemAudio::All, ..Default::default() },
        cursor: true,
    };
    let status = engine.start_buffer(&request).map_err(|e| e.to_string())?;
    let plan = status.plan.ok_or("kein Plan")?;
    println!("{:?} {:?}, Keyframe alle {} Bilder, Encoder {}", plan.mode, plan.codec, plan.gop_frames, status.encoder.unwrap_or_default());
    std::thread::sleep(std::time::Duration::from_secs(17));
    for seconds in [Some(5), Some(7), None, Some(3), Some(10), Some(4)] {
        let clip = engine.save_clip(&SaveClipRequest { folder: folder.clone(), seconds }).map_err(|e| e.to_string())?;
        let wanted = seconds.unwrap_or(plan.seconds) as f64;
        let check = if codec == veloframe::VideoCodec::H264 {
            // Naht: Helligkeit jedes Bildes; ein kaputtes Bild am Übergang wäre ein Sprung.
            let jump = veloframe_win::verify::video_brightness(std::path::Path::new(&clip.path), (0.0, 0.0, 1.0, 1.0))
                .map(|b| b.windows(2).enumerate().map(|(i, w)| ((w[1].1 - w[0].1).abs(), i + 1)).fold((0.0f32, 0), |a, x| if x.0 > a.0 { x } else { a }))
                .unwrap_or((f32::NAN, 0));
            veloframe_win::verify::check_video(std::path::Path::new(&clip.path), clip.frames)
                .map(|c| format!("Windows-Decoder {} Bilder, {:.3} s, erstes Bild bei {:.1} ms, größter Helligkeitssprung {:.2} bei Bild {}", c.frames, c.duration_seconds, c.first_time as f64 / 1e4, jump.0, jump.1))
                .unwrap_or_else(|e| format!("Windows-Decoder: {e}"))
        } else {
            "HEVC: Windows-Decoder fehlt".into()
        };
        println!(
            "  {:>4.0} s verlangt: {:.3} s, {} Bilder, Anfang {:?} ({:+.1} ms), Ton {:.3} s, gespeichert in {:.0} ms (davon Neu-Kodieren {:.0} ms); {check}",
            wanted,
            clip.duration_seconds,
            clip.frames,
            clip.start,
            clip.start_offset_ms,
            clip.audio_seconds,
            clip.save_ms,
            clip.start_encode_ms
        );
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
    engine.stop_buffer();
    Ok(())
}

/// Langzeitlauf: `minutes` Minuten Aufnahme mit Ton, jede Minute eine Zeile (RAM, CPU, Bilder,
/// Aufwand, Ton-Drift, Aussetzer, Handles), auch als CSV neben die Clips. Am Ende ein Clip zur Prüfung.
#[cfg(windows)]
fn longrun(mode: veloframe::ClipMode, minutes: u32) -> Result<(), String> {
    use std::io::Write as _;
    use veloframe::{AudioRequest, BufferRequest, Quality, Resolution, SaveClipRequest};
    let mb = |b: usize| b as f64 / 1_048_576.0;
    let folder = std::env::temp_dir().join("veloframe-clips");
    let _ = std::fs::create_dir_all(&folder);
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    let seconds = if mode == veloframe::ClipMode::Long { 120 } else { 30 };
    let request = BufferRequest {
        mode,
        seconds,
        fps: 60,
        quality: Quality::High,
        resolution: Resolution::Native,
        codec: None,
        storage: None,
        screen,
        audio: AudioRequest::default(),
        cursor: true,
    };
    let status = engine.start_buffer(&request).map_err(|e| e.to_string())?;
    let plan = status.plan.ok_or("kein Plan")?;
    let csv_path = folder.join(format!("longrun-{:?}-{}.csv", mode, std::process::id()).to_lowercase());
    let mut csv = std::fs::File::create(&csv_path).map_err(|e| e.to_string())?;
    let _ = writeln!(csv, "minute,ram_mb,cpu_percent,frames_encoded,frames_dropped,effort,audio_drift_ms,audio_late_ms,audio_dropouts,handles,screen_updates,capture_restarts");
    println!("Langzeitlauf {:?} {:?} {} min, Puffer {} s, CSV: {}", plan.mode, plan.codec, minutes, seconds, csv_path.display());
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    let (mut cpu0, mut t0) = (process_cpu_seconds(), Instant::now());
    let (mut enc0, mut drop0, mut upd0) = (0u64, 0u64, 0u64);
    let (mut busy0, mut pool0, mut late0) = (0u64, 0u64, 0u64);
    for minute in 1..=minutes {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let st = engine.buffer_status();
        let cpu = (process_cpu_seconds() - cpu0) / t0.elapsed().as_secs_f64() * 100.0 / cores;
        (cpu0, t0) = (process_cpu_seconds(), Instant::now());
        let audio = st.audio.as_ref();
        let drift = audio.map_or(0.0, |a| a.sources.iter().map(|s| s.drift_ms.abs()).fold(0.0, f64::max));
        let late = audio.map_or(0.0, |a| a.sources.iter().map(|s| s.late_ms).sum());
        let dropouts: u64 = audio.map_or(0, |a| a.sources.iter().map(|s| s.dropouts).sum());
        let ram = mb(private_working_set());
        println!(
            "  {minute:>3} min: RAM {ram:.1} MB, CPU {cpu:.2} %, Bilder +{} / verworfen +{} (Encoder belegt {}, Pool voll {}, zu spät {}), neue Bildschirmbilder +{}, Neustarts {}, Aufwand {}, Drift {drift:.2} ms, zu spät {late:.0} ms, Aussetzer {dropouts}, Handles {}{}",
            st.frames_encoded - enc0,
            st.frames_dropped - drop0,
            st.dropped_encoder_busy - busy0,
            st.dropped_pool_full - pool0,
            st.dropped_late - late0,
            st.screen_updates - upd0,
            st.capture_restarts,
            st.encoder_effort,
            handle_count(),
            st.error.as_ref().map(|e| format!(", Fehler: {e}")).unwrap_or_default()
        );
        let _ = writeln!(csv, "{minute},{ram:.2},{cpu:.3},{},{},{},{drift:.3},{late:.1},{dropouts},{},{},{}", st.frames_encoded, st.frames_dropped, st.encoder_effort, handle_count(), st.screen_updates, st.capture_restarts);
        (enc0, drop0, upd0) = (st.frames_encoded, st.frames_dropped, st.screen_updates);
        (busy0, pool0, late0) = (st.dropped_encoder_busy, st.dropped_pool_full, st.dropped_late);
    }
    let clip = engine.save_clip(&SaveClipRequest { folder, seconds: None }).map_err(|e| e.to_string())?;
    engine.stop_buffer();
    println!("Clip am Ende: {} ({:.1} s, {} Bilder, Ton {:.1} s, Anfang {:?}, gespeichert in {:.0} ms)", clip.path, clip.duration_seconds, clip.frames, clip.audio_seconds, clip.start, clip.save_ms);
    Ok(())
}

/// Verworfene Bilder mit und ohne Mauszeiger im selben Puffer: Der Zeiger wird alle 15 s umgeschaltet,
/// so trifft fremde Last (andere Programme auf der GPU) beide Hälften gleich. Die Maus steht dabei in
/// der Mitte des Hauptbildschirms (danach zurück).
#[cfg(windows)]
fn cursor_load(rounds: u32) -> Result<(), String> {
    use veloframe::{AudioRequest, BufferRequest, ClipMode, Quality, Resolution, SystemAudio};
    let mut engine = Engine::new().map_err(|e| e.to_string())?;
    let screen = main_screen(&engine)?;
    let request = BufferRequest {
        mode: ClipMode::Performance,
        seconds: 30,
        fps: 60,
        quality: Quality::High,
        resolution: Resolution::Native,
        codec: None,
        storage: None,
        screen,
        audio: AudioRequest { system: SystemAudio::Off, ..Default::default() },
        cursor: true,
    };
    let status = engine.start_buffer(&request).map_err(|e| e.to_string())?;
    let plan = status.plan.ok_or("kein Plan")?;
    println!("{:?} {:?}, Zeiger {} — {} Runden à 2 × 15 s", plan.mode, plan.codec, status.cursor_error.unwrap_or_else(|| "möglich".into()), rounds);
    let old = move_mouse(screen.x + screen.width / 2, screen.y + screen.height / 2);
    std::thread::sleep(std::time::Duration::from_secs(5));
    let mut totals = [(0u64, 0u64); 2]; // (kodiert, verworfen) je Zeiger aus/an
    let mut last = engine.buffer_status();
    for round in 1..=rounds {
        // `VELOFRAME_BENCH_OFF_FIRST`: Reihenfolge umdrehen (prüft, ob ein Unterschied nur an der
        // Reihenfolge liegt).
        // `VELOFRAME_BENCH_HOLD=on|off`: Zeiger die ganze Zeit an bzw. aus (z. B. für GPU-Zähler).
        let order = match std::env::var("VELOFRAME_BENCH_HOLD").as_deref() {
            Ok("on") => [true, true],
            Ok("off") => [false, false],
            _ if std::env::var_os("VELOFRAME_BENCH_OFF_FIRST").is_some() => [false, true],
            _ => [true, false],
        };
        for visible in order {
            engine.set_cursor(visible).map_err(|e| e.to_string())?;
            std::thread::sleep(std::time::Duration::from_secs(15));
            let now = engine.buffer_status();
            let (encoded, dropped) = (now.frames_encoded - last.frames_encoded, now.frames_dropped - last.frames_dropped);
            totals[visible as usize].0 += encoded;
            totals[visible as usize].1 += dropped;
            println!("  Runde {round:>2}, Zeiger {}: {encoded} kodiert, {dropped} verworfen, Aufwand {}", if visible { "an " } else { "aus" }, now.encoder_effort);
            last = now;
        }
    }
    move_mouse(old.0, old.1);
    engine.stop_buffer();
    for (visible, (encoded, dropped)) in [(false, totals[0]), (true, totals[1])] {
        println!("Zeiger {}: {encoded} kodiert, {dropped} verworfen = {:.2} %", if visible { "an " } else { "aus" }, dropped as f64 * 100.0 / (encoded + dropped).max(1) as f64);
    }
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
        #[cfg(windows)]
        Some("start") => start_test(
            if args.get(1).map(String::as_str) == Some("long") { veloframe::ClipMode::Long } else { veloframe::ClipMode::Performance },
            if args.get(2).map(String::as_str) == Some("hevc") { veloframe::VideoCodec::Hevc } else { veloframe::VideoCodec::H264 },
        ),
        #[cfg(windows)]
        Some("longrun") => longrun(
            if args.get(1).map(String::as_str) == Some("long") { veloframe::ClipMode::Long } else { veloframe::ClipMode::Performance },
            args.get(2).and_then(|n| n.parse().ok()).unwrap_or(60),
        ),
        #[cfg(windows)]
        Some("cursorload") => cursor_load(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(10)),
        #[cfg(windows)]
        Some("robust") => robust(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(50)),
        #[cfg(windows)]
        Some("devices") => devices_test(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(60)),
        #[cfg(windows)]
        Some("mic") => mic_test(
            match args.get(1).map(String::as_str) {
                None | Some("default") => veloframe::Microphone::Default,
                Some(id) => veloframe::Microphone::Device(id.to_string()),
            },
            if args.get(2).map(String::as_str) == Some("separate") { veloframe::TrackLayout::Separate } else { veloframe::TrackLayout::Both },
        ),
        #[cfg(windows)]
        Some("cursor") => cursor_test(match args.get(1).map(String::as_str) {
            Some("1440p") => veloframe::Resolution::P1440,
            Some("1080p") => veloframe::Resolution::P1080,
            Some("720p") => veloframe::Resolution::P720,
            _ => veloframe::Resolution::Native,
        }),
        Some("sync") => {
            // sync [anzahl] [self] [performance|long] [h264|hevc] [ausgabegerät]
            let rest: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
            let mode = if rest.contains(&"performance") { veloframe::ClipMode::Performance } else { veloframe::ClipMode::Long };
            let codec = if rest.contains(&"hevc") { veloframe::VideoCodec::Hevc } else { veloframe::VideoCodec::H264 };
            let output = rest.iter().find(|a| !["self", "performance", "long", "h264", "hevc"].contains(a)).map(|s| s.to_string());
            sync(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(8), rest.contains(&"self"), output, mode, codec)
        }
        _ => Err("Aufruf: veloframe-bench encode <bild.png>...  |  capture [anzahl]  |  memory  |  clip [performance|long] [sekunden] [laufzeit] [auto|h264|hevc] [host|all|mic|off|<pid>]  |  audio  |  sync [anzahl]".into()),
    };
    if let Err(error) = result {
        eprintln!("Fehler: {error}");
        std::process::exit(1);
    }
}
