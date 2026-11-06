//! Clip-Puffer: ein eigener Thread nimmt laufend auf, kodiert in Hardware und hält die letzten
//! Sekunden im Replay-Puffer. Speichern schreibt daraus eine MP4-Datei, ohne neu zu kodieren.
//!
//! Pro Bild (bei 60 fps alle 16,7 ms):
//! 1. neuestes Bild per Desktop Duplication holen (nur wenn sich etwas geändert hat, GPU → GPU)
//! 2. fertige Pakete des Encoders in den Puffer legen
//! 3. freies Bild aus dem Encoder-Pool nehmen, auf der GPU nach NV12 umrechnen und in die
//!    Warteschlange stellen (bis zu 4 Bilder); der Encoder holt sie ab, sobald er bereit ist
//!
//! Die CPU fasst dabei kein einziges Pixel an. Screenshots desselben Bildschirms während der Aufnahme
//! kommen aus diesem Bild (`RecorderTap`), ohne zweite Aufnahme.
//!
//! Ton läuft in einem eigenen Thread (`audio.rs`) auf derselben Uhr: Beide zählen ab demselben
//! QPC-Ursprung. Beim Speichern kommen die Tonpakete dazu, die genau die Zeit des Videos abdecken.

use crate::audio::{AudioRecorder, Clock, qpc_now, qpc_to_ticks};
use crate::duplication::FrameTap;
use crate::encode::{HardwareEncoder, hardware_available};
use crate::video::{Converter, Gpu, ScreenGrabber, find_output};
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use veloframe_core::audio::{AudioSetRequest, AudioStatus, aac_lc_config, clip_range};
use veloframe_core::clip::{self, BufferRequest, BufferStatus, ClipResult, EncoderPlan, SaveClipRequest, Storage, VideoCodec};
use veloframe_core::mp4::{AudioTrack, Packet, TICKS_PER_SECOND, VideoTrack, write_mp4};
use veloframe_core::replay::{FileStore, RamStore, ReplayBuffer, Store};
use veloframe_core::screenshot::create_unique_file;
use veloframe_core::frame::{BgraView, Rotation};
use veloframe_core::{Desktop, Error, Rect, Result, RgbImage};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Texture2D,
};

/// Bitte um einen Ausschnitt des aktuellen Bildes (Desktop-Koordinaten).
struct FrameRequest {
    region: Rect,
    image: RgbImage,
    reply: mpsc::Sender<Result<RgbImage>>,
}

/// Screenshots aus dem laufenden Clip-Puffer.
pub struct RecorderTap {
    screen: Rect,
    requests: mpsc::Sender<FrameRequest>,
}

impl FrameTap for RecorderTap {
    fn screen(&self) -> Rect {
        self.screen
    }

    fn grab(&self, region: Rect, out: &mut RgbImage) -> Result<()> {
        let (reply, answer) = mpsc::channel();
        let image = std::mem::take(out);
        self.requests
            .send(FrameRequest { region, image, reply })
            .map_err(|_| Error::Capture("Der Clip-Puffer läuft nicht mehr.".into()))?;
        *out = answer.recv_timeout(Duration::from_secs(2)).map_err(|_| Error::Capture("Der Clip-Puffer hat kein Bild geliefert.".into()))??;
        Ok(())
    }
}

struct Shared {
    buffer: Option<ReplayBuffer>,
    status: BufferStatus,
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
    plan: EncoderPlan,
    screen: Rect,
    frames: mpsc::Sender<FrameRequest>,
    audio: Option<AudioRecorder>,
    /// Warum der Ton nicht läuft, obwohl er geplant war (das Video läuft trotzdem).
    audio_error: Option<String>,
}

/// Alles, was der Aufnahme-Thread besitzt (D3D und Media Foundation bleiben auf diesem Thread).
struct Pipeline {
    gpu: Gpu,
    grabber: ScreenGrabber,
    converter: Converter,
    encoder: HardwareEncoder,
}

