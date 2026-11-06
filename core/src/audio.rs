//! Ton für Clips, plattformunabhängig: Anfrage und Plan, die gemeinsame Zeitachse, ein Puffer je
//! Quelle (Stille-Füllung, Drift-Korrektur), der Mischer und der AAC-Kopf für die MP4-Datei.
//!
//! **Zeitachse:** Video und Ton zählen in 100 ns ab demselben Ursprung (unter Windows QPC). Eine
//! Position auf der Ton-Zeitachse ist ein Sample-Index bei 48 kHz. [`ticks_of`] und [`position_of`]
//! rechnen exakt um: Jedes Paket trägt seine eigene Uhrzeit, Rundungsfehler summieren sich nie auf,
//! auch nach Stunden nicht.
//!
//! **Pro Quelle** ([`SourceBuffer`]) landet jedes Paket an der Stelle, die seine Uhrzeit vorgibt:
//! - Lücken (Loopback liefert nichts, solange nichts spielt) werden mit Stille gefüllt, der Ton
//!   rutscht nie gegen das Bild.
//! - Läuft die Uhr der Soundkarte ein paar ppm anders als QPC, wächst die Abweichung langsam. Ab
//!   2 ms wird pro Paket ein einzelnes Sample eingefügt oder weggelassen (unhörbar). Springt sie um
//!   mehr als 4 ms, ist das keine Drift, sondern eine Lücke: dann sofort neu ausrichten.
//! - Was zu spät kommt (der Mischer ist schon weiter), wird verworfen und gezählt.

use crate::clip::ClipMode;
use crate::mp4::{PacketInfo, TICKS_PER_SECOND};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Abtastrate aller Tonspuren. Windows rechnet jede Quelle selbst darauf um.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo.
pub const CHANNELS: usize = 2;
/// Samples pro AAC-Frame (AAC-LC).
pub const FRAME_SAMPLES: u32 = 1024;
/// Bitraten, die der AAC-Encoder von Windows annimmt (bit/s je Spur).
pub const BITRATES: [u32; 4] = [96_000, 128_000, 160_000, 192_000];
/// Ab dieser Abweichung zwischen Uhr und Sample-Zähler wird korrigiert: 2 ms.
pub const DRIFT_TOLERANCE: i64 = SAMPLE_RATE as i64 * 2 / 1000;
/// Ab dieser Abweichung ist es keine Drift mehr (die wächst nur um Bruchteile eines Samples pro Paket),
/// sondern eine Lücke oder ein Sprung: hart neu ausrichten statt Sample für Sample. 4 ms.
pub const RESYNC: i64 = 2 * DRIFT_TOLERANCE;

/// Welcher Ton des Systems aufgenommen wird.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SystemAudio {
    /// Alles, was über den Standard-Ausgang läuft.
    All,
    /// Alles außer dem einbettenden Programm (z. B. ClipHub): dessen eigene Töne landen nie im Clip.
    #[default]
    AllExceptHost,
    /// Nur dieses Programm samt seinen Kindprozessen, z. B. das Spiel (`{"process": 1234}`).
    Process(u32),
    Off,
}

/// Welches Mikrofon aufgenommen wird: `"off"`, `"default"` oder die ID eines Geräts (`audio-devices`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Microphone {
    #[default]
    Off,
    Default,
    Device(String),
}

impl Serialize for Microphone {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            Microphone::Off => "off",
            Microphone::Default => "default",
            Microphone::Device(id) => id,
        })
    }
}

impl<'de> Deserialize<'de> for Microphone {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = String::deserialize(d)?;
        Ok(match id.as_str() {
            "off" | "" => Microphone::Off,
            "default" => Microphone::Default,
            _ => Microphone::Device(id),
        })
    }
}

/// Welche Spuren der Clip bekommt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackLayout {
    /// Eine Spur mit allem gemischt (Browser spielen nur die erste Spur).
    #[default]
    Mixed,
    /// Je Quelle eine Spur, für den Videoschnitt.
    Separate,
    /// Gemischt als Spur 1, dazu je Quelle eine eigene Spur.
    Both,
}

