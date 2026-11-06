//! Ton für den Clip-Puffer: **ein** Thread für alle Quellen.
//!
//! ```text
//! Spiel / System ─► WASAPI (Prozess- oder System-Loopback) ─┐
//! Mikrofon       ─► WASAPI ─────────────────────────────────┤
//!                                                           ▼
//!   Ton-Thread (ereignisgesteuert, MMCSS „Pro Audio“)
//!   ├─ jedes Paket an seine QPC-Zeit legen (Stille füllen, Drift korrigieren)   core/audio.rs
//!   ├─ mischen (Verstärkung je Quelle), float → 16 Bit direkt in den Encoder
//!   └─ AAC je Spur (Media Foundation)                                           aac.rs
//!                                                           ▼
//!   Tonpuffer je Spur (RAM) ─► save-clip: MP4 mit Video + 1–3 Tonspuren
//! ```
//!
//! - Der Thread wartet mit `WaitForMultipleObjects` auf alle Quellen: kein Abfragen im Leerlauf.
//!   Spätestens alle 10 ms wacht er auf, damit Stille weiterläuft, wenn keine Quelle etwas liefert.
//! - Gemischt wird 60 ms hinter der Echtzeit: So lange darf ein Paket unterwegs sein. Was später
//!   kommt, fehlt (gezählt in `lateMs`).
//! - Die Übergabe an den Puffer wartet nie: Ist er gerade gesperrt (Speichern), bleiben die Pakete
//!   bis zur nächsten Runde liegen.
//! - Gerätewechsel (Headset eingesteckt, Standardgerät geändert, Gerät abgezogen): Die Quelle wird
//!   neu geöffnet, die Lücke ist Stille, das Video läuft weiter.

use crate::aac::AacEncoder;
use crate::wasapi::{Capture, CapturePath, DeviceWatcher, Event, INPUT_CHANGED, OUTPUT_CHANGED, enumerator, is_bluetooth};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use veloframe_core::audio::{
    AudioPlan, AudioSetRequest, AudioStatus, AudioWarning, CHANNELS, FRAME_SAMPLES, Microphone, SAMPLE_RATE, SourceBuffer, SourceKind,
    SourceStatus, SystemAudio, TrackKind, position_of, ticks_of, to_i16,
};
use veloframe_core::mp4::PacketInfo;
use veloframe_core::replay::{AudioBuffer, Packets};
use veloframe_core::{Error, Result};
use windows::Win32::Media::Audio::{IMMDevice, IMMDeviceEnumerator, eCapture, eConsole};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

/// So weit hinter der Echtzeit wird gemischt (Samples): 60 ms.
const MARGIN: i64 = SAMPLE_RATE as i64 * 60 / 1000;
/// Liegt der Mischer weiter zurück (z. B. nach dem Ruhezustand), springt er statt Stunden Stille
/// nachzuholen: 2 s.
const MAX_CATCH_UP: i64 = 2 * SAMPLE_RATE as i64;
/// Puffer je Quelle: 2 s.
const SOURCE_FRAMES: usize = 2 * SAMPLE_RATE as usize;

/// QPC-Zählerstand in 100 ns umrechnen (dieselbe Einheit wie die Zeitstempel von WASAPI).
pub fn qpc_to_ticks(count: i64) -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceFrequency;
    static FREQUENCY: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut f = 0i64;
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    (count as i128 * 10_000_000 / frequency as i128) as i64
}

/// QPC-Zeit jetzt, in 100 ns.
pub fn qpc_now() -> i64 {
    let mut count = 0i64;
    let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
    qpc_to_ticks(count)
}

/// Gemeinsame Uhr von Bild und Ton. Clip-Zeit (100 ns) = QPC − `origin` + `lag`.
#[derive(Clone, Debug)]
pub struct Clock {
    pub origin: i64,
    /// Ausgleich für das Bild (100 ns): Ein Bild wird zum Bildtakt abgeholt und zeigt, was Windows
    /// etwas früher angezeigt hat. Der Video-Thread misst dieses Alter laufend (`LastPresentTime`
    /// von Desktop Duplication) und mittelt es; vorher gilt eine halbe Bilddauer.
    pub lag: Arc<std::sync::atomic::AtomicI64>,
}