fn setup(request: &BufferRequest) -> Result<(Pipeline, EncoderPlan, ReplayBuffer, Rect)> {
    let (adapter, output, rect) = find_output(request.screen)?;
    // Vorab prüfen, ob HEVC geht: entscheidet im Performance-Modus über den Codec.
    let luid = crate::encode::luid_u64(unsafe { adapter.GetDesc() }.map_err(|e| Error::Capture(format!("Grafikkarte: {e}")))?.AdapterLuid);
    let wants_hevc = request.codec == Some(VideoCodec::Hevc) || (request.codec.is_none() && request.mode == clip::ClipMode::Performance);
    let hevc = wants_hevc && hardware_available(VideoCodec::Hevc, luid);
    let mut request = request.clone();
    request.screen = rect; // aufgenommen wird immer der ganze Bildschirm
    let plan = clip::plan(&request, hevc)?;

    let gpu = Gpu::new(&adapter, plan.low_gpu_priority)?;
    let grabber = ScreenGrabber::new(output, rect, &gpu)?;
    let encoder = HardwareEncoder::create(&gpu.device, gpu.luid, &plan)?;
    let converter = Converter::new(&gpu, (grabber.width, grabber.height), (plan.width, plan.height), plan.fps)?;
    let store: Box<dyn Store> = match plan.storage {
        Storage::File => Box::new(
            FileStore::create(&std::env::temp_dir(), plan.buffer_capacity())
                .map_err(|e| Error::Capture(format!("Puffer-Datei konnte nicht angelegt werden: {e}")))?,
        ),
        Storage::Ram => Box::new(RamStore::new(plan.buffer_capacity())),
    };
    let buffer = ReplayBuffer::new(plan.codec, plan.seconds, store);
    Ok((Pipeline { gpu, grabber, converter, encoder }, plan, buffer, rect))
}

/// Kopiert `region` (relativ zum Bildschirm) aus dem aktuellen GPU-Bild und liest es als RGB.
/// Nur dieser Ausschnitt wandert zur CPU; die Kopie dafür wird danach wieder freigegeben.
fn read_region(gpu: &Gpu, latest: &ID3D11Texture2D, region: Rect, out: &mut RgbImage) -> Result<()> {
    let failed = |e: windows::core::Error| Error::Capture(format!("Bild aus dem Clip-Puffer: {e}"));
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        latest.GetDesc(&mut desc);
        desc.Width = region.width as u32;
        desc.Height = region.height as u32;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        gpu.device.CreateTexture2D(&desc, None, Some(&mut staging)).map_err(failed)?;
        let staging = staging.unwrap();
        let area = D3D11_BOX {
            left: region.x as u32,
            top: region.y as u32,
            front: 0,
            right: region.right() as u32,
            bottom: region.bottom() as u32,
            back: 1,
        };
        gpu.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, latest, 0, Some(&area));
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(failed)?;
        let (width, height) = (region.width as usize, region.height as usize);
        let pitch = mapped.RowPitch as usize;
        // SAFETY: Map liefert mindestens pitch × (Höhe − 1) + Breite × 4 lesbare Bytes bis Unmap.
        let pixels = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (height - 1) + width * 4);
        BgraView { pixels, pitch, width, height, rotation: Rotation::Identity }.crop_to_rgb(Rect::new(0, 0, region.width, region.height), out);
        gpu.context.Unmap(&staging, 0);
    }
    Ok(())
}

/// Meldet den Takt-Thread beim Multimedia-Klassendienst (MMCSS) als Aufnahme an: Windows weckt ihn
/// pünktlich, auch wenn die CPU ausgelastet ist. Er braucht selbst kaum Rechenzeit (unter 1 %).
struct MmcssCapture(Option<windows::Win32::Foundation::HANDLE>);

impl MmcssCapture {
    fn join() -> Self {
        let mut index = 0u32;
        let handle = unsafe { windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(windows::core::w!("Capture"), &mut index) }.ok();
        Self(handle)
    }
}