/// Inhalt einer Tonspur.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Mixed,
    System,
    Microphone,
}

impl TrackKind {
    /// Name der Spur in der MP4-Datei (Schnittprogramme zeigen ihn an).
    pub fn label(self) -> &'static str {
        match self {
            TrackKind::Mixed => "VeloFrame Mix",
            TrackKind::System => "VeloFrame System",
            TrackKind::Microphone => "VeloFrame Microphone",
        }
    }

    /// Welche Quellen in die Spur gehen.
    pub fn includes(self, source: SourceKind) -> bool {
        match self {
            TrackKind::Mixed => true,
            TrackKind::System => source == SourceKind::System,
            TrackKind::Microphone => source == SourceKind::Microphone,
        }
    }
}

/// Eine Tonquelle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    #[default]
    System,
    Microphone,
}

/// Feld `audio` in `buffer-start`. Ohne Angabe: alles außer dem Host, kein Mikrofon, eine Spur.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioRequest {
    #[serde(default)]
    pub system: SystemAudio,
    #[serde(default)]
    pub microphone: Microphone,
    #[serde(default)]
    pub tracks: TrackLayout,
    /// Bitrate je Spur in bit/s; ohne Angabe wählt der Modus (Performance 128 000, Long 160 000).
    #[serde(default)]
    pub bitrate: Option<u32>,
    /// Prozess, dessen Ton bei `all-except-host` fehlt. Ohne Angabe: das Programm, das die Engine
    /// eingebettet bzw. `veloframe.exe` gestartet hat.
    #[serde(default)]
    pub host_process: Option<u32>,
}

/// Was die Ton-Aufnahme konkret tut. Aus Anfrage + Modus berechnet.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioPlan {
    pub system: SystemAudio,
    pub microphone: Microphone,
    /// Spuren in der Reihenfolge in der Datei; die erste spielen Browser und Player ab.
    pub tracks: Vec<TrackKind>,
    /// Bitrate je Spur (bit/s).
    pub bitrate: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub host_process: Option<u32>,
}

impl AudioPlan {
    pub fn sources(&self) -> Vec<SourceKind> {
        let mut sources = Vec::new();
        if self.system != SystemAudio::Off {
            sources.push(SourceKind::System);
        }
        if self.microphone != Microphone::Off {
            sources.push(SourceKind::Microphone);
        }
        sources
    }
}

/// Berechnet den Ton-Plan. `None`, wenn keine Quelle eingeschaltet ist.
pub fn plan(request: &AudioRequest, mode: ClipMode) -> crate::Result<Option<AudioPlan>> {
    let system = request.system != SystemAudio::Off;
    let microphone = request.microphone != Microphone::Off;
    if let SystemAudio::Process(0) = request.system {
        return Err(crate::Error::InvalidRequest("Die Prozess-ID für den Ton fehlt.".into()));
    }
    let tracks = match (system, microphone, request.tracks) {
        (false, false, _) => return Ok(None),
        (true, false, _) => vec![TrackKind::System],
        (false, true, _) => vec![TrackKind::Microphone],
        (true, true, TrackLayout::Mixed) => vec![TrackKind::Mixed],
        (true, true, TrackLayout::Separate) => vec![TrackKind::System, TrackKind::Microphone],
        (true, true, TrackLayout::Both) => vec![TrackKind::Mixed, TrackKind::System, TrackKind::Microphone],
    };
    let bitrate = match request.bitrate {
        None => match mode {
            ClipMode::Performance => 128_000,
            ClipMode::Long => 160_000,
        },
        Some(b) if !(64_000..=320_000).contains(&b) => {
            return Err(crate::Error::InvalidRequest("Die Ton-Bitrate muss zwischen 64 000 und 320 000 bit/s liegen.".into()));
        }
        // Der AAC-Encoder kennt nur feste Stufen: die nächste nehmen.
        Some(b) => *BITRATES.iter().min_by_key(|&&s| s.abs_diff(b)).unwrap(),
    };
    Ok(Some(AudioPlan {
        system: request.system,
        microphone: request.microphone.clone(),
        tracks,
        bitrate,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS as u16,
        host_process: request.host_process,
    }))
}