impl Clock {
    fn position(&self, qpc: i64) -> i64 {
        position_of(qpc - self.origin + self.lag.load(Ordering::Relaxed))
    }

    fn now(&self) -> i64 {
        self.position(qpc_now())
    }
}

/// Lautstärke und Stummschaltung je Quelle, ohne Sperre lesbar.
struct Controls {
    gain: [AtomicU32; 2],
    muted: [AtomicBool; 2],
}

impl Controls {
    fn new() -> Self {
        Self { gain: [AtomicU32::new(1f32.to_bits()), AtomicU32::new(1f32.to_bits())], muted: [AtomicBool::new(false), AtomicBool::new(false)] }
    }

    fn gain(&self, kind: SourceKind) -> f32 {
        let i = kind as usize;
        if self.muted[i].load(Ordering::Relaxed) { 0.0 } else { f32::from_bits(self.gain[i].load(Ordering::Relaxed)) }
    }
}

/// Meldet den Thread beim Multimedia-Klassendienst als „Pro Audio“ an: Er braucht kaum Rechenzeit,
/// darf aber nie zu spät dran sein.
struct Mmcss(Option<windows::Win32::Foundation::HANDLE>);

impl Mmcss {
    fn join() -> Self {
        let mut index = 0u32;
        Self(unsafe { windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut index) }.ok())
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            let _ = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

struct Source {
    kind: SourceKind,
    capture: Option<Capture>,
    buffer: SourceBuffer,
    /// Nächster Versuch, die Quelle (wieder) zu öffnen.
    retry_at: Option<Instant>,
    discontinuities: u64,
    device_changes: u32,
    warnings: Vec<AudioWarning>,
    peak: f32,
}

fn warning(code: &str, message: impl Into<String>) -> AudioWarning {
    AudioWarning { code: code.into(), message: message.into() }
}

impl Source {
    /// Öffnet die Quelle laut Plan. Was nicht klappt, steht danach in `warnings`.
    fn open(&mut self, plan: &AudioPlan, enumerator: &IMMDeviceEnumerator) {
        self.warnings.clear();
        self.capture = None;
        self.retry_at = None;
        let host = plan.host_process.unwrap_or_else(std::process::id);
        let opened = match self.kind {
            SourceKind::System => {
                let process = match plan.system {
                    SystemAudio::AllExceptHost => Some((host, false)),
                    SystemAudio::Process(pid) => Some((pid, true)),
                    _ => None,
                };
                match process.map(|(pid, include)| Capture::process_loopback(pid, include)) {
                    Some(Ok(capture)) => Ok(capture),
                    Some(Err(e)) => {
                        self.warnings.push(warning(
                            "process-loopback-unavailable",
                            format!("Ton einzelner Programme geht hier nicht ({e}). Aufgenommen wird der ganze Systemton (ab Windows 10 2004 möglich)."),
                        ));
                        Capture::system_loopback(enumerator)
                    }
                    None => Capture::system_loopback(enumerator),
                }
                .map_err(Some)
            }
            SourceKind::Microphone => self.open_microphone(&plan.microphone, enumerator),
        };
        match opened {
            Ok(capture) => self.capture = Some(capture),
            Err(Some(e)) => {
                self.warnings.push(warning("no-device", e.to_string()));
                self.retry_at = Some(Instant::now() + Duration::from_secs(2));
            }
            Err(None) => {}
        }
    }

    /// `Err(None)`: absichtlich nicht geöffnet (Bluetooth), kein erneuter Versuch.
    fn open_microphone(&mut self, microphone: &Microphone, enumerator: &IMMDeviceEnumerator) -> std::result::Result<Capture, Option<Error>> {
        let device: IMMDevice = unsafe {
            match microphone {
                Microphone::Device(id) => enumerator.GetDevice(&windows::core::HSTRING::from(id.as_str())),
                _ => enumerator.GetDefaultAudioEndpoint(eCapture, eConsole),
            }
        }
        .map_err(|_| Some(Error::Capture("Kein Mikrofon gefunden.".into())))?;
        if is_bluetooth(&device) {
            if *microphone == Microphone::Default {
                // Das Mikrofon eines Bluetooth-Headsets zu öffnen schaltet das Headset in den
                // Freisprech-Modus: Der Spielton klingt dann plötzlich schlecht. Nie ungefragt.
                self.warnings.push(warning(
                    "bluetooth-hands-free",
                    "Das Standard-Mikrofon gehört zu einem Bluetooth-Headset. Es zu öffnen würde das Headset in den Freisprech-Modus schalten (Mono, 16 kHz) und den Spielton verschlechtern. VeloFrame nimmt es deshalb nicht auf; wähle es über seine ID aus audio-devices, wenn du es trotzdem willst.",
                ));
                return Err(None);
            }
            self.warnings.push(warning(
                "bluetooth-hands-free",
                "Das Mikrofon gehört zu einem Bluetooth-Headset: Solange es offen ist, läuft das Headset im Freisprech-Modus (Mono, 16 kHz).",
            ));
        }
        Capture::microphone(&device).map_err(Some)
    }