impl Drop for MmcssCapture {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            let _ = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

/// Erhöht die Timer-Auflösung von Windows auf 1 ms, solange aufgenommen wird (genauer Bildtakt).
struct TimerResolution;

impl TimerResolution {
    fn begin() -> Self {
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        Self
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe { windows::Win32::Media::timeEndPeriod(1) };
    }
}

fn pts(frame: u64, fps: u32) -> i64 {
    (frame as i128 * TICKS_PER_SECOND as i128 / fps as i128) as i64
}

/// Legt fertige Pakete des Encoders in den Puffer.
fn drain(encoder: &mut HardwareEncoder, shared: &Mutex<Shared>) -> Result<()> {
    let mut guard = shared.lock().unwrap();
    let shared = &mut *guard;
    let Some(buffer) = shared.buffer.as_mut() else { return Ok(()) };
    encoder.poll(|data, info, header| {
        if let Some(sets) = header {
            buffer.set_parameter_sets(sets.clone());
        }
        if let Err(e) = buffer.push(data, info) {
            shared.status.error = Some(format!("Puffer: {e}"));
        }
    })
}

/// So viele umgerechnete Bilder dürfen auf den Encoder warten. Fängt kurze Verzögerungen (GPU belegt,
/// Encoder-Thread kurz ausgebremst) ab, ohne dass ein Bild verloren geht.
const MAX_PENDING: usize = 4;

/// Gibt wartende Bilder an den Encoder, solange er welche annimmt.
fn feed(encoder: &mut HardwareEncoder, pending: &mut VecDeque<(windows::Win32::Media::MediaFoundation::IMFSample, i64, i64)>, encoded: &mut u64) -> Result<()> {
    while encoder.wants_input() {
        let Some((sample, time, duration)) = pending.pop_front() else { break };
        encoder.submit(&sample, time, duration)?;
        *encoded += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run(mut p: Pipeline, plan: EncoderPlan, screen: Rect, frames: mpsc::Receiver<FrameRequest>, stop: Arc<AtomicBool>, shared: Arc<Mutex<Shared>>, origin: i64, lag: Arc<AtomicI64>) {
    let _timer = TimerResolution::begin();
    let _mmcss = MmcssCapture::join();
    let period = Duration::from_secs_f64(1.0 / plan.fps as f64);
    let start = Instant::now();
    // Bildzeiten auf der gemeinsamen Uhr mit dem Ton (Instant misst unter Windows ebenfalls mit QPC).
    let offset = qpc_now() - origin;
    let mut frame: u64 = 0;
    let mut encoded = 0u64;
    let (mut busy, mut pool_full, mut late) = (0u64, 0u64, 0u64);
    let mut last_status = Instant::now();
    // Regler für den Encoder-Aufwand: alle 2 s die Verlustquote prüfen.
    let mut effort = plan.quality_vs_speed;
    let mut effort_changes = 0u32;
    let (mut window_start, mut window_encoded, mut window_busy) = (Instant::now(), 0u64, 0u64);
    let mut calm_windows = 0u32;
    let mut error: Option<String> = None;
    let mut pending = VecDeque::with_capacity(MAX_PENDING + 1);
    let mut lag_samples = 0u32;

    while !stop.load(Ordering::Relaxed) {
        // Bis zum nächsten Bildzeitpunkt in kleinen Schritten den Encoder bedienen: Pakete abholen,
        // wartende Bilder nachreichen, sobald er bereit ist.
        let deadline = start + period * frame as u32;
        loop {
            if let Err(e) = drain(&mut p.encoder, &shared).and_then(|()| feed(&mut p.encoder, &mut pending, &mut encoded)) {
                error = Some(e.to_string());
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(2)));
        }
        let now = Instant::now();
        if now - deadline > period * 2 {
            let behind = ((now - start).as_secs_f64() * plan.fps as f64) as u64;
            if encoded > 0 {
                late += behind.saturating_sub(frame);
            }
            frame = behind;
        }

        let step = (|| -> Result<()> {
            p.grabber.grab()?;
            if p.grabber.fresh {
                // Wie alt das Bild beim Abholen ist (sein Zeitstempel minus Anzeigezeit). Gleitend
                // gemittelt über etwa 2 s bei 60 fps: Der Ton liegt um so viel später, damit beide
                // zusammenpassen, und verschiebt sich dabei nie sprunghaft (höchstens ~2 ms/s, was
                // die Drift-Korrektur unhörbar nachführt).
                let age = origin + offset + pts(frame, plan.fps) - qpc_to_ticks(p.grabber.last_present);
                if (0..4 * TICKS_PER_SECOND / plan.fps as i64).contains(&age) {
                    // Die ersten 32 Bilder rasten schnell ein, danach nur noch langsam nachführen.
                    let current = lag.load(Ordering::Relaxed);
                    let weight = if lag_samples < 32 { 8 } else { 128 };
                    lag.store(current + (age - current) / weight, Ordering::Relaxed);
                    lag_samples += 1;
                }
            }
            // Screenshot-Wünsche bedienen, bevor das Bild weiterläuft.
            while let Ok(request) = frames.try_recv() {
                let FrameRequest { region, mut image, reply } = request;
                let answer = match (&p.grabber.latest, region.intersect(&screen)) {
                    (Some(latest), Some(part)) => read_region(&p.gpu, latest, part.relative_to(&screen), &mut image).map(|()| image),
                    (None, _) => Err(Error::Capture("Der Clip-Puffer hat noch kein Bild.".into())),
                    (_, None) => Err(Error::NoScreen("Der Bereich liegt nicht auf dem aufgenommenen Bildschirm.".into())),
                };
                let _ = reply.send(answer);
            }
            let Some(latest) = p.grabber.latest.clone() else { return Ok(()) };
            // Pünktlich umrechnen und einreihen; abgeholt wird, sobald der Encoder bereit ist.
            match p.encoder.free_frame()? {
                Some((sample, texture, slice)) => {
                    p.converter.convert(&latest, &texture, slice)?;
                    let time = pts(frame, plan.fps);
                    pending.push_back((sample, offset + time, pts(frame + 1, plan.fps) - time));
                    if pending.len() > MAX_PENDING {
                        // Der Encoder kommt dauerhaft nicht hinterher: ältestes wartendes Bild verwerfen.
                        pending.pop_front();
                        // Solange er noch hochfährt, ist nichts verloren: der Puffer beginnt nur später.
                        if encoded > 0 {
                            busy += 1;
                        }
                    }
                }
                None => {
                    if encoded > 0 {
                        pool_full += 1;
                    }
                }
            }
            feed(&mut p.encoder, &mut pending, &mut encoded)?;
            drain(&mut p.encoder, &shared)
        })();
        if let Err(e) = step {
            error = Some(e.to_string());
        }
        frame += 1;

        if encoded > 0 && window_start.elapsed() >= Duration::from_secs(2) {
            let (frames, lost) = (encoded - window_encoded + busy - window_busy, busy - window_busy);
            let rate = if frames > 0 { lost as f64 / frames as f64 } else { 0.0 };
            // Mehr als 1 % verloren: Aufwand senken. Viermal in Folge ohne Verlust: vorsichtig erhöhen.
            let target = if rate > 0.01 && effort > 0 {
                calm_windows = 0;
                Some(effort.saturating_sub(15))
            } else if lost == 0 && effort < plan.quality_vs_speed {
                calm_windows += 1;
                (calm_windows >= 4).then(|| (effort + 5).min(plan.quality_vs_speed))
            } else {
                None
            };
            if let Some(target) = target {
                if p.encoder.set_effort(target) {
                    effort = target;
                    effort_changes += 1;
                    calm_windows = 0;
                }
            }
            (window_start, window_encoded, window_busy) = (Instant::now(), encoded, busy);
        }

        if last_status.elapsed() >= Duration::from_millis(250) {
            last_status = Instant::now();
            let mut guard = shared.lock().unwrap();
            let shared = &mut *guard;
            if let Some(buffer) = shared.buffer.as_ref() {
                let status = &mut shared.status;
                status.buffered_seconds = buffer.buffered_seconds();
                status.buffered_bytes = buffer.buffered_bytes();
                status.ram_bytes = buffer.ram_bytes();
                status.actual_bitrate = if status.buffered_seconds > 0.0 { (status.buffered_bytes as f64 * 8.0 / status.buffered_seconds) as u64 } else { 0 };
            }
            shared.status.frames_encoded = encoded;
            shared.status.encoder_effort = effort;
            shared.status.effort_changes = effort_changes;
            shared.status.frames_dropped = busy + pool_full + late;
            shared.status.dropped_encoder_busy = busy;
            shared.status.dropped_pool_full = pool_full;
            shared.status.dropped_late = late;
            if error.is_some() {
                shared.status.error = error.take();
            }
        }
    }
    // Abbau in fester Reihenfolge: wartende Bilder zurück in den Pool, dann Encoder (fährt seine
    // Threads herunter), dann Umrechnung und Aufnahme, zuletzt das D3D-Gerät.
    drop(pending);
    let Pipeline { gpu, grabber, converter, encoder } = p;
    drop(encoder);
    drop(converter);
    drop(grabber);
    drop(gpu);
    drop(_mmcss);
    drop(_timer);
}

impl Recorder {
    pub fn start(request: &BufferRequest) -> Result<Self> {
        let origin = qpc_now();
        // Bis gemessen ist, wie alt ein Bild beim Abholen ist: eine halbe Bilddauer.
        let lag = Arc::new(AtomicI64::new(TICKS_PER_SECOND / (2 * request.fps.max(1) as i64)));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(Shared { buffer: None, status: BufferStatus::default() }));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(EncoderPlan, Rect)>>();
        let (frames_tx, frames_rx) = mpsc::channel::<FrameRequest>();
        let request = request.clone();
        let thread = {
            let (stop, shared, lag) = (stop.clone(), shared.clone(), lag.clone());
            std::thread::Builder::new()
                .name("veloframe-video".into())
                .spawn(move || match setup(&request) {
                    Ok((pipeline, plan, buffer, screen)) => {
                        {
                            let mut s = shared.lock().unwrap();
                            s.buffer = Some(buffer);
                            s.status = BufferStatus {
                                running: true,
                                plan: Some(plan.clone()),
                                encoder: Some(pipeline.encoder.name.clone()),
                                applied: pipeline.encoder.applied.clone(),
                                ..Default::default()
                            };
                        }
                        let _ = ready_tx.send(Ok((plan.clone(), screen)));
                        run(pipeline, plan, screen, frames_rx, stop, shared, origin, lag);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                })
                .map_err(|e| Error::Capture(format!("Aufnahme-Thread: {e}")))?
        };
        let (plan, screen) = ready_rx.recv().map_err(|_| Error::Capture("Aufnahme-Thread ist beim Start abgestürzt.".into()))??;
        // Ton: Fehlt er (z. B. kein AAC-Encoder unter Windows N), läuft das Video trotzdem.
        let (audio, audio_error) = match &plan.audio {
            Some(audio_plan) => {
                // So weit zurück wie das Video: Fenster + ein GOP (Clips beginnen am Keyframe davor).
                let seconds = plan.seconds + plan.gop_frames.div_ceil(plan.fps) + 2;
                match AudioRecorder::start(audio_plan, Clock { origin, lag }, seconds) {
                    Ok(audio) => (Some(audio), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            }
            None => (None, None),
        };
        Ok(Self { stop, shared, thread: Some(thread), plan, screen, frames: frames_tx, audio, audio_error })
    }

    /// Lautstärke oder Stummschaltung des Tons ändern, während der Puffer läuft.
    pub fn set_audio(&self, request: &AudioSetRequest) -> Result<AudioStatus> {
        let audio = self.audio.as_ref().ok_or_else(|| Error::InvalidRequest("Der Clip-Puffer nimmt keinen Ton auf.".into()))?;
        audio.set(request)?;
        Ok(audio.status())
    }

    /// Zugang für Screenshots aus dem laufenden Bild (für `DesktopDuplication::set_tap`).
    pub fn tap(&self) -> Box<dyn FrameTap> {
        Box::new(RecorderTap { screen: self.screen, requests: self.frames.clone() })
    }

    pub fn status(&self) -> BufferStatus {
        let mut status = self.shared.lock().unwrap().status.clone();
        status.running = self.thread.as_ref().is_some_and(|t| !t.is_finished());
        status.audio = match (&self.audio, &self.audio_error) {
            (Some(audio), _) => Some(audio.status()),
            (None, Some(error)) => Some(AudioStatus { error: Some(error.clone()), ..Default::default() }),
            (None, None) => None,
        };
        status
    }

    /// Schreibt den Puffer (oder die letzten `seconds`) als MP4. Die Aufnahme läuft dabei weiter:
    /// Gesperrt wird nur für den Schnappschuss des Inhaltsverzeichnisses.
    pub fn save(&self, request: &SaveClipRequest, desktop: &impl Desktop) -> Result<ClipResult> {
        let start = Instant::now();
        if !request.folder.is_absolute() {
            return Err(Error::InvalidRequest("Der Speicherort muss ein vollständiger Pfad sein.".into()));
        }
        let snapshot = {
            let guard = self.shared.lock().unwrap();
            let buffer = guard.buffer.as_ref().ok_or_else(|| Error::InvalidRequest("Der Clip-Puffer läuft nicht.".into()))?;
            buffer.snapshot(request.seconds).map_err(|e| Error::Save(e.to_string()))?
        };
        let infos = &snapshot.packets.infos;
        let video_start = infos.iter().map(|i| i.pts).min().unwrap_or(0);
        let video_end = infos.iter().map(|i| i.pts + i.duration).max().unwrap_or(0);
        // Ton: Der Mischer läuft bewusst etwas hinter der Echtzeit (60 ms + 2 Frames im Encoder).
        // Kurz warten, bis er das Ende des Videos erreicht hat, damit der Clip bis zum Schluss Ton
        // hat (höchstens 250 ms). Dann eine Kopie der (kleinen) Tonpuffer.
        let audio = match &self.audio {
            Some(audio) => {
                let deadline = Instant::now() + Duration::from_millis(250);
                while audio.end_time().is_some_and(|end| end < video_end) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                audio.snapshot().map_err(|e| Error::Save(e.to_string()))?
            }
            None => Vec::new(),
        };
        let ranges: Vec<std::ops::Range<usize>> = audio.iter().map(|a| clip_range(&a.infos, video_start, video_end)).collect();
        let config = self.audio.as_ref().map(|a| aac_lc_config(a.plan.sample_rate, a.plan.channels)).unwrap_or_default();
        let tracks: Vec<AudioTrack> = match &self.audio {
            Some(a) => a
                .plan
                .tracks
                .iter()
                .zip(audio.iter().zip(&ranges))
                .map(|(kind, (packets, range))| AudioTrack {
                    name: kind.label(),
                    sample_rate: a.plan.sample_rate,
                    channels: a.plan.channels,
                    bitrate: a.plan.bitrate,
                    config: &config,
                    priming: crate::aac::PRIMING,
                    infos: &packets.infos[range.clone()],
                })
                .collect(),
            None => Vec::new(),
        };
        let (path, file) = create_unique_file(&request.folder, "Clip", &desktop.timestamp(), "mp4")?;
        let written = (|| -> std::io::Result<veloframe_core::mp4::Mp4Info> {
            let mut out = BufWriter::with_capacity(1 << 20, file);
            let video = VideoTrack { codec: snapshot.codec, width: self.plan.width, height: self.plan.height, sets: &snapshot.sets, infos };
            let info = write_mp4(&mut out, &video, &tracks, |p, buf| match p {
                Packet::Video(i) => snapshot.packets.packet(i, buf),
                Packet::Audio { track, index } => audio[track].packet(ranges[track].start + index, buf),
            })?;
            out.flush()?;
            Ok(info)
        })();
        let still_valid = snapshot.packets.still_valid(self.shared.lock().unwrap().buffer.as_ref().map_or(0, |b| b.written()));
        let info = match (written, still_valid) {
            (Ok(info), true) => info,
            (Ok(_), false) => {
                let _ = std::fs::remove_file(&path);
                return Err(Error::Save("Der Puffer wurde während des Speicherns überschrieben. Bitte erneut versuchen.".into()));
            }
            (Err(e), _) => {
                let _ = std::fs::remove_file(&path);
                return Err(Error::Save(format!("Clip konnte nicht gespeichert werden: {e}")));
            }
        };
        Ok(ClipResult {
            path: path.to_string_lossy().into_owned(),
            bytes: info.bytes,
            duration_seconds: info.duration_seconds,
            frames: info.frames,
            codec: snapshot.codec,
            width: self.plan.width,
            height: self.plan.height,
            save_ms: start.elapsed().as_secs_f64() * 1000.0,
            audio_tracks: self.audio.as_ref().map(|a| a.plan.tracks[..info.audio_tracks].to_vec()).unwrap_or_default(),
            audio_seconds: info.audio_seconds,
        })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.audio = None;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.shared.lock().unwrap().buffer = None;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shutdown();
    }
}