/// AudioSpecificConfig für AAC-LC (steht in der MP4-Datei im `esds`).
pub fn aac_lc_config(sample_rate: u32, channels: u16) -> [u8; 2] {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let index = RATES.iter().position(|&r| r == sample_rate).unwrap_or(3) as u16;
    // 5 Bit Objekttyp (2 = LC), 4 Bit Ratenindex, 4 Bit Kanäle, 3 Bit 0.
    let bits = (2u16 << 11) | (index << 7) | ((channels & 0xF) << 3);
    bits.to_be_bytes()
}

/// Zeit (100 ns) des Samples `position` auf der Ton-Zeitachse.
pub fn ticks_of(position: i64) -> i64 {
    (position as i128 * TICKS_PER_SECOND as i128).div_euclid(SAMPLE_RATE as i128) as i64
}

/// Sample, das zum Zeitpunkt `ticks` gehört (gerundet).
pub fn position_of(ticks: i64) -> i64 {
    (ticks as i128 * SAMPLE_RATE as i128 + TICKS_PER_SECOND as i128 / 2).div_euclid(TICKS_PER_SECOND as i128) as i64
}

/// Welche Tonpakete ein Clip braucht, der von `start` bis `end` (100 ns) zu sehen ist: ab dem ersten
/// Paket, das bei oder nach `start` beginnt, bis zum letzten Paket vor `end`. Bricht der Strom ab
/// (Lücke), zählt nur der Teil vor der Lücke.
///
/// Bewusst **ohne Vorlauf davor**: Der Ton beginnt höchstens ein AAC-Frame (21 ms) nach dem ersten
/// Bild und wird per leerer Bearbeitung genau an seine Zeit geschoben. Ein Vorlauf müsste über
/// `media_time` übersprungen werden, und den legt Media Foundation (Windows-Player) mit falschem
/// Vorzeichen aus: Der Ton käme dort um die doppelte Vorlaufzeit zu spät. Gemessen mit
/// `veloframe-bench check`; leere Bearbeitungen wertet Windows richtig aus.
pub fn clip_range(infos: &[PacketInfo], start: i64, end: i64) -> std::ops::Range<usize> {
    let from = infos.partition_point(|p| p.pts < start);
    let mut to = infos.partition_point(|p| p.pts < end).max(from);
    for i in from + 1..to {
        let previous = &infos[i - 1];
        if (infos[i].pts - (previous.pts + previous.duration)).abs() > previous.duration / 2 {
            to = i;
            break;
        }
    }
    from..to
}

/// Puffer einer Quelle auf der gemeinsamen Zeitachse: ein Ring aus Stereo-Frames (f32), adressiert
/// über die absolute Sample-Position.
pub struct SourceBuffer {
    ring: Vec<f32>,
    capacity: i64,
    /// Position nach dem zuletzt geschriebenen Frame.
    next: i64,
    /// Davor ist Stille.
    valid_from: i64,
    started: bool,
    resync: bool,
    pub stats: SourceStats,
}

/// Zähler einer Quelle, seit dem Start.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SourceStats {
    pub frames: u64,
    /// Mit Stille gefüllt, weil die Quelle nichts geliefert hat (normal bei Loopback ohne Ton).
    pub silence_frames: u64,
    /// Kam zu spät und wurde verworfen.
    pub late_frames: u64,
    /// Eingefügte oder weggelassene Samples gegen die Drift.
    pub drift_corrections: u64,
    /// Sprünge der Uhr rückwärts (neu ausgerichtet).
    pub resyncs: u64,
    /// Abweichung zwischen Uhr und Sample-Zähler beim letzten Paket (Frames, positiv = Quelle zu langsam).
    pub deviation: i64,
    /// Höchster Pegel (0–1) seit dem letzten [`SourceBuffer::take_peak`].
    pub peak: f32,
}