    fn status(&mut self, controls: &Controls) -> SourceStatus {
        let stats = self.buffer.stats;
        let ms = |frames: u64| frames as f64 * 1000.0 / SAMPLE_RATE as f64;
        let i = self.kind as usize;
        SourceStatus {
            source: self.kind,
            capture: self.capture.as_ref().map_or("none", |c| c.path.label()).into(),
            device: self.capture.as_ref().and_then(|c| c.device.clone()),
            active: self.capture.is_some(),
            gain: f32::from_bits(controls.gain[i].load(Ordering::Relaxed)),
            muted: controls.muted[i].load(Ordering::Relaxed),
            level: std::mem::take(&mut self.peak),
            dropouts: self.discontinuities,
            late_ms: ms(stats.late_frames),
            silence_ms: ms(stats.silence_frames),
            drift_corrections: stats.drift_corrections,
            drift_ms: stats.deviation as f64 * 1000.0 / SAMPLE_RATE as f64,
            device_changes: self.device_changes,
            latency_ms: self.capture.as_ref().map_or(0.0, |c| c.latency as f64 / 1e4),
            warnings: self.warnings.clone(),
        }
    }
}

struct Shared {
    buffers: Vec<AudioBuffer>,
    status: AudioStatus,
}

/// Was der Ton-Thread pro Spur hält.
struct Track {
    kind: TrackKind,
    encoder: AacEncoder,
    /// Positionen der Blöcke, die im Encoder stecken (er gibt sie in derselben Reihenfolge aus).
    blocks: VecDeque<i64>,
    pending: Vec<(Vec<u8>, PacketInfo)>,
}

pub struct AudioRecorder {
    stop: Arc<AtomicBool>,
    wake: Arc<Event>,
    shared: Arc<Mutex<Shared>>,
    controls: Arc<Controls>,
    thread: Option<JoinHandle<()>>,
    pub plan: AudioPlan,
}

impl AudioRecorder {
    /// Startet die Ton-Aufnahme. `seconds`: wie weit der Ton zurückreichen muss.
    pub fn start(plan: &AudioPlan, clock: Clock, seconds: u32) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Event::new()?);
        let controls = Arc::new(Controls::new());
        let buffers = plan.tracks.iter().map(|_| AudioBuffer::new(seconds, plan.bitrate)).collect();
        let shared = Arc::new(Mutex::new(Shared {
            buffers,
            status: AudioStatus { running: true, tracks: plan.tracks.clone(), bitrate: plan.bitrate, ..Default::default() },
        }));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread = {
            let (plan, stop, wake, shared, controls) = (plan.clone(), stop.clone(), wake.clone(), shared.clone(), controls.clone());
            std::thread::Builder::new()
                .name("veloframe-audio".into())
                .spawn(move || {
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    }
                    run(&plan, &clock, &stop, &wake, &shared, &controls, ready_tx);
                    unsafe { CoUninitialize() };
                })
                .map_err(|e| Error::Capture(format!("Ton-Thread: {e}")))?
        };
        ready_rx.recv().map_err(|_| Error::Capture("Ton-Thread ist beim Start abgestürzt.".into()))??;
        Ok(Self { stop, wake, shared, controls, thread: Some(thread), plan: plan.clone() })
    }

    /// Lautstärke oder Stummschaltung ändern; wirkt sofort.
    pub fn set(&self, request: &AudioSetRequest) -> Result<()> {
        request.validate()?;
        for (kind, control) in [(SourceKind::System, request.system), (SourceKind::Microphone, request.microphone)] {
            let Some(control) = control else { continue };
            let i = kind as usize;
            if let Some(gain) = control.gain {
                self.controls.gain[i].store(gain.to_bits(), Ordering::Relaxed);
            }
            if let Some(muted) = control.muted {
                self.controls.muted[i].store(muted, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    pub fn status(&self) -> AudioStatus {
        let mut status = self.shared.lock().unwrap().status.clone();
        status.running = self.thread.as_ref().is_some_and(|t| !t.is_finished());
        // Lautstärke sofort richtig zeigen, nicht erst nach der nächsten Statusrunde.
        for source in &mut status.sources {
            let i = source.source as usize;
            source.gain = f32::from_bits(self.controls.gain[i].load(Ordering::Relaxed));
            source.muted = self.controls.muted[i].load(Ordering::Relaxed);
        }
        status
    }

    /// Bis wohin (100 ns, Clip-Zeit) der Ton fertig im Puffer liegt.
    pub fn end_time(&self) -> Option<i64> {
        self.shared.lock().unwrap().buffers.first().and_then(AudioBuffer::end_time)
    }

    /// Alle Tonpakete je Spur (eine Kopie, wenige Millisekunden unter der Sperre).
    pub fn snapshot(&self) -> std::io::Result<Vec<Packets>> {
        self.shared.lock().unwrap().buffers.iter().map(AudioBuffer::snapshot).collect()
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(plan: &AudioPlan, clock: &Clock, stop: &AtomicBool, wake: &Arc<Event>, shared: &Mutex<Shared>, controls: &Controls, ready: mpsc::Sender<Result<()>>) {
    let setup = (|| -> Result<(IMMDeviceEnumerator, Vec<Track>)> {
        let enumerator = enumerator()?;
        let mut tracks = Vec::new();
        for &kind in &plan.tracks {
            let encoder = AacEncoder::new(plan.sample_rate, plan.channels, plan.bitrate)?;
            tracks.push(Track { kind, encoder, blocks: VecDeque::new(), pending: Vec::new() });
        }
        Ok((enumerator, tracks))
    })();
    let (enumerator, mut tracks) = match setup {
        Ok(ok) => ok,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _mmcss = Mmcss::join();
    let watcher = DeviceWatcher::new(&enumerator, wake.clone()).ok();
    let mut sources: Vec<Source> = plan
        .sources()
        .into_iter()
        .map(|kind| Source {
            kind,
            capture: None,
            buffer: SourceBuffer::new(SOURCE_FRAMES),
            retry_at: None,
            discontinuities: 0,
            device_changes: 0,
            warnings: Vec::new(),
            peak: 0.0,
        })
        .collect();
    let watch = |sources: &[Source]| {
        if let Some(w) = &watcher {
            for s in sources {
                let id = s.capture.as_ref().and_then(|c| c.device_id.clone());
                w.watch(s.kind as usize, id);
            }
        }
    };
    for source in &mut sources {
        source.open(plan, &enumerator);
    }
    watch(&sources);
    {
        let mut s = shared.lock().unwrap();
        s.status.encoder = Some(tracks[0].encoder.name.clone());
        s.status.sources = sources.iter_mut().map(|src| src.status(controls)).collect();
    }
    let _ = ready.send(Ok(()));

    let block = FRAME_SAMPLES as i64;
    let mut mixed_to = clock.now() - MARGIN;
    let mut mix = vec![0.0f32; FRAME_SAMPLES as usize * CHANNELS];
    let mut last_status = Instant::now();
    let mut error: Option<String> = None;

    while !stop.load(Ordering::Relaxed) {
        let mut handles = vec![wake.0];
        handles.extend(sources.iter().filter_map(|s| s.capture.as_ref().map(|c| c.event.0)));
        unsafe {
            let _ = windows::Win32::System::Threading::WaitForMultipleObjects(&handles, false, 10);
        }

        // Gerätewechsel: betroffene Quellen neu öffnen; die Lücke wird Stille.
        let changed = watcher.as_ref().map_or(0, DeviceWatcher::take);
        let mut reopened = false;
        for source in &mut sources {
            let affected = match source.kind {
                SourceKind::System => changed & OUTPUT_CHANGED != 0 && source.capture.as_ref().is_none_or(|c| c.path == CapturePath::SystemLoopback),
                SourceKind::Microphone => changed & INPUT_CHANGED != 0,
            };
            let retry = source.retry_at.is_some_and(|t| Instant::now() >= t);
            if affected || retry {
                if affected {
                    source.device_changes += 1;
                }
                let before = source.capture.as_ref().and_then(|c| c.device_id.clone());
                source.open(plan, &enumerator);
                if affected || source.capture.as_ref().and_then(|c| c.device_id.clone()) != before {
                    source.buffer.reset();
                }
                reopened = true;
            }
        }
        if reopened {
            watch(&sources);
        }

        // Pakete an ihre Zeit legen.
        let read_to = mixed_to;
        for source in &mut sources {
            let Some(capture) = source.capture.as_mut() else { continue };
            let (buffer, discontinuities) = (&mut source.buffer, &mut source.discontinuities);
            let result = capture.read(|packet| {
                if packet.discontinuity {
                    *discontinuities += 1;
                }
                buffer.write(packet.qpc.map(|q| clock.position(q)), packet.frames, read_to);
            });
            source.peak = source.peak.max(source.buffer.take_peak());
            if let Err(e) = result {
                // Gerät weg: gleich neu versuchen (meist ist ein anderes Standardgerät da).
                source.capture = None;
                source.device_changes += 1;
                source.warnings = vec![warning("device-lost", format!("Tongerät verloren ({e}), wird neu geöffnet."))];
                source.retry_at = Some(Instant::now() + Duration::from_millis(500));
                source.buffer.reset();
            }
        }

        // Mischen und kodieren, was sicher vollständig ist.
        let target = clock.now() - MARGIN;
        if target - mixed_to > MAX_CATCH_UP {
            mixed_to = target - block; // Ruhezustand o. Ä.: springen statt Stunden Stille zu kodieren
        }
        while mixed_to + block <= target {
            for track in &mut tracks {
                mix.fill(0.0);
                for source in &sources {
                    if track.kind.includes(source.kind) {
                        source.buffer.mix_into(mixed_to, &mut mix, controls.gain(source.kind));
                    }
                }
                track.blocks.push_back(mixed_to);
                let (blocks, pending) = (&mut track.blocks, &mut track.pending);
                let encoded = track.encoder.encode(FRAME_SAMPLES as usize, |pcm| to_i16(&mix, pcm), |data| {
                    let position = blocks.pop_front().unwrap_or(mixed_to);
                    let pts = ticks_of(position);
                    pending.push((data.to_vec(), PacketInfo { pts, dts: pts, duration: ticks_of(position + block) - pts, key: true }));
                });
                if let Err(e) = encoded {
                    error = Some(e.to_string());
                }
            }
            mixed_to += block;
        }

        // Übergabe an die Puffer, ohne je zu warten.
        let now = Instant::now();
        let report = now - last_status >= Duration::from_millis(250);
        if tracks.iter().any(|t| !t.pending.is_empty()) || report {
            if let Ok(mut guard) = shared.try_lock() {
                let s = &mut *guard;
                for (track, buffer) in tracks.iter_mut().zip(&mut s.buffers) {
                    for (data, info) in track.pending.drain(..) {
                        if let Err(e) = buffer.push(&data, info) {
                            error = Some(format!("Tonpuffer: {e}"));
                        }
                    }
                }
                if report {
                    last_status = now;
                    s.status.buffered_seconds = s.buffers.first().map_or(0.0, AudioBuffer::buffered_seconds);
                    s.status.ram_bytes = s.buffers.iter().map(AudioBuffer::ram_bytes).sum::<u64>() + (sources.len() * SOURCE_FRAMES * CHANNELS * 4) as u64;
                    s.status.sources = sources.iter_mut().map(|src| src.status(controls)).collect();
                    s.status.video_lag_ms = clock.lag.load(Ordering::Relaxed) as f64 / 1e4;
                    if error.is_some() {
                        s.status.error = error.take();
                    }
                }
            }
        }
    }
    // Abbau: Quellen zuerst (stoppt WASAPI), dann Encoder, zuletzt die Benachrichtigungen.
    drop(sources);
    drop(tracks);
    drop(watcher);
}