impl SourceBuffer {
    pub fn new(capacity_frames: usize) -> Self {
        Self {
            ring: vec![0.0; capacity_frames.max(1) * CHANNELS],
            capacity: capacity_frames.max(1) as i64,
            next: 0,
            valid_from: 0,
            started: false,
            resync: false,
            stats: SourceStats::default(),
        }
    }

    /// Nach einem Gerätewechsel: Das nächste Paket wird neu an seiner Uhrzeit ausgerichtet.
    pub fn reset(&mut self) {
        self.resync = true;
    }

    /// Position nach dem letzten Frame (oder `None`, wenn noch nichts kam).
    pub fn end(&self) -> Option<i64> {
        self.started.then_some(self.next)
    }

    pub fn take_peak(&mut self) -> f32 {
        std::mem::take(&mut self.stats.peak)
    }

    fn zero(&mut self, from: i64, to: i64) {
        let (from, to) = (from.max(to - self.capacity), to);
        for pos in from..to {
            let at = pos.rem_euclid(self.capacity) as usize * CHANNELS;
            self.ring[at..at + CHANNELS].fill(0.0);
        }
    }

    fn copy_in(&mut self, at: i64, frames: &[f32]) {
        let count = (frames.len() / CHANNELS) as i64;
        let start = at.rem_euclid(self.capacity);
        let first = (self.capacity - start).min(count);
        let (s, f) = (start as usize * CHANNELS, first as usize * CHANNELS);
        self.ring[s..s + f].copy_from_slice(&frames[..f]);
        if first < count {
            let rest = frames.len() - f;
            self.ring[..rest].copy_from_slice(&frames[f..]);
        }
    }

    /// Legt `frames` (Stereo, abwechselnd links/rechts) ab. `clock`: Position des ersten Frames laut
    /// Uhr, `None` wenn Windows die Uhrzeit als ungültig meldet (dann lückenlos anhängen).
    /// `read_to`: bis hierhin hat der Mischer schon gelesen; was davor landen würde, kam zu spät.
    pub fn write(&mut self, clock: Option<i64>, frames: &[f32], read_to: i64) {
        let count = (frames.len() / CHANNELS) as i64;
        if count == 0 {
            return;
        }
        let frames = &frames[..count as usize * CHANNELS];
        let clock = clock.or(self.started.then_some(self.next));
        let mut frames = frames;
        let mut repeat_first = false;
        let at = match clock {
            Some(clock) if !self.started || self.resync || (clock - self.next).abs() > RESYNC => {
                if self.started && clock > self.next && clock - self.next < self.capacity {
                    // Die Quelle hat eine Weile nichts geliefert: Lücke mit Stille füllen.
                    self.zero(self.next, clock);
                    self.stats.silence_frames += (clock - self.next) as u64;
                } else if self.started && clock < self.next {
                    self.stats.resyncs += 1;
                } else {
                    self.valid_from = clock;
                }
                self.started = true;
                self.resync = false;
                self.stats.deviation = 0;
                clock
            }
            Some(clock) => {
                let deviation = clock - self.next;
                self.stats.deviation = deviation;
                if deviation > DRIFT_TOLERANCE {
                    repeat_first = true; // Quelle zu langsam: ein Sample doppelt
                    self.stats.drift_corrections += 1;
                } else if deviation < -DRIFT_TOLERANCE && count > 1 {
                    frames = &frames[CHANNELS..]; // Quelle zu schnell: ein Sample weglassen
                    self.stats.drift_corrections += 1;
                }
                self.next
            }
            None => {
                // Erstes Paket ohne gültige Uhrzeit: dort beginnen, wo der Mischer gerade ist.
                self.started = true;
                self.valid_from = read_to;
                read_to
            }
        };
        self.stats.frames += count as u64;
        let peak = frames.iter().fold(0.0f32, |p, &x| p.max(x.abs()));
        self.stats.peak = self.stats.peak.max(peak);

        let mut pos = at;
        if repeat_first {
            let first = [frames[0], frames[1]];
            if pos >= read_to {
                self.copy_in(pos, &first);
            }
            pos += 1;
        }
        // Zu spät: Der Mischer hat diese Stellen schon (als Stille) verarbeitet.
        let late = (read_to - pos).clamp(0, (frames.len() / CHANNELS) as i64);
        if late > 0 {
            self.stats.late_frames += late as u64;
            frames = &frames[late as usize * CHANNELS..];
            pos += late;
        }
        // Mehr als der Ring fasst, kann nicht gemischt werden: nur das Neueste behalten.
        let room = (frames.len() / CHANNELS) as i64 - self.capacity;
        if room > 0 {
            frames = &frames[room as usize * CHANNELS..];
            pos += room;
        }
        if !frames.is_empty() {
            self.copy_in(pos, frames);
        }
        self.next = pos + (frames.len() / CHANNELS) as i64;
    }

    /// Addiert die Frames ab `from` (mal `gain`) auf `out`. Wo die Quelle nichts hat: Stille.
    pub fn mix_into(&self, from: i64, out: &mut [f32], gain: f32) {
        if !self.started || gain == 0.0 {
            return;
        }
        let count = (out.len() / CHANNELS) as i64;
        let lo = from.max(self.valid_from).max(self.next - self.capacity);
        let hi = (from + count).min(self.next);
        for pos in lo..hi {
            let src = pos.rem_euclid(self.capacity) as usize * CHANNELS;
            let dst = (pos - from) as usize * CHANNELS;
            out[dst] += self.ring[src] * gain;
            out[dst + 1] += self.ring[src + 1] * gain;
        }
    }
}

/// Rechnet gemischte Samples (−1 … 1) in 16 Bit um; was darüber hinausgeht, wird begrenzt.
pub fn to_i16(input: &[f32], out: &mut [i16]) {
    for (o, &x) in out.iter_mut().zip(input) {
        *o = (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    }
}

/// Zustand der Ton-Aufnahme (Teil von `buffer-status`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    pub running: bool,
    pub tracks: Vec<TrackKind>,
    /// Name des AAC-Encoders.
    pub encoder: Option<String>,
    pub bitrate: u32,
    /// Sekunden Ton im Puffer (erste Spur).
    pub buffered_seconds: f64,
    /// Wie alt ein Bild beim Abholen im Mittel ist (ms); um so viel liegt der Ton später, damit
    /// Bild und Ton zusammenpassen.
    pub video_lag_ms: f64,
    pub ram_bytes: u64,
    pub sources: Vec<SourceStatus>,
    pub error: Option<String>,
}

/// Zustand einer Quelle.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub source: SourceKind,
    /// Wie aufgenommen wird: `process-exclude` (alles außer dem Host), `process-include` (ein
    /// Programm), `system-loopback` (alles) oder `device` (Mikrofon).
    pub capture: String,
    pub device: Option<String>,
    /// Ob die Quelle gerade offen ist und Ton liefern kann.
    pub active: bool,
    pub gain: f32,
    pub muted: bool,
    /// Höchster Pegel (0–1) seit der letzten Statusmeldung.
    pub level: f32,
    /// Aussetzer, die Windows gemeldet hat, plus verspätete Pakete.
    pub dropouts: u64,
    /// Millisekunden, die zu spät kamen und fehlen.
    pub late_ms: f64,
    /// Millisekunden, die mit Stille gefüllt wurden (Quelle lieferte nichts, z. B. weil nichts spielte).
    pub silence_ms: f64,
    /// Eingefügte oder weggelassene Samples gegen die Drift der Soundkarten-Uhr.
    pub drift_corrections: u64,
    /// Aktuelle Abweichung zwischen Uhr und Sample-Zähler (ms).
    pub drift_ms: f64,
    pub device_changes: u32,
    /// Um so viel werden die Zeitstempel verschoben (ms): die gemessene Ausgabelatenz bei
    /// System-Loopback, sonst 0.
    pub latency_ms: f64,
    pub warnings: Vec<AudioWarning>,
}

/// Hinweis zu einer Quelle. `code`: `bluetooth-hands-free`, `process-loopback-unavailable`,
/// `no-device`, `device-lost`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AudioWarning {
    pub code: String,
    pub message: String,
}

/// Lautstärke oder Stummschaltung einer Quelle, während der Puffer läuft (`audio-set`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControl {
    /// Verstärkung, 0–4 (1 = unverändert, 4 ≈ +12 dB).
    #[serde(default)]
    pub gain: Option<f32>,
    #[serde(default)]
    pub muted: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSetRequest {
    #[serde(default)]
    pub system: Option<SourceControl>,
    #[serde(default)]
    pub microphone: Option<SourceControl>,
}

impl AudioSetRequest {
    pub fn validate(&self) -> crate::Result<()> {
        for control in [self.system, self.microphone].into_iter().flatten() {
            if let Some(gain) = control.gain {
                if !(0.0..=4.0).contains(&gain) {
                    return Err(crate::Error::InvalidRequest("Die Verstärkung muss zwischen 0 und 4 liegen.".into()));
                }
            }
        }
        Ok(())
    }
}

/// Ein Tongerät (`audio-devices`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    /// `output` (Lautsprecher, Kopfhörer) oder `input` (Mikrofon).
    pub kind: String,
    pub default: bool,
    /// Bluetooth: Das Mikrofon zu öffnen schaltet ein Headset in den Freisprech-Modus (Mono, 16 kHz).
    pub bluetooth: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(values: &[f32]) -> Vec<f32> {
        values.iter().flat_map(|&v| [v, v]).collect()
    }

    fn read(buffer: &SourceBuffer, from: i64, count: usize) -> Vec<f32> {
        let mut out = vec![0.0; count * CHANNELS];
        buffer.mix_into(from, &mut out, 1.0);
        out.iter().step_by(CHANNELS).copied().collect()
    }

    #[test]
    fn reads_requests_and_defaults() {
        let r: AudioRequest =
            serde_json::from_str(r#"{"system":{"process":1234},"microphone":"{0.0.1.00000000}.{abc}","tracks":"both","bitrate":150000}"#).unwrap();
        assert_eq!(r.system, SystemAudio::Process(1234));
        assert_eq!(r.microphone, Microphone::Device("{0.0.1.00000000}.{abc}".into()));
        assert_eq!(r.tracks, TrackLayout::Both);
        let p = plan(&r, ClipMode::Performance).unwrap().unwrap();
        assert_eq!(p.tracks, vec![TrackKind::Mixed, TrackKind::System, TrackKind::Microphone]);
        assert_eq!(p.bitrate, 160_000, "nächste Stufe des Encoders");

        let empty: AudioRequest = serde_json::from_str("{}").unwrap();
        assert_eq!((empty.system, &empty.microphone, empty.tracks), (SystemAudio::AllExceptHost, &Microphone::Off, TrackLayout::Mixed));
        let p = plan(&empty, ClipMode::Long).unwrap().unwrap();
        assert_eq!((p.tracks.as_slice(), p.bitrate), (&[TrackKind::System][..], 160_000));

        let off: AudioRequest = serde_json::from_str(r#"{"system":"off","microphone":"off"}"#).unwrap();
        assert_eq!(plan(&off, ClipMode::Performance).unwrap(), None);
        let all: AudioRequest = serde_json::from_str(r#"{"system":"all","microphone":"default","tracks":"separate"}"#).unwrap();
        assert_eq!(plan(&all, ClipMode::Performance).unwrap().unwrap().tracks, vec![TrackKind::System, TrackKind::Microphone]);
        assert_eq!(serde_json::to_value(&all).unwrap()["microphone"], "default");
        let bad = AudioRequest { bitrate: Some(1_000), ..Default::default() };
        assert_eq!(plan(&bad, ClipMode::Performance).unwrap_err().code(), "invalid-request");
    }

    #[test]
    fn aac_config_for_48k_stereo() {
        assert_eq!(aac_lc_config(48_000, 2), [0x11, 0x90]);
        assert_eq!(aac_lc_config(44_100, 2), [0x12, 0x10]);
    }

    #[test]
    fn clock_conversion_is_exact_over_hours() {
        let three_hours = SAMPLE_RATE as i64 * 3 * 3600;
        assert_eq!(ticks_of(three_hours), 3 * 3600 * TICKS_PER_SECOND);
        assert_eq!(position_of(ticks_of(three_hours + 7)), three_hours + 7);
        assert_eq!(position_of(-ticks_of(5)), -5);
        assert_eq!(ticks_of(1024), 213_333);
    }

    #[test]
    fn packets_land_at_their_clock_time_and_gaps_become_silence() {
        let mut b = SourceBuffer::new(48_000);
        b.write(Some(100), &stereo(&[1.0; 10]), 0);
        // 290 Frames (6 ms) lang nichts (Loopback ohne Ton), dann wieder Ton bei 400.
        b.write(Some(400), &stereo(&[0.5; 10]), 0);
        assert_eq!(b.stats.silence_frames, 290);
        assert_eq!(b.stats.drift_corrections, 0, "eine Lücke ist keine Drift");
        let out = read(&b, 95, 330);
        assert_eq!(&out[..5], &[0.0; 5], "vor dem ersten Paket ist Stille");
        assert_eq!(&out[5..15], &[1.0; 10]);
        assert!(out[15..305].iter().all(|&x| x == 0.0), "Lücke ist Stille");
        assert_eq!(&out[305..315], &[0.5; 10]);
        assert_eq!(&out[315..], &[0.0; 15], "nach dem letzten Paket ist Stille");
    }

    #[test]
    fn small_drift_is_corrected_one_sample_at_a_time() {
        let mut b = SourceBuffer::new(48_000);
        let packet = stereo(&[0.25; 480]); // 10 ms
        // Die Soundkarte ist etwas zu langsam: Die Uhr läuft pro Paket 1 Frame weiter als die Daten.
        let mut clock = 0;
        for _ in 0..200 {
            b.write(Some(clock), &packet, 0);
            clock += 481;
        }
        assert!(b.stats.drift_corrections > 0);
        assert!(b.stats.deviation.abs() <= DRIFT_TOLERANCE + 1, "Abweichung bleibt bei höchstens 2 ms: {}", b.stats.deviation);
        assert_eq!(b.stats.silence_frames, 0, "keine harten Sprünge");
        // Zu schnell: Die Daten laufen der Uhr davon.
        let mut fast = SourceBuffer::new(48_000);
        let mut clock = 0;
        for _ in 0..200 {
            fast.write(Some(clock), &packet, 0);
            clock += 479;
        }
        assert!(fast.stats.drift_corrections > 0);
        assert!(fast.stats.deviation.abs() <= DRIFT_TOLERANCE + 1);
        assert!(fast.end().unwrap() - clock < DRIFT_TOLERANCE + 480);
    }

    #[test]
    fn jitter_below_the_tolerance_changes_nothing() {
        let mut b = SourceBuffer::new(48_000);
        let packet = stereo(&[0.1; 480]);
        for i in 0..100i64 {
            let jitter = [0, 30, -20, 45, -40][i as usize % 5];
            b.write(Some(i * 480 + jitter), &packet, 0);
        }
        assert_eq!(b.stats.drift_corrections, 0);
        assert_eq!(b.end(), Some(100 * 480 + 0), "lückenlos angehängt");
    }

    #[test]
    fn late_data_is_dropped_and_counted() {
        let mut b = SourceBuffer::new(48_000);
        b.write(Some(1_000), &stereo(&[1.0; 100]), 1_050);
        assert_eq!(b.stats.late_frames, 50);
        assert_eq!(read(&b, 1_000, 100)[..50], [0.0; 50]);
        assert_eq!(read(&b, 1_050, 50), vec![1.0; 50]);
    }

    #[test]
    fn device_change_realigns_and_fills_the_gap() {
        let mut b = SourceBuffer::new(48_000);
        b.write(Some(0), &stereo(&[1.0; 480]), 0);
        b.reset();
        // Neues Gerät liefert 5 ms später (unter der Schwelle für harte Sprünge), wird aber trotzdem
        // genau an seiner Uhrzeit ausgerichtet.
        b.write(Some(480 + 240), &stereo(&[0.5; 480]), 0);
        assert_eq!(b.stats.drift_corrections, 0);
        let out = read(&b, 0, 1200);
        assert!(out[480..720].iter().all(|&x| x == 0.0));
        assert_eq!(out[720], 0.5);
    }

    #[test]
    fn mixing_applies_gain_and_clips() {
        let mut game = SourceBuffer::new(1_000);
        let mut mic = SourceBuffer::new(1_000);
        game.write(Some(0), &stereo(&[0.8; 10]), 0);
        mic.write(Some(5), &stereo(&[0.5; 10]), 0);
        let mut out = vec![0.0; 20 * CHANNELS];
        game.mix_into(0, &mut out, 1.0);
        mic.mix_into(0, &mut out, 0.5);
        let mut pcm = vec![0i16; out.len()];
        to_i16(&out, &mut pcm);
        assert_eq!(pcm[0], (0.8f32 * 32767.0).round() as i16);
        assert_eq!(pcm[5 * CHANNELS], (1.05f32.min(1.0) * 32767.0) as i16, "Summe über 1 wird begrenzt");
        assert_eq!(pcm[12 * CHANNELS], (0.25f32 * 32767.0).round() as i16);
        assert_eq!(pcm[19 * CHANNELS], 0);
    }

    #[test]
    fn ring_wraps_and_forgets_what_it_overwrote() {
        let mut b = SourceBuffer::new(100);
        for i in 0..5 {
            b.write(Some(i * 60), &stereo(&[i as f32; 60]), 0);
        }
        // Nur die letzten 100 Frames (200..300) sind noch da.
        let out = read(&b, 150, 150);
        assert!(out[..50].iter().all(|&x| x == 0.0));
        assert_eq!(out[50..60], [3.0; 10]);
        assert_eq!(out[90..150], [4.0; 60]);
    }

    #[test]
    fn clip_range_starts_with_the_first_frame_inside_the_video() {
        let d = ticks_of(1024);
        let infos: Vec<PacketInfo> = (0..100).map(|i| PacketInfo { pts: ticks_of(i * 1024), dts: ticks_of(i * 1024), duration: d, key: true }).collect();
        let start = ticks_of(10 * 1024 + 500);
        let end = ticks_of(50 * 1024);
        let range = clip_range(&infos, start, end);
        assert_eq!(range, 11..50, "Paket 11 ist das erste, das im Video beginnt");
        assert_eq!(clip_range(&infos, ticks_of(10 * 1024), end).start, 10, "genau auf dem Bild zählt mit");
        assert_eq!(clip_range(&infos, -1_000, ticks_of(3 * 1024)), 0..3);
        let mut gap = infos.clone();
        for p in &mut gap[30..] {
            p.pts += TICKS_PER_SECOND;
        }
        assert_eq!(clip_range(&gap, start, end + TICKS_PER_SECOND).end, 30);
    }
}
